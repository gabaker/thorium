//! Checks a restored cluster against the manifest of the backup it was restored from
//!
//! `backup verify --cluster` counts the rows of every backed-up table per token range and compares
//! them with the manifest's counts, asks S3 for every object the backup's indexes list, diffs the
//! API's Redis state with `redis.json` record by record, checks that every registered view exists
//! and is built, and checks that every partition of every table with a census is in its census
//! stream.
//! Counting assumes no writer is active: a count above the manifest's fails too.

use aws_sdk_s3::Client;
use chrono::Utc;
use futures::stream::{self, StreamExt};
use scylla::client::session::Session;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use thorium::{Conf, CtlConf, Thorium};
use tracing::{info, warn};

use super::coverage::Registry;
use super::failures::{FailureLog, FailurePolicy, FailureRecord, Operation};
use super::manifest::{self, Layout, Manifest, RangeCount, S3Entry};
use super::s3::s3_client;
use super::sink::SinkFactory;
use super::target::bucket_for;
use super::verify::{
    Checks, MANIFEST_COMPONENT, REPORT_FILE, VerifyMode, VerifyReport, log_checks, open_report_dir,
    table_component,
};
use super::version;
use super::views::{existing_views, view_built};
use crate::Error;
use crate::args::{BackupComponents, CensusKinds, VerifyBackup};
use crate::outcome::RunOutcome;
use crate::shared::retry::RetryPolicy;

/// How long one count of a token range may take before it is retried
const COUNT_TIMEOUT: Duration = Duration::from_secs(300);

/// The fields of each Redis record type that hold sets, which the API lists in no fixed order, as
/// JSON pointers into the record
const SET_FIELDS: &[(&str, &[&str])] = &[
    (
        "groups",
        &[
            "/owners/combined",
            "/owners/direct",
            "/owners/metagroups",
            "/managers/combined",
            "/managers/direct",
            "/managers/metagroups",
            "/users/combined",
            "/users/direct",
            "/users/metagroups",
            "/monitors/combined",
            "/monitors/direct",
            "/monitors/metagroups",
            "/analysts",
        ],
    ),
    (
        "images",
        &[
            "/network_policies",
            "/child_filters/mime",
            "/child_filters/file_name",
            "/child_filters/file_extension",
            "/used_by",
        ],
    ),
    ("settings", &["/host_path_whitelist"]),
    ("network_policy_sets", &["/policies"]),
    ("users", &["/groups", "/actual_groups"]),
];

/// Sorts the set fields of a Redis record, so two listings of one record compare equal
///
/// # Arguments
///
/// * `kind` - The record type
/// * `record` - The record, as JSON
fn sort_sets(kind: &str, record: &mut Value) {
    // each set field the record type has, sorted by each member's JSON text
    let pointers = SET_FIELDS
        .iter()
        .find(|(set_kind, _)| *set_kind == kind)
        .map_or(&[][..], |(_, pointers)| *pointers);
    for pointer in pointers {
        if let Some(Value::Array(members)) = record.pointer_mut(pointer) {
            members.sort_by_cached_key(ToString::to_string);
        }
    }
}

/// Builds a failed verify record for a component
///
/// # Arguments
///
/// * `component` - The component checked
/// * `error` - What is wrong
fn problem<C: Into<String>, E: std::fmt::Display>(component: C, error: E) -> FailureRecord {
    // every verify record has the verify operation
    FailureRecord::failed(component, Operation::Verify, error)
}

/// Returns a table's partition key columns, in order, quoted for CQL
///
/// # Arguments
///
/// * `scylla` - The session to the cluster
/// * `keyspace` - The keyspace
/// * `table` - The table
async fn partition_key(
    scylla: &Session,
    keyspace: &str,
    table: &str,
) -> Result<Vec<String>, Error> {
    // the schema lists each column with its kind and position
    let rows = scylla
        .query_unpaged(
            "SELECT column_name, kind, position FROM system_schema.columns \
             WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, table),
        )
        .await?
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to read the columns of {table}: {error}")))?;
    let mut key: Vec<(i32, String)> = rows
        .rows::<(String, String, i32)>()
        .map_err(|error| Error::new(format!("failed to read the columns of {table}: {error}")))?
        .filter_map(Result::ok)
        .filter(|(_, kind, _)| kind == "partition_key")
        .map(|(name, _, position)| (position, format!("\"{}\"", name.replace('"', "\"\""))))
        .collect();
    key.sort();
    Ok(key.into_iter().map(|(_, name)| name).collect())
}

/// Checks a restored cluster against a backup's manifest
pub struct ClusterVerifier<'a> {
    /// The checks run and their counts
    checks: Checks,
    /// The config of the cluster, which names its keyspace and buckets
    conf: &'a Conf,
    /// A session to the cluster's Scylla
    scylla: &'a Session,
    /// How a read that fails for a transient reason is retried
    retry: RetryPolicy,
    /// How many ranges or objects are checked at once
    workers: usize,
}

impl<'a> ClusterVerifier<'a> {
    /// Builds a verifier of a cluster that records problems in a log
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log
    /// * `conf` - The config of the cluster
    /// * `scylla` - A session to the cluster's Scylla
    /// * `retry` - How a read that fails for a transient reason is retried
    /// * `workers` - How many ranges or objects are checked at once
    #[must_use]
    pub fn new(
        log: Arc<FailureLog>,
        conf: &'a Conf,
        scylla: &'a Session,
        retry: RetryPolicy,
        workers: usize,
    ) -> Self {
        // nothing checked yet
        ClusterVerifier {
            checks: Checks::new(log),
            conf,
            scylla,
            retry,
            workers: workers.max(1),
        }
    }

    /// Runs every check of the cluster the manifest makes possible
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `manifest` - The backup's manifest
    /// * `ctl_conf` - The Thorctl config of the API that serves the cluster's Redis state
    pub async fn check(
        &self,
        root: &Path,
        manifest: &Manifest,
        ctl_conf: &CtlConf,
    ) -> Result<(), Error> {
        // every table's rows per token range
        for (table, entry) in &manifest.tables {
            if self.checks.stopped() {
                return Ok(());
            }
            self.check_rows(table, &entry.ranges).await?;
        }
        // every object the backup's indexes list
        let s3 = s3_client(self.conf, self.conf.thorium.s3.use_path_style);
        for (component, entry) in &manifest.s3 {
            if self.checks.stopped() {
                return Ok(());
            }
            self.check_objects(&s3, root, component, entry, &manifest.buckets)
                .await?;
        }
        // the Redis state, the views, and the census
        if manifest.redis.is_some() && !self.checks.stopped() {
            self.check_redis(root, ctl_conf).await?;
        }
        let tables: Vec<&str> = manifest.tables.keys().map(String::as_str).collect();
        if !self.checks.stopped() {
            self.check_views(&tables).await?;
        }
        if !self.checks.stopped() {
            // the census of the tables the backup holds, which the restore rebuilt, and of every
            // censused table the keyspace holds when the backup holds Redis, since the Redis
            // restore deleted every census stream and the restore rebuilt them all
            let mut censused = tables.clone();
            if manifest.redis.is_some() {
                for table in self.censused_tables().await? {
                    if !censused.contains(&table) {
                        censused.push(table);
                    }
                }
            }
            self.check_census(&censused).await?;
        }
        Ok(())
    }

    /// Counts a table's rows in each token range and compares them with the manifest
    ///
    /// # Arguments
    ///
    /// * `table` - The table
    /// * `ranges` - The rows the backup read from each token range
    async fn check_rows(&self, table: &str, ranges: &[RangeCount]) -> Result<(), Error> {
        let component = table_component(table);
        let keyspace = &self.conf.thorium.namespace;
        // count each range by the token of the table's partition key
        let key = partition_key(self.scylla, keyspace, table).await?;
        // a table with no partition key isn't in the keyspace at all
        if key.is_empty() {
            let record =
                problem(&component, format!("{keyspace}.{table} doesn't exist")).table(table);
            return self.checks.fail("row-count", record).await;
        }
        // one prepared count of a token range, with a timeout long enough for a large range
        let token = format!("token({})", key.join(", "));
        let mut prepared = self
            .scylla
            .prepare(format!(
                "SELECT COUNT(*) FROM {keyspace}.{table} WHERE {token} >= ? AND {token} <= ?"
            ))
            .await?;
        prepared.set_request_timeout(Some(COUNT_TIMEOUT));
        let prepared = &prepared;
        // count the ranges a few at a time, failing on the first error
        stream::iter(ranges.iter().copied())
            .map(|range| {
                let component = &component;
                async move { self.check_range(component, table, prepared, range).await }
            })
            .buffer_unordered(self.workers)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Error>>()?;
        Ok(())
    }

    /// Counts one token range of a table and compares it with the manifest
    ///
    /// # Arguments
    ///
    /// * `component` - The table's component
    /// * `table` - The table
    /// * `prepared` - The count of a token range
    /// * `range` - The range and the rows the backup read from it
    async fn check_range(
        &self,
        component: &str,
        table: &str,
        prepared: &scylla::statement::prepared::PreparedStatement,
        range: RangeCount,
    ) -> Result<(), Error> {
        // take no new range once --abort-on-failure stopped the run
        if self.checks.stopped() {
            return Ok(());
        }
        let bounds = (range.start, range.end);
        // count the range, retrying a count that fails for a transient reason
        let retried = self
            .retry
            .run(|| self.scylla.execute_unpaged(prepared, bounds))
            .await;
        let counted = retried
            .result
            .map_err(Error::from)
            .and_then(|result| {
                result
                    .into_rows_result()
                    .map_err(|error| Error::new(format!("the count returned no rows: {error}")))
            })
            .and_then(|rows| {
                rows.first_row::<(i64,)>()
                    .map_err(|error| Error::new(format!("the count didn't decode: {error}")))
            });
        let error = match counted {
            Ok((rows,)) if u64::try_from(rows).is_ok_and(|rows| rows == range.rows) => {
                self.checks.pass(component, "row-count");
                return Ok(());
            }
            Ok((rows,)) => format!(
                "{table} holds {rows} rows in tokens {} to {}; the backup holds {}",
                range.start, range.end, range.rows
            ),
            Err(error) => format!(
                "failed to count {table} in tokens {} to {}: {error}",
                range.start, range.end
            ),
        };
        let record = problem(component, error)
            .table(table)
            .token_range(bounds)
            .attempts(retried.attempts);
        self.checks.fail("row-count", record).await
    }

    /// Asks S3 for every object an S3 component's index lists, checking its size
    ///
    /// # Arguments
    ///
    /// * `s3` - The client for the cluster's S3
    /// * `root` - The backup root
    /// * `name` - The S3 component
    /// * `entry` - The component's manifest entry
    /// * `buckets` - The bucket name of each bucket role when the backup ran
    async fn check_objects(
        &self,
        s3: &Client,
        root: &Path,
        name: &str,
        entry: &S3Entry,
        buckets: &BTreeMap<String, String>,
    ) -> Result<(), Error> {
        // each object goes to the bucket its role has in this cluster's config
        let roles: BTreeMap<&str, &str> = buckets
            .iter()
            .map(|(role, bucket)| (bucket.as_str(), role.as_str()))
            .collect();
        let mut objects: Vec<(String, String, u64)> =
            manifest::read_index(&root.join(&entry.index))
                .await?
                .into_iter()
                .map(|((bucket, key), (size, _))| {
                    let target = roles
                        .get(bucket.as_str())
                        .and_then(|role| bucket_for(self.conf, role))
                        .unwrap_or(bucket);
                    (target, key, size)
                })
                .collect();
        // check the objects a few at a time, in key order
        objects.sort();
        stream::iter(objects)
            .map(|(bucket, key, size)| async move {
                self.check_object(s3, name, &bucket, &key, size).await
            })
            .buffer_unordered(self.workers)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Error>>()?;
        Ok(())
    }

    /// Asks S3 for one object and checks its size
    ///
    /// # Arguments
    ///
    /// * `s3` - The client for the cluster's S3
    /// * `name` - The S3 component
    /// * `bucket` - The bucket the object belongs in
    /// * `key` - The object's key
    /// * `size` - Its size in the backup's index
    async fn check_object(
        &self,
        s3: &Client,
        name: &str,
        bucket: &str,
        key: &str,
        size: u64,
    ) -> Result<(), Error> {
        // take no new object once --abort-on-failure stopped the run
        if self.checks.stopped() {
            return Ok(());
        }
        // ask for the object, retrying a request that fails for a transient reason
        let retried = self
            .retry
            .run(|| async {
                s3.head_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(Error::from)
            })
            .await;
        let error = match retried.result {
            Ok(head)
                if head
                    .content_length()
                    .and_then(|len| u64::try_from(len).ok())
                    == Some(size) =>
            {
                self.checks.pass(name, "object");
                return Ok(());
            }
            Ok(head) => format!(
                "{bucket}/{key} is {:?} bytes long; the backup holds {size}",
                head.content_length()
            ),
            Err(error) => format!("{bucket}/{key} can't be read: {error}"),
        };
        let record = problem(name, error)
            .bucket(bucket)
            .key(key)
            .attempts(retried.attempts);
        self.checks.fail("object", record).await
    }

    /// Diffs the API's Redis state with `redis.json`, record by record, naming the fields that
    /// differ but never their values, which hold credentials; sets compare in any order
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `ctl_conf` - The Thorctl config of the API that serves the cluster's Redis state
    async fn check_redis(&self, root: &Path, ctl_conf: &CtlConf) -> Result<(), Error> {
        let component = BackupComponents::Redis.to_string();
        // the backup's state and the cluster's, as the API serves them
        let mut backed_up: Value =
            serde_json::from_slice(&tokio::fs::read(root.join("redis.json")).await?)?;
        // a backup taken before network-policy sets were backed up holds none, and the restore
        // rebuilds each group's set from the policies its images use, so expect those
        if let Some(fields) = backed_up.as_object_mut()
            && !fields.contains_key("network_policy_sets")
        {
            let sets = policy_sets_of_images(&fields["images"]);
            fields.insert("network_policy_sets".to_owned(), sets);
        }
        let live = match Thorium::from_ctl_conf(ctl_conf.clone()).await {
            Ok(client) => client.system.backup().await.map_err(Error::from),
            Err(error) => Err(Error::from(error)),
        };
        let live = match live.and_then(|live| Ok(serde_json::to_value(live)?)) {
            Ok(live) => live,
            Err(error) => {
                let record = problem(
                    &component,
                    format!("the API's Redis state can't be read: {error}"),
                );
                return self.checks.fail("redis-diff", record).await;
            }
        };
        // each record type, by the fields that name a record; a backup taken before scoped
        // tokens were backed up holds none, and restores none
        let types: [(&str, &[&str]); 6] = [
            ("users", &["username"]),
            ("groups", &["name"]),
            ("images", &["group", "name"]),
            ("pipelines", &["group", "name"]),
            ("scoped_tokens", &["owner", "name"]),
            ("network_policy_sets", &["group"]),
        ];
        for (kind, naming) in types {
            let expected = records(&backed_up[kind], naming);
            let found = records(&live[kind], naming);
            let names: BTreeSet<&String> = expected.keys().chain(found.keys()).collect();
            for name in names {
                let error = match (expected.get(name), found.get(name)) {
                    (Some(expected), Some(found)) => {
                        let fields = compare_records(kind, expected, found);
                        if fields.is_empty() {
                            self.checks.pass(&component, "redis-diff");
                            continue;
                        }
                        format!(
                            "{kind} {name} differs from the backup in {}",
                            fields.join(", ")
                        )
                    }
                    (Some(_), None) => {
                        format!("{kind} {name} is in the backup but not the cluster")
                    }
                    (None, _) => format!("{kind} {name} is in the cluster but not the backup"),
                };
                let record = problem(&component, error).key(format!("{kind}/{name}"));
                self.checks.fail("redis-diff", record).await?;
                if self.checks.stopped() {
                    return Ok(());
                }
            }
        }
        // the settings
        let fields = compare_records("settings", &backed_up["settings"], &live["settings"]);
        if fields.is_empty() {
            self.checks.pass(&component, "redis-diff");
        } else {
            let error = format!(
                "the settings differ from the backup in {}",
                fields.join(", ")
            );
            let record = problem(&component, error).key("settings");
            self.checks.fail("redis-diff", record).await?;
        }
        Ok(())
    }

    /// Checks that every view the registry lists for the backed-up tables exists and is built
    ///
    /// # Arguments
    ///
    /// * `tables` - The tables the backup holds
    async fn check_views(&self, tables: &[&str]) -> Result<(), Error> {
        let keyspace = &self.conf.thorium.namespace;
        let existing = existing_views(self.scylla, keyspace).await?;
        for table in tables {
            for view in Registry::views_of(table) {
                // a legacy view is checked only where an older release created it
                if Registry::is_legacy_view(view) && !existing.contains(view) {
                    continue;
                }
                // a view exists and every node reports it built
                let error = if !existing.contains(view) {
                    format!("{keyspace}.{view} doesn't exist")
                } else if !view_built(self.scylla, keyspace, view).await? {
                    format!("{keyspace}.{view} hasn't finished building")
                } else {
                    self.checks.pass("views", "views");
                    continue;
                };
                let record = problem("views", error).table(*table).key(view);
                self.checks.fail("views", record).await?;
                if self.checks.stopped() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Returns the backed-up tables with a census that the cluster's keyspace holds
    async fn censused_tables(&self) -> Result<Vec<&'static str>, Error> {
        // the keyspace's tables, by the schema
        let keyspace = &self.conf.thorium.namespace;
        let rows = self
            .scylla
            .query_unpaged(
                "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
                (keyspace,),
            )
            .await?
            .into_rows_result()
            .map_err(|error| {
                Error::new(format!("failed to list the tables of {keyspace}: {error}"))
            })?;
        let held: BTreeSet<String> = rows
            .rows::<(String,)>()
            .map_err(|error| {
                Error::new(format!("failed to list the tables of {keyspace}: {error}"))
            })?
            .filter_map(Result::ok)
            .map(|(table,)| table)
            .collect();
        // each backed-up table with census streams that the keyspace holds
        let tables = Registry::backed_up(super::coverage::Store::ScyllaTable)
            .map(|entry| entry.name)
            .filter(|table| !CensusKinds::of_table(table).is_empty() && held.contains(*table))
            .collect();
        Ok(tables)
    }

    /// Checks that every partition the given tables hold is in its census stream
    ///
    /// # Arguments
    ///
    /// * `tables` - The tables to check
    async fn check_census(&self, tables: &[&str]) -> Result<(), Error> {
        let keyspace = &self.conf.thorium.namespace;
        // every census kind of the tables
        let mut kinds = Vec::new();
        for table in tables {
            for kind in CensusKinds::of_table(table) {
                if !kinds.contains(kind) {
                    kinds.push(*kind);
                }
            }
        }
        // one Redis connection for every stream
        let pool = crate::shared::redis::get_client(self.conf).await?;
        let mut conn = pool
            .get()
            .await
            .map_err(|error| Error::new(format!("failed to connect to Redis: {error}")))?;
        for kind in kinds {
            // the members the partitions call for, grouped by stream
            let expected =
                crate::census::expected_members(kind, self.scylla, keyspace, self.retry).await?;
            let mut streams: BTreeMap<String, BTreeSet<i32>> = BTreeMap::new();
            for (stream, bucket) in expected {
                streams.entry(stream).or_default().insert(bucket);
            }
            // each stream holds every bucket
            for (stream, buckets) in streams {
                let held: Vec<String> = redis::cmd("ZRANGE")
                    .arg(&stream)
                    .arg(0)
                    .arg(-1)
                    .query_async(&mut *conn)
                    .await?;
                let held: BTreeSet<i32> = held
                    .iter()
                    .filter_map(|bucket| bucket.parse().ok())
                    .collect();
                let missing: Vec<i32> = buckets.difference(&held).copied().collect();
                if missing.is_empty() {
                    self.checks.pass("census", "census");
                    continue;
                }
                let error = format!(
                    "the {kind} census stream {stream} lacks buckets {missing:?} that the tables hold"
                );
                self.checks
                    .fail("census", problem("census", error).key(stream))
                    .await?;
                if self.checks.stopped() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Returns the network-policy sets a restore rebuilds from a backup's images: each group that
/// has an image using a policy, with every policy its images use
///
/// # Arguments
///
/// * `images` - The backup's images, as JSON
fn policy_sets_of_images(images: &Value) -> Value {
    // gather each group's policies across its images
    let mut sets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for image in images.as_array().into_iter().flatten() {
        let group = image["group"].as_str().unwrap_or_default();
        for policy in image["network_policies"].as_array().into_iter().flatten() {
            if let Some(policy) = policy.as_str() {
                sets.entry(group).or_default().insert(policy);
            }
        }
    }
    // list them as the backup lists its sets
    sets.into_iter()
        .map(|(group, policies)| serde_json::json!({"group": group, "policies": policies}))
        .collect()
}

/// Returns a list of records by the fields that name each one
///
/// # Arguments
///
/// * `list` - The records, as JSON
/// * `naming` - The fields whose values name a record
fn records(list: &Value, naming: &[&str]) -> BTreeMap<String, Value> {
    // a record's name joins its naming fields' values
    list.as_array().map_or_else(BTreeMap::new, |records| {
        records
            .iter()
            .map(|record| {
                let name = naming
                    .iter()
                    .map(|field| record[*field].as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("/");
                (name, record.clone())
            })
            .collect()
    })
}

/// Compares a cluster's Redis record with the backup's, returning the fields that differ
///
/// # Arguments
///
/// * `kind` - The record type
/// * `expected` - The backup's record
/// * `found` - The cluster's record
fn compare_records(kind: &str, expected: &Value, found: &Value) -> Vec<String> {
    // compare sets whatever order their members are listed in
    let (mut expected, mut found) = (expected.clone(), found.clone());
    sort_sets(kind, &mut expected);
    sort_sets(kind, &mut found);
    if expected == found {
        return Vec::new();
    }
    // every field that differs
    differing_fields(&expected, &found)
}

/// Returns the top-level fields two records disagree on
///
/// # Arguments
///
/// * `expected` - The backup's record
/// * `found` - The cluster's record
fn differing_fields(expected: &Value, found: &Value) -> Vec<String> {
    // records that aren't both objects differ in their shape
    let (Some(expected), Some(found)) = (expected.as_object(), found.as_object()) else {
        return vec!["their shape".to_owned()];
    };
    // every field either side holds whose values differ; a field the backup's record doesn't hold
    // was added by a later release, which serves it empty for a record restored from an older
    // backup
    let fields: BTreeSet<&String> = expected.keys().chain(found.keys()).collect();
    fields
        .into_iter()
        .filter(|field| match (expected.get(*field), found.get(*field)) {
            (None, Some(value)) => !is_empty_value(value),
            (expected, found) => expected != found,
        })
        .cloned()
        .collect()
}

/// Returns whether a value holds nothing: null, an empty list or map, or a map of empty values
///
/// # Arguments
///
/// * `value` - The value to check
fn is_empty_value(value: &Value) -> bool {
    // a map is empty when each of its values is
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.values().all(is_empty_value),
        _ => false,
    }
}

/// Verifies a restored cluster against a backup's manifest and returns how the run ended
///
/// # Arguments
///
/// * `args` - The `backup verify` arguments
/// * `conf` - The config of the cluster to check
/// * `ctl_conf` - The Thorctl config of the API that serves the cluster's Redis state
/// * `scylla` - A session to the cluster's Scylla
/// * `workers` - How many ranges or objects are checked at once
/// * `sinks` - The file-system seam the report is written through
pub async fn run(
    args: &VerifyBackup,
    conf: &Conf,
    ctl_conf: &CtlConf,
    scylla: &Session,
    workers: usize,
    sinks: Arc<dyn SinkFactory>,
) -> Result<RunOutcome, Error> {
    // record failures in a report directory so the backup's own files never change
    let reports = open_report_dir(&args.backup, args.report_dir.as_deref(), "verify").await?;
    let policy = FailurePolicy::from_flag(args.abort_on_failure);
    let log = Arc::new(FailureLog::open(&reports, policy, sinks.as_ref()).await?);
    let started = Utc::now();
    info!("Verifying the cluster against {}", args.backup.display());
    // an API of another version than thoradm's only warns, since verify writes nothing
    version::warn_on_mismatch(ctl_conf, "verify the cluster").await;
    let verifier = ClusterVerifier::new(
        log.clone(),
        conf,
        scylla,
        RetryPolicy::new(args.retries),
        workers,
    );
    // only a finished backup's manifest holds the counts the cluster is checked against
    let manifest = match manifest::classify(&args.backup).await? {
        Layout::Finished => Manifest::read(&args.backup).await,
        _ => Err(Error::new(format!(
            "{} has no manifest, so there is nothing to check the cluster against",
            args.backup.display()
        ))),
    };
    let (format_version, checked) = match manifest {
        Ok(Some(manifest)) => {
            verifier.check(&args.backup, &manifest, ctl_conf).await?;
            (Some(manifest.format_version), true)
        }
        Ok(None) => (None, false),
        Err(error) => {
            let record = problem(MANIFEST_COMPONENT, error).key("manifest.json");
            verifier.checks.fail("manifest", record).await?;
            (None, false)
        }
    };
    // sum it up in verify.json and the log
    let report = VerifyReport {
        mode: VerifyMode::Cluster,
        backup: args.backup.clone(),
        format_version,
        complete_backup: checked,
        checks: verifier.checks.snapshot(),
        stopped_early: verifier.checks.stopped(),
        started,
        finished: Utc::now(),
    };
    manifest::write_json_atomic(&reports, REPORT_FILE, &report, sinks.as_ref()).await?;
    log_checks(&report);
    let outcome = if checked {
        report.outcome()
    } else {
        RunOutcome::Aborted
    };
    match outcome {
        RunOutcome::Complete => info!("verify complete: the cluster matches the backup"),
        _ => warn!(
            "verify found {} differences between the cluster and the backup, listed in {}",
            report.failed(),
            log.path().display()
        ),
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::{compare_records, policy_sets_of_images};
    use serde_json::json;

    /// The network-policy sets rebuilt from images hold each group's policies across its images,
    /// and no group whose images use none
    #[test]
    fn policy_sets_come_from_the_images() {
        // two images of one group share a policy, and a group's image uses none
        let images = json!([
            {"group": "g", "name": "a", "network_policies": ["p2", "p1"]},
            {"group": "g", "name": "b", "network_policies": ["p1", "p3"]},
            {"group": "h", "name": "c", "network_policies": []},
        ]);
        assert_eq!(
            policy_sets_of_images(&images),
            json!([{"group": "g", "policies": ["p1", "p2", "p3"]}])
        );
    }

    /// Set fields compare in any order, while a member that differs is still found
    #[test]
    fn set_members_compare_in_any_order() {
        // the same members listed in another order
        let backup = json!({"name": "minerva", "analysts": ["io", "europa"], "owners": {"combined": ["io", "europa"], "direct": [], "metagroups": []}});
        let cluster = json!({"name": "minerva", "analysts": ["europa", "io"], "owners": {"combined": ["europa", "io"], "direct": [], "metagroups": []}});
        assert_eq!(
            compare_records("groups", &backup, &cluster),
            Vec::<String>::new()
        );
        // another member
        let changed = json!({"name": "minerva", "analysts": ["europa", "ganymede"], "owners": {"combined": ["europa", "io"], "direct": [], "metagroups": []}});
        assert_eq!(
            compare_records("groups", &backup, &changed),
            vec!["analysts"]
        );
        // a list that isn't a set keeps its order
        let first = json!({"name": "p", "order": [["a"], ["b"]]});
        let second = json!({"name": "p", "order": [["b"], ["a"]]});
        assert_eq!(compare_records("pipelines", &first, &second), vec!["order"]);
    }

    /// A user's group lists, read from Redis sets, compare in any order, while a member that
    /// differs is still found
    #[test]
    fn user_group_lists_compare_in_any_order() {
        // the same groups listed in another order, as Redis versions without ordered small sets do
        let backup =
            json!({"username": "io", "groups": ["g1", "g2", "g3"], "actual_groups": ["g2", "g1"]});
        let cluster =
            json!({"username": "io", "groups": ["g3", "g1", "g2"], "actual_groups": ["g1", "g2"]});
        assert_eq!(
            compare_records("users", &backup, &cluster),
            Vec::<String>::new()
        );
        // another group
        let changed =
            json!({"username": "io", "groups": ["g1", "g2", "g4"], "actual_groups": ["g1", "g2"]});
        assert_eq!(compare_records("users", &backup, &changed), vec!["groups"]);
    }

    /// An image's `used_by` list, read from a Redis set, compares in any order, while a member that
    /// differs is still found
    #[test]
    fn image_used_by_compares_in_any_order() {
        // the same pipelines listed in another order
        let backup = json!({"name": "img", "used_by": ["a", "b", "c"]});
        let cluster = json!({"name": "img", "used_by": ["c", "a", "b"]});
        assert_eq!(
            compare_records("images", &backup, &cluster),
            Vec::<String>::new()
        );
        // another member
        let changed = json!({"name": "img", "used_by": ["a", "b", "d"]});
        assert_eq!(
            compare_records("images", &backup, &changed),
            vec!["used_by"]
        );
    }

    /// A field the backup's record doesn't hold, because a later release added it, matches the
    /// cluster's record when the cluster holds it empty, as a later release serves a record
    /// restored from an older backup; a value in it is still a difference
    #[test]
    fn a_field_newer_than_the_backup_matches_when_empty() {
        // a pipeline from before stage tools, served with none
        let older = json!({"group": "g", "name": "p", "order": [["s"]]});
        let served = json!({"group": "g", "name": "p", "order": [["s"]], "stage_tools": {}});
        assert_eq!(
            compare_records("pipelines", &older, &served),
            Vec::<String>::new()
        );
        // settings from before model settings, served with their empty defaults
        let older = json!({"reserved_cpu": 500});
        let served = json!({"reserved_cpu": 500, "models": {"allowed_endpoints": []}});
        assert_eq!(
            compare_records("settings", &older, &served),
            Vec::<String>::new()
        );
        // a newer field the cluster holds a value in still differs
        let older = json!({"group": "g", "name": "p", "order": [["s"]]});
        let set = json!({"group": "g", "name": "p", "order": [["s"]], "stage_tools": {"s": "t"}});
        assert_eq!(
            compare_records("pipelines", &older, &set),
            vec!["stage_tools"]
        );
    }

    /// Every Redis field that differs from the backup is a difference: emptied bans, a cleared
    /// verification time, allowed data types or a setting at their defaults
    #[test]
    fn every_differing_redis_field_is_a_difference() {
        // emptied bans
        let backup = json!({"group": "g", "name": "i", "bans": {"b1": {"msg": "banned"}}});
        let emptied = json!({"group": "g", "name": "i", "bans": {}});
        assert_eq!(compare_records("images", &backup, &emptied), vec!["bans"]);
        // a cleared verification time
        let sent = json!({"username": "io", "verification_sent": "2026-01-01T00:00:00Z"});
        let cleared = json!({"username": "io", "verification_sent": null});
        assert_eq!(
            compare_records("users", &sent, &cleared),
            vec!["verification_sent"]
        );
        // a group's allowed data types put back to their defaults
        let disallowed = json!({"name": "g", "allowed": {"repos": false}});
        let defaults = json!({"name": "g", "allowed": {"repos": true}});
        assert_eq!(
            compare_records("groups", &disallowed, &defaults),
            vec!["allowed"]
        );
        // a setting put back to its default
        let changed = json!({"reserved_cpu": 1500, "host_path_whitelist": ["/b", "/a"]});
        let default = json!({"reserved_cpu": 500, "host_path_whitelist": ["/a", "/b"]});
        assert_eq!(
            compare_records("settings", &changed, &default),
            vec!["reserved_cpu"]
        );
        // a network-policy set's policies compare in any order
        let listed = json!({"group": "g", "policies": ["p1", "p2"]});
        let reordered = json!({"group": "g", "policies": ["p2", "p1"]});
        assert_eq!(
            compare_records("network_policy_sets", &listed, &reordered),
            Vec::<String>::new()
        );
    }
}
