//! The `rebucket-<data type>` migrations: move every row of a data type to the bucket the
//! configured partition size gives, then rebuild its census in Redis
//!
//! A row is classified by whether its bucket is the one the configured size gives for its
//! timestamp, so a half-done move is safe to resume. A move is an idempotent insert of the new row
//! followed by a delete of the old one, two plain statements. The census is rebuilt in two
//! resumable phases once every row has moved: the census keys are found with `SCAN` and deleted,
//! then the tables are scanned by token and each partition's bucket is added to its census key with
//! `ZADD`, which is idempotent. The census phases checkpoint in Redis, so a run stopped in either
//! phase resumes from where it was. The record of the partition size is written last.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use scylla::client::session::Session;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::{CqlTimestamp, CqlValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, RwLock};

use super::pass_store::{backend_error, is_rejection};
use super::scylla::ThoriumKeyspace;
use super::{
    Batch, Bucket, ColumnRole, Keyspace, MigrateError, MigrationRunner, ObjectDef, Outcome,
    RunContext, RunMode, ScyllaScan, Slice, ring_slices,
};
use crate::models::upgrades::{IssueCategory, IssueReason, Record};

/// The statements the moves have prepared, by statement text, shared by every worker of the process
///
/// The driver prepares a statement again on every call that binds values to its text, which sends
/// a prepare request beside each execute; a move holds two statements per table, so preparing
/// each once keeps the requests per moved row at two.
static PREPARED: LazyLock<RwLock<HashMap<String, PreparedStatement>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// How many distinct statements the moves have prepared on a keyspace so far
///
/// # Arguments
///
/// * `keyspace` - The keyspace the statements name
#[must_use]
pub fn prepared_statements(keyspace: &str) -> usize {
    // the cache holds one entry per statement text, and a statement names its keyspace
    let marker = format!(" {keyspace}.");
    PREPARED.read().map_or(0, |prepared| {
        prepared
            .keys()
            .filter(|text| text.contains(&marker))
            .count()
    })
}

/// Returns a statement prepared on the session, preparing it only the first time its text is asked for
///
/// # Arguments
///
/// * `session` - The Scylla session to prepare it on
/// * `text` - The statement's CQL
async fn prepared(session: &Session, text: String) -> Result<PreparedStatement, MigrateError> {
    // a statement an earlier row of this process prepared
    if let Some(found) = PREPARED
        .read()
        .ok()
        .and_then(|prepared| prepared.get(&text).cloned())
    {
        return Ok(found);
    }
    // prepare it, naming the statement that fails, and remember it for the next row
    let statement = session
        .prepare(text.clone())
        .await
        .map_err(|err| backend_error("prepare a moved row's statement", &err, false))?;
    if let Ok(mut prepared) = PREPARED.write() {
        prepared.insert(text, statement.clone());
    }
    Ok(statement)
}

/// The column a converted record carries its old bucket in, which `write` removes before it
/// inserts the new row and uses to delete the old one
const OLD_BUCKET: &str = "__old_bucket";

/// A census family: a sorted set of bucket numbers per group, year, and more
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CensusFamily {
    /// `{ns}:census:samples:stream:{group}:{year}`
    Samples,
    /// `{ns}:census:repos:stream:{group}:{year}`
    Repos,
    /// `{ns}:census:commitish:stream:{kind}:{group}:{repo}:{year}`
    Commitish,
    /// `{ns}:census:entities:stream:{group}:{year}`
    Entities,
    /// `{ns}:census:associations:stream:{group}:{source}:{year}`, one key for the source and one
    /// for the target of each row
    Associations,
    /// `{ns}:census:tags:stream:{kind}:{group}:{key}:{value}:{year}`
    Tags,
    /// `{ns}:census:tags_case_insensitive:stream:{kind}:{group}:{lower key}:{lower value}:{year}`
    TagsCaseInsensitive,
}

impl CensusFamily {
    /// The family's name in its keys
    const fn name(self) -> &'static str {
        // each family's segment after `census:`
        match self {
            CensusFamily::Samples => "samples",
            CensusFamily::Repos => "repos",
            CensusFamily::Commitish => "commitish",
            CensusFamily::Entities => "entities",
            CensusFamily::Associations => "associations",
            CensusFamily::Tags => "tags",
            CensusFamily::TagsCaseInsensitive => "tags_case_insensitive",
        }
    }
}

/// One table a data type's rows live in
#[derive(Debug, Clone, Copy)]
pub struct RebucketTable {
    /// The table's name
    pub table: &'static str,
    /// The timestamp column the bucket is computed from
    pub timestamp: &'static str,
    /// The census families this table's partitions are counted in
    pub census: &'static [CensusFamily],
}

/// A `rebucket-<data type>` migration
#[derive(Debug, Clone, Copy)]
pub struct Rebucket {
    /// The data type, as its partition size record names it (`files`, `repos`, `results`,
    /// `entities`, `associations`, or `tags`)
    pub data_type: &'static str,
    /// The tables whose rows move
    pub tables: &'static [RebucketTable],
}

/// Files: the samples list
pub const FILES: Rebucket = Rebucket {
    data_type: "files",
    tables: &[RebucketTable {
        table: "samples_list",
        timestamp: "uploaded",
        census: &[CensusFamily::Samples],
    }],
};

/// Repos: the repos list and the commitish list
pub const REPOS: Rebucket = Rebucket {
    data_type: "repos",
    tables: &[
        RebucketTable {
            table: "repos_list",
            timestamp: "uploaded",
            census: &[CensusFamily::Repos],
        },
        RebucketTable {
            table: "commitish_list",
            timestamp: "timestamp",
            census: &[CensusFamily::Commitish],
        },
    ],
};

/// Results: the results stream, which has no census
pub const RESULTS: Rebucket = Rebucket {
    data_type: "results",
    tables: &[RebucketTable {
        table: "results_stream",
        timestamp: "uploaded",
        census: &[],
    }],
};

/// Entities
pub const ENTITIES: Rebucket = Rebucket {
    data_type: "entities",
    tables: &[RebucketTable {
        table: "entities",
        timestamp: "created",
        census: &[CensusFamily::Entities],
    }],
};

/// Associations, whose census covers both the source and the target of each row
pub const ASSOCIATIONS: Rebucket = Rebucket {
    data_type: "associations",
    tables: &[RebucketTable {
        table: "associations",
        timestamp: "created",
        census: &[CensusFamily::Associations],
    }],
};

/// Tags of every tag type, each with its own partition size
pub const TAGS: Rebucket = Rebucket {
    data_type: "tags",
    tables: &[RebucketTable {
        table: "tags",
        timestamp: "uploaded",
        census: &[CensusFamily::Tags, CensusFamily::TagsCaseInsensitive],
    }],
};

/// Where a census rebuild is, kept in Redis with the attempt that made it so a stopped run resumes
/// and a different attempt starts over
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum CensusState {
    /// Nothing has been done yet
    #[default]
    Start,
    /// Deleting the census keys: the family being scanned and its `SCAN` cursor
    Delete {
        /// The index of the family in this migration's census list
        family: usize,
        /// The cursor to continue the scan from
        cursor: u64,
    },
    /// Adding buckets: the table being scanned and the token of the last partition done
    Add {
        /// The index of the table
        table: usize,
        /// The token of the last partition done, or none at the table's start
        token: Option<i64>,
    },
    /// The census is rebuilt; only the record of the sizes is left
    Done,
}

/// A table's declared columns, its partition key columns, and its clustering columns
type TableColumns = (Vec<&'static str>, Vec<&'static str>, Vec<&'static str>);

/// Get a table's declared columns and its key columns, partition first
///
/// # Arguments
///
/// * `table` - The table's name
fn table_columns(table: &str) -> Result<TableColumns, MigrateError> {
    // find the table among the declared objects
    let object = <ThoriumKeyspace as Keyspace>::OBJECTS
        .iter()
        .find(|object| matches!(object, ObjectDef::Table(_)) && object.name() == table)
        .ok_or_else(|| MigrateError::Systematic(format!("{table} isn't a declared table")))?;
    let columns = object.columns();
    // the key columns in key order
    let mut partition = columns
        .iter()
        .filter_map(|column| match column.role {
            ColumnRole::Partition(at) => Some((at, column.name)),
            _ => None,
        })
        .collect::<Vec<_>>();
    partition.sort_unstable();
    let mut cluster = columns
        .iter()
        .filter_map(|column| match column.role {
            ColumnRole::Cluster(at, _) => Some((at, column.name)),
            _ => None,
        })
        .collect::<Vec<_>>();
    cluster.sort_unstable();
    Ok((
        columns.iter().map(|column| column.name).collect(),
        partition.into_iter().map(|(_, name)| name).collect(),
        cluster.into_iter().map(|(_, name)| name).collect(),
    ))
}

/// A column's text, from a record
///
/// # Arguments
///
/// * `record` - The record
/// * `name` - The column
fn text_of(record: &Record, name: &str) -> Option<String> {
    // text and ascii columns hold text
    record.text(name)
}

/// A column's integer, from a record
///
/// # Arguments
///
/// * `record` - The record
/// * `name` - The column
fn int_of(record: &Record, name: &str) -> Option<i32> {
    match record {
        Record::Scylla { columns, .. } => match columns.get(name) {
            Some(Some(CqlValue::Int(value))) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

/// A column's timestamp, from a record
///
/// # Arguments
///
/// * `record` - The record
/// * `name` - The column
fn time_of(record: &Record, name: &str) -> Option<DateTime<Utc>> {
    match record {
        Record::Scylla { columns, .. } => match columns.get(name) {
            Some(Some(CqlValue::Timestamp(CqlTimestamp(millis)))) => {
                DateTime::from_timestamp_millis(*millis)
            }
            _ => None,
        },
        _ => None,
    }
}

impl Rebucket {
    /// The pass key and migration id of this data type's migration
    #[must_use]
    pub fn id(&self) -> String {
        // `rebucket-files`, `rebucket-tags`, and so on
        format!("rebucket-{}", self.data_type)
    }

    /// The Redis key a census rebuild's progress is kept in
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace
    #[must_use]
    pub fn state_key(&self, ns: &str) -> String {
        // namespaced like every other Thorium Redis key
        format!("{ns}:rebucket:{}:census", self.data_type)
    }

    /// The recorded-setting ids and sizes this migration records when it finishes
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context, holding the configured sizes
    fn recorded(&self, ctx: &RunContext) -> Vec<(String, u16)> {
        // tags record one size per tag type, every other data type one
        let sizes = &ctx.backends.partition_sizes;
        sizes
            .iter()
            .filter(|(name, _)| {
                if self.data_type == "tags" {
                    name.starts_with("tags.")
                } else {
                    name.as_str() == self.data_type
                }
            })
            .map(|(name, size)| (format!("partition_size.{name}"), *size))
            .collect()
    }

    /// The partition size a row's bucket is counted in
    ///
    /// # Arguments
    ///
    /// * `record` - The row
    /// * `ctx` - The run's context
    fn size_of(&self, record: &Record, ctx: &RunContext) -> Result<u16, MigrateError> {
        // tags look their size up by the row's tag type
        let name = if self.data_type == "tags" {
            let kind = text_of(record, "type").unwrap_or_default().to_lowercase();
            format!("tags.{kind}")
        } else {
            self.data_type.to_owned()
        };
        ctx.backends
            .partition_sizes
            .get(&name)
            .copied()
            .ok_or_else(|| {
                MigrateError::Systematic(format!("no partition size is configured for {name}"))
            })
    }

    /// The scan of one table's rows
    ///
    /// # Arguments
    ///
    /// * `table` - The table's index
    /// * `ctx` - The run's context
    /// * `columns` - The columns to read, or every declared column
    fn scan(
        &self,
        table: usize,
        ctx: &RunContext,
        columns: Option<&[&str]>,
    ) -> Result<ScyllaScan, MigrateError> {
        // the table's declared columns and key
        let spec = self
            .tables
            .get(table)
            .ok_or_else(|| MigrateError::Systematic("a slice named no table".to_owned()))?;
        let (all, partition, cluster) = table_columns(spec.table)?;
        Ok(ScyllaScan {
            keyspace: ctx.backends.namespace.clone(),
            table: spec.table.to_owned(),
            partition_key: partition.iter().map(|name| (*name).to_owned()).collect(),
            columns: columns.map_or_else(
                || all.iter().map(|name| (*name).to_owned()).collect(),
                |columns| columns.iter().map(|name| (*name).to_owned()).collect(),
            ),
            key_columns: partition
                .iter()
                .chain(&cluster)
                .map(|name| (*name).to_owned())
                .collect(),
            cuts: Vec::new(),
        })
    }

    /// The census keys and bucket one row adds to
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace
    /// * `family` - The census family
    /// * `record` - The row
    fn census_keys(ns: &str, family: CensusFamily, record: &Record) -> Vec<(String, i32)> {
        // the pieces every family's key is built from
        let (Some(year), Some(bucket)) = (int_of(record, "year"), int_of(record, "bucket")) else {
            return Vec::new();
        };
        let group = text_of(record, "group").unwrap_or_default();
        let keys = match family {
            CensusFamily::Samples => vec![format!("{ns}:census:samples:stream:{group}:{year}")],
            CensusFamily::Repos => vec![format!("{ns}:census:repos:stream:{group}:{year}")],
            CensusFamily::Entities => vec![format!("{ns}:census:entities:stream:{group}:{year}")],
            CensusFamily::Commitish => {
                let kind = text_of(record, "kind").unwrap_or_default();
                let repo = text_of(record, "repo").unwrap_or_default();
                vec![format!(
                    "{ns}:census:commitish:stream:{kind}:{group}:{repo}:{year}"
                )]
            }
            CensusFamily::Associations => ["source", "target"]
                .into_iter()
                .map(|side| {
                    let end = text_of(record, side).unwrap_or_default();
                    format!("{ns}:census:associations:stream:{group}:{end}:{year}")
                })
                .collect(),
            CensusFamily::Tags | CensusFamily::TagsCaseInsensitive => {
                let kind = text_of(record, "type").unwrap_or_default();
                let key = text_of(record, "key").unwrap_or_default();
                let value = text_of(record, "value").unwrap_or_default();
                if family == CensusFamily::Tags {
                    vec![format!(
                        "{ns}:census:tags:stream:{kind}:{group}:{key}:{value}:{year}"
                    )]
                } else {
                    vec![format!(
                        "{ns}:census:tags_case_insensitive:stream:{kind}:{group}:{}:{}:{year}",
                        key.to_lowercase(),
                        value.to_lowercase()
                    )]
                }
            }
        };
        keys.into_iter().map(|key| (key, bucket)).collect()
    }

    /// Read the census rebuild's progress
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn load_state(&self, ctx: &RunContext) -> Result<CensusState, MigrateError> {
        // a missing key is a rebuild that hasn't started
        let mut conn = ctx
            .backends
            .redis
            .get()
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        let raw: Option<String> = redis::cmd("GET")
            .arg(self.state_key(&ctx.backends.namespace))
            .query_async(&mut *conn)
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        // progress another attempt left is ignored, so an abandoned run never decides a later one
        let saved: Option<serde_json::Value> = raw.and_then(|raw| serde_json::from_str(&raw).ok());
        let attempt = ctx.attempt.as_deref().unwrap_or_default();
        Ok(saved
            .filter(|saved| saved["attempt"].as_str() == Some(attempt))
            .and_then(|saved| serde_json::from_value(saved["progress"].clone()).ok())
            .unwrap_or_default())
    }

    /// Save the census rebuild's progress
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    /// * `state` - Where the rebuild is
    async fn save_state(&self, ctx: &RunContext, state: &CensusState) -> Result<(), MigrateError> {
        // one plain string, overwritten at each checkpoint
        let mut conn = ctx
            .backends
            .redis
            .get()
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        redis::cmd("SET")
            .arg(self.state_key(&ctx.backends.namespace))
            .arg(
                serde_json::json!({
                    "attempt": ctx.attempt.as_deref().unwrap_or_default(),
                    "progress": state,
                })
                .to_string(),
            )
            .query_async::<()>(&mut *conn)
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))
    }

    /// Delete the census keys of every family this migration rebuilds, scanning from a cursor
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    /// * `batches` - How many `SCAN` steps are left to this call, when bounded
    /// * `family` - The family to start at
    /// * `cursor` - The cursor to start at
    async fn delete_census(
        &self,
        ctx: &RunContext,
        batches: &mut Option<usize>,
        family: usize,
        cursor: u64,
    ) -> Result<(), MigrateError> {
        let ns = &ctx.backends.namespace;
        // every family, in the order the tables list them
        let families = self.families();
        let mut cursor = cursor;
        for (at, census) in families.iter().enumerate().skip(family) {
            let pattern = format!("{ns}:census:{}:stream:*", census.name());
            loop {
                if ctx.cancel.is_cancelled() {
                    return Err(MigrateError::Cancelled);
                }
                if let Some(left) = batches {
                    if *left == 0 {
                        return Err(MigrateError::Cancelled);
                    }
                    *left -= 1;
                }
                // one step of the scan, and its keys deleted
                let mut conn = ctx
                    .backends
                    .redis
                    .get()
                    .await
                    .map_err(|err| MigrateError::Backend(err.to_string()))?;
                let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(&pattern)
                    .arg("COUNT")
                    .arg(ctx.limits.scan_count)
                    .query_async(&mut *conn)
                    .await
                    .map_err(|err| MigrateError::Backend(err.to_string()))?;
                if !keys.is_empty() {
                    redis::cmd("UNLINK")
                        .arg(&keys)
                        .query_async::<()>(&mut *conn)
                        .await
                        .map_err(|err| MigrateError::Backend(err.to_string()))?;
                }
                drop(conn);
                cursor = next;
                // checkpoint where the scan is: the next family when this one is done
                let state = if next == 0 {
                    CensusState::Delete {
                        family: at + 1,
                        cursor: 0,
                    }
                } else {
                    CensusState::Delete { family: at, cursor }
                };
                self.save_state(ctx, &state).await?;
                if next == 0 {
                    break;
                }
            }
            cursor = 0;
        }
        Ok(())
    }

    /// The census families this migration rebuilds, each once
    fn families(&self) -> Vec<CensusFamily> {
        // the tables' families, without repeats
        let mut seen = BTreeSet::new();
        self.tables
            .iter()
            .flat_map(|table| table.census.iter().copied())
            .filter(|family| seen.insert(family.name()))
            .collect()
    }

    /// Add each partition's bucket to its census key, table by table, from a position
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    /// * `batches` - How many batches are left to this call, when bounded
    /// * `table` - The table to start at
    /// * `token` - The token of the last partition done in that table
    async fn add_census(
        &self,
        ctx: &RunContext,
        batches: &mut Option<usize>,
        table: usize,
        token: Option<i64>,
    ) -> Result<(), MigrateError> {
        let ns = &ctx.backends.namespace;
        let mut token = token;
        for (at, spec) in self.tables.iter().enumerate().skip(table) {
            // read only the columns the census keys are built from
            let needed: Vec<&str> = [
                "group", "year", "bucket", "kind", "repo", "source", "target", "type", "key",
                "value",
            ]
            .into_iter()
            .filter(|name| table_columns(spec.table).is_ok_and(|(all, _, _)| all.contains(name)))
            .collect();
            let scan = self.scan(at, ctx, Some(&needed))?;
            let slice = ring_slices(1, &[]).remove(0);
            let mut after = token.map_or(serde_json::Value::Null, |token| serde_json::json!(token));
            loop {
                if ctx.cancel.is_cancelled() {
                    return Err(MigrateError::Cancelled);
                }
                if let Some(left) = batches {
                    if *left == 0 {
                        return Err(MigrateError::Cancelled);
                    }
                    *left -= 1;
                }
                // read a batch of partitions and add each one's bucket to its keys
                let batch = scan.next_batch(&slice, &after, ctx).await?;
                let mut adds: BTreeSet<(String, i32)> = BTreeSet::new();
                for record in &batch.records {
                    for family in spec.census {
                        adds.extend(Self::census_keys(ns, *family, record));
                    }
                }
                if !adds.is_empty() {
                    let mut pipe = redis::pipe();
                    for (key, bucket) in &adds {
                        pipe.zadd(key, bucket, f64::from(*bucket)).ignore();
                    }
                    let mut conn = ctx
                        .backends
                        .redis
                        .get()
                        .await
                        .map_err(|err| MigrateError::Backend(err.to_string()))?;
                    pipe.query_async::<()>(&mut *conn)
                        .await
                        .map_err(|err| MigrateError::Backend(err.to_string()))?;
                }
                after = batch.next.clone();
                let done = batch.done;
                // checkpoint the token, or the start of the next table
                let state = if done {
                    CensusState::Add {
                        table: at + 1,
                        token: None,
                    }
                } else {
                    CensusState::Add {
                        table: at,
                        token: after.as_i64(),
                    }
                };
                self.save_state(ctx, &state).await?;
                if done {
                    break;
                }
            }
            token = None;
        }
        Ok(())
    }

    /// Record the sizes this migration moved rows to
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn record_sizes(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // one config row per recorded size, the way `schema apply` records them
        let ns = &ctx.backends.namespace;
        for (id, size) in self.recorded(ctx) {
            ctx.backends
                .scylla
                .query_unpaged(
                    format!(
                        "INSERT INTO {ns}.schema_migrations (kind, id, value, updated) \
                         VALUES ('config', ?, ?, ?)"
                    ),
                    (id, size.to_string(), Utc::now()),
                )
                .await
                .map_err(|err| {
                    backend_error("record a partition size", &err, is_rejection(&err))
                })?;
        }
        Ok(())
    }
}

#[async_trait]
impl MigrationRunner for Rebucket {
    /// Split each table's ring into `n` slices, the tables one after another
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make of each table
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // a slice names its table in its bounds
        let _ = ctx;
        let ring = ring_slices(n, &[]);
        let mut slices = Vec::new();
        for table in 0..self.tables.len() {
            for part in &ring {
                slices.push(Slice {
                    index: u32::try_from(slices.len()).unwrap_or(u32::MAX),
                    start: serde_json::json!([table, part.start]),
                    end: serde_json::json!([table, part.end]),
                });
            }
        }
        Ok(slices)
    }

    /// Read the next batch of a slice
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last token done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // split the table out of the bounds and scan that table's slice
        let table = slice.start[0]
            .as_u64()
            .and_then(|table| usize::try_from(table).ok())
            .ok_or_else(|| MigrateError::Systematic("a slice named no table".to_owned()))?;
        let plain = Slice {
            index: slice.index,
            start: slice.start[1].clone(),
            end: slice.end[1].clone(),
        };
        self.scan(table, ctx, None)?
            .next_batch(&plain, after, ctx)
            .await
    }

    /// Classify a row by whether its bucket is the one the configured size gives
    ///
    /// # Arguments
    ///
    /// * `record` - The row
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // the pieces of the row the bucket is computed from
        let _ = mode;
        let size = self.size_of(record, ctx)?;
        let table = self
            .tables
            .iter()
            .find(|table| !table.timestamp.is_empty() && record_has(record, table.timestamp))
            .ok_or_else(|| MigrateError::Systematic("a row had no timestamp".to_owned()))?;
        let issue = |reason: IssueReason| {
            Ok(Outcome::Issue(super::IssueReport {
                key: super::record_key(record).to_owned(),
                transform: self.id(),
                class: super::ObjectClass::Unrecognized,
                reason,
            }))
        };
        let (Some(year), Some(stored), Some(at)) = (
            int_of(record, "year"),
            int_of(record, "bucket"),
            time_of(record, table.timestamp),
        ) else {
            return issue(IssueReason::new(IssueCategory::MissingField, "bucket"));
        };
        // the bucket the configured size gives
        let Ok(expected) = Bucket::of(at, year, size) else {
            return issue(IssueReason::new(IssueCategory::InvalidValue, "bucket"));
        };
        let expected = i32::try_from(expected.get()).unwrap_or(i32::MAX);
        if expected == stored {
            return Ok(Outcome::Current);
        }
        // the new row, carrying the old bucket so the write can delete the old row
        let Record::Scylla { key, columns } = record else {
            return Ok(Outcome::Current);
        };
        let mut columns: BTreeMap<String, Option<CqlValue>> = columns.clone();
        columns.insert("bucket".to_owned(), Some(CqlValue::Int(expected)));
        columns.insert(OLD_BUCKET.to_owned(), Some(CqlValue::Int(stored)));
        Ok(Outcome::Write(Record::Scylla {
            key: key.clone(),
            columns,
        }))
    }

    /// Insert the row in its new bucket, then delete it from the old one
    ///
    /// # Arguments
    ///
    /// * `record` - The converted row
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        let Record::Scylla { columns, .. } = record else {
            return Err(MigrateError::Systematic(
                "a rebucket row isn't a Scylla row".to_owned(),
            ));
        };
        let ns = &ctx.backends.namespace;
        // the table this row is from, found by its timestamp column
        let spec = self
            .tables
            .iter()
            .find(|table| columns.contains_key(table.timestamp))
            .ok_or_else(|| MigrateError::Systematic("a row had no timestamp".to_owned()))?;
        let (names, partition, cluster) = table_columns(spec.table)?;
        let old = columns.get(OLD_BUCKET).cloned().flatten();
        // the idempotent insert of the new row
        let values = names
            .iter()
            .map(|name| columns.get(*name).cloned().flatten())
            .collect::<Vec<Option<CqlValue>>>();
        let markers = vec!["?"; names.len()].join(", ");
        let insert = format!(
            "INSERT INTO {ns}.{} ({}) VALUES ({markers})",
            spec.table,
            names.join(", ")
        );
        let insert = prepared(&ctx.backends.scylla, insert).await?;
        ctx.backends
            .scylla
            .execute_unpaged(&insert, values)
            .await
            .map_err(|err| backend_error("insert a moved row", &err, is_rejection(&err)))?;
        // then the delete of the old row, by its old key
        let keys = partition
            .iter()
            .chain(&cluster)
            .copied()
            .collect::<Vec<_>>();
        let filter = keys
            .iter()
            .map(|name| format!("{name} = ?"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let binds = keys
            .iter()
            .map(|name| {
                if *name == "bucket" {
                    old.clone()
                } else {
                    columns.get(*name).cloned().flatten()
                }
            })
            .collect::<Vec<Option<CqlValue>>>();
        let delete = prepared(
            &ctx.backends.scylla,
            format!("DELETE FROM {ns}.{} WHERE {filter}", spec.table),
        )
        .await?;
        ctx.backends
            .scylla
            .execute_unpaged(&delete, binds)
            .await
            .map_err(|err| backend_error("delete a moved row", &err, is_rejection(&err)))?;
        Ok(())
    }

    /// Rebuild the census in its two phases, then record the sizes
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // resume from where the last run stopped, bounded when a test asks
        let mut batches = ctx.limits.census_batches;
        let mut state = self.load_state(ctx).await?;
        if state == CensusState::Start {
            state = CensusState::Delete {
                family: 0,
                cursor: 0,
            };
        }
        // phase one: delete the census keys
        if let CensusState::Delete { family, cursor } = state {
            self.delete_census(ctx, &mut batches, family, cursor)
                .await?;
            state = CensusState::Add {
                table: 0,
                token: None,
            };
            self.save_state(ctx, &state).await?;
        }
        // phase two: add each partition's bucket from the tables
        if let CensusState::Add { table, token } = state {
            self.add_census(ctx, &mut batches, table, token).await?;
            self.save_state(ctx, &CensusState::Done).await?;
        }
        // the census is rebuilt; record the sizes the rows now have
        self.record_sizes(ctx).await?;
        // the rebuild is over, so the next run starts fresh
        let mut conn = ctx
            .backends
            .redis
            .get()
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        redis::cmd("DEL")
            .arg(self.state_key(&ctx.backends.namespace))
            .query_async::<()>(&mut *conn)
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))
    }
}

/// Read every row of a declared table in token order, a batch at a time, handing each batch to a
/// callback
///
/// The whole ring is walked as one slice, so a caller that only needs to look at every row, such
/// as a migration collecting the names it will map, needs no worker, checkpoint, or lease. A batch
/// ends on a partition boundary and holds at most `ctx.limits.batch_rows` rows, except that its
/// last partition is read whole.
///
/// # Arguments
///
/// * `ctx` - The run's context, for the Scylla session, the keyspace, and the batch size
/// * `table` - The declared table to read
/// * `columns` - The columns to read
/// * `visit` - Called with each batch's records; returning an error stops the scan
pub async fn scan_table<F>(
    ctx: &RunContext,
    table: &str,
    columns: &[&str],
    mut visit: F,
) -> Result<(), MigrateError>
where
    F: FnMut(&[Record]) -> Result<(), MigrateError>,
{
    // the table's declared key decides the token and the record keys
    let (_, partition, cluster) = table_columns(table)?;
    let scan = ScyllaScan {
        keyspace: ctx.backends.namespace.clone(),
        table: table.to_owned(),
        partition_key: partition.iter().map(|name| (*name).to_owned()).collect(),
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        key_columns: partition
            .iter()
            .chain(&cluster)
            .map(|name| (*name).to_owned())
            .collect(),
        cuts: Vec::new(),
    };
    // walk the whole ring in order, one batch after another
    let slice = ring_slices(1, &[]).remove(0);
    let mut after = serde_json::Value::Null;
    loop {
        let batch = scan.next_batch(&slice, &after, ctx).await?;
        visit(&batch.records)?;
        after = batch.next;
        if batch.done {
            return Ok(());
        }
    }
}

/// Whether a record holds a column
///
/// # Arguments
///
/// * `record` - The record
/// * `name` - The column
fn record_has(record: &Record, name: &str) -> bool {
    // only a Scylla record has columns
    record.has_field(name)
}
