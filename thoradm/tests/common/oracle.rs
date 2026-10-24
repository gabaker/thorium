//! The deep equality oracle: full snapshots of a namespace and the comparison of two of them
//!
//! A snapshot holds every row of every backed-up and excluded table and every view, every Redis
//! key of the namespace with its raw value, every object of every bucket with its length, sha256,
//! content type, and metadata, the census streams, and the TTLs of rows whose TTL a restore must
//! keep. Two snapshots compare equal location by location; the only exceptions are
//! `ORACLE_RULES`, each with a reason, and every other difference is listed.

use super::columns::{Table, live_table, read_rows};
use super::harness::{BUCKET_ROLES, Harness, bucket_for, list_keys, redis_pool};
use super::records::{RECORD_FAMILIES, part_node};
use super::schema::{BackupSchema, Node, Scalar, matches_variant};
use data_encoding::HEXLOWER;
use scylla::client::session::Session;
use scylla::value::CqlValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock, PoisonError};
use thoradm::backup::coverage::{Category, Coverage, Registry, Store};

/// How the oracle treats one location that isn't compared for plain equality
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleRuleKind {
    /// Not compared
    Ignore,
    /// Compared to a value computed from the found data (a lower-cased tag, a rebuilt set)
    Recomputed,
    /// Compared to a fixed value, rendered as the snapshot renders it (a node's reset health)
    Reset(&'static str),
    /// Compared by a bound (a TTL no greater than the TTL left at backup)
    Bound,
    /// Two stored forms that are the same value (JSON objects with their keys in another order)
    Equivalent,
    /// Must be absent after a restore
    Absent,
    /// Must be unchanged by a restore
    Unchanged,
    /// Not carried by this format version, so restored as null
    FormatLoss(Option<u32>),
}

/// One entry of `ORACLE_RULES`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleRule {
    /// The location the rule covers: `table.column`, `table.ttl()`, a Redis family pattern, the
    /// `EPHEMERAL` Redis families, an excluded table, or `s3:<role>` or `s3 <property>`
    pub location: &'static str,
    /// What the oracle does there
    pub kind: OracleRuleKind,
    /// Why, in words a reviewer can check
    pub reason: &'static str,
}

/// The location naming every Redis family the registry marks EPHEMERAL
pub const EPHEMERAL_FAMILIES: &str = "EPHEMERAL Redis families";

/// The location naming every Redis hash field and string that holds JSON
pub const JSON_VALUES: &str = "Redis JSON values";

/// The location naming the optional keys of record fields, which read as their defaults when
/// missing
pub const ABSENT_KEYS: &str = "absent optional record keys";

/// The location naming every column the registry says format 1 doesn't archive
pub const FORMAT_2_COLUMNS: &str = "format-2 columns";

/// The location naming the row TTLs format 1 archives don't carry, so restored rows take their
/// table's default TTL
pub const FORMAT_1_TTLS: &str = "format-1 row TTLs";

/// The archive format version today's backups write
pub const CURRENT_FORMAT: u32 = thoradm::backup::manifest::FORMAT_VERSION;

/// The object properties S3 keeps that a snapshot doesn't record
pub const UNRECORDED_S3_PROPERTIES: &[&str] =
    &["etag", "last_modified", "version_id", "storage_class"];

/// Every location the oracle doesn't compare for plain equality, with its rule and reason
pub const ORACLE_RULES: &[OracleRule] = &[
    OracleRule {
        location: "nodes.health",
        kind: OracleRuleKind::Reset("Text(\"\\\"Registered\\\"\")"),
        reason: "restore resets a node's health to registered",
    },
    OracleRule {
        location: "nodes.heart_beat",
        kind: OracleRuleKind::Reset("null"),
        reason: "restore clears a node's last heartbeat",
    },
    OracleRule {
        location: "notifications.ttl()",
        kind: OracleRuleKind::Bound,
        reason: "a permanent notification stays permanent and an expiring one keeps at most the TTL it had left, since TTLs keep counting down",
    },
    OracleRule {
        location: "tags.key_lower",
        kind: OracleRuleKind::Recomputed,
        reason: "restore recomputes it as the lower-cased key",
    },
    OracleRule {
        location: "tags.value_lower",
        kind: OracleRuleKind::Recomputed,
        reason: "restore recomputes it as the lower-cased value",
    },
    OracleRule {
        location: "image_used_by:*",
        kind: OracleRuleKind::Recomputed,
        reason: "restore rebuilds each image's pipelines from the restored pipelines",
    },
    OracleRule {
        location: "netpol_used_by:*",
        kind: OracleRuleKind::Recomputed,
        reason: "rebuilt from the restored images' network policies",
    },
    OracleRule {
        location: "image_data:* field bans",
        kind: OracleRuleKind::Equivalent,
        reason: "the API reads a missing ban map as empty and only banning writes it, so an empty map and no map are the same state",
    },
    OracleRule {
        location: "pipeline_data:* field bans",
        kind: OracleRuleKind::Equivalent,
        reason: "the API reads a missing ban map as empty and only banning writes it, so an empty map and no map are the same state",
    },
    OracleRule {
        location: "user_data:* field groups",
        kind: OracleRuleKind::Ignore,
        reason: "saving a user writes it, but nothing reads it: a user's groups come from user_groups",
    },
    OracleRule {
        location: "system_info",
        kind: OracleRuleKind::Ignore,
        reason: "restore re-initializes it the way a fresh deployment does",
    },
    OracleRule {
        location: "system_keys",
        kind: OracleRuleKind::Ignore,
        reason: "excluded and read by nothing",
    },
    OracleRule {
        location: "schema_migrations",
        kind: OracleRuleKind::Ignore,
        reason: "a restore records the schema: the backup's record, which the schema record tests compare, and the restoring cluster's own data-shaping settings and fingerprints",
    },
    OracleRule {
        location: "schema_migration_issues",
        kind: OracleRuleKind::Ignore,
        reason: "a restore writes the backup's record of migration passes, which the schema record tests compare",
    },
    OracleRule {
        location: "event-handler:*",
        kind: OracleRuleKind::Ignore,
        reason: "the API raises the event handler's cache-refresh flag when it restores; runtime state",
    },
    OracleRule {
        location: EPHEMERAL_FAMILIES,
        kind: OracleRuleKind::Absent,
        reason: "the Redis restore replaces the namespace, and runtime state is invalid after it",
    },
    OracleRule {
        location: JSON_VALUES,
        kind: OracleRuleKind::Equivalent,
        reason: "the API serializes maps in hash order, so equal JSON can store its keys in another order",
    },
    OracleRule {
        location: ABSENT_KEYS,
        kind: OracleRuleKind::Equivalent,
        reason: "a record written before a field existed reads it as its default, and restore writes the default out; the source has no value to lose",
    },
    OracleRule {
        location: FORMAT_2_COLUMNS,
        kind: OracleRuleKind::FormatLoss(Some(1)),
        reason: "format 1 archives don't carry them, so a restore leaves them null",
    },
    OracleRule {
        location: FORMAT_1_TTLS,
        kind: OracleRuleKind::FormatLoss(Some(1)),
        reason: "format 1 archives don't carry row TTLs, so a restored row takes its table's default TTL, permanent rows included",
    },
    OracleRule {
        location: "logs",
        kind: OracleRuleKind::Unchanged,
        reason: "excluded runtime data is never written or wiped",
    },
    OracleRule {
        location: "events",
        kind: OracleRuleKind::Unchanged,
        reason: "excluded runtime data is never written or wiped",
    },
    OracleRule {
        location: "s3:ephemeral",
        kind: OracleRuleKind::Unchanged,
        reason: "excluded runtime data is never written or wiped",
    },
    OracleRule {
        location: "s3:reaction_cache",
        kind: OracleRuleKind::Unchanged,
        reason: "excluded runtime data is never written or wiped",
    },
    OracleRule {
        location: "s3 etag",
        kind: OracleRuleKind::Ignore,
        reason: "depends on how the object was uploaded; the bytes are compared by sha256",
    },
    OracleRule {
        location: "s3 last_modified",
        kind: OracleRuleKind::Ignore,
        reason: "the time of the upload",
    },
    OracleRule {
        location: "s3 version_id",
        kind: OracleRuleKind::Ignore,
        reason: "assigned by the store on upload",
    },
    OracleRule {
        location: "s3 storage_class",
        kind: OracleRuleKind::Ignore,
        reason: "chosen by the store",
    },
];

/// One raw Redis value
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedisValueRepr {
    /// A string
    String(String),
    /// A hash, by field
    Hash(BTreeMap<String, String>),
    /// A set
    Set(BTreeSet<String>),
    /// A list, in order
    List(Vec<String>),
    /// A sorted set, as members with their scores, in score order
    ZSet(Vec<(String, String)>),
    /// A key that exists but couldn't be read, with the reason; it never equals another value
    Unreadable(String),
}

/// One Redis key's value and whether it expires
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisEntry {
    /// The raw value
    pub value: RedisValueRepr,
    /// Whether the key has a TTL
    pub expires: bool,
}

/// What a snapshot records of one object
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRepr {
    /// Its length in bytes
    pub len: u64,
    /// The sha256 of its bytes
    pub sha256: String,
    /// Its content type
    pub content_type: Option<String>,
    /// Its user metadata
    pub metadata: BTreeMap<String, String>,
}

/// One row: each set column's rendered value, by column name
pub type Row = BTreeMap<String, String>;

/// A full snapshot of a namespace
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Rows per table and per view, by rendered primary key
    pub scylla: BTreeMap<String, BTreeMap<String, Row>>,
    /// The remaining TTL of each row of the tables whose TTL a restore keeps, by table and key;
    /// `None` for a permanent row
    pub ttls: BTreeMap<String, BTreeMap<String, Option<i32>>>,
    /// The default TTL of each table whose row TTLs the snapshot records, in seconds (0 for none)
    #[serde(default)]
    pub default_ttls: BTreeMap<String, i32>,
    /// Every key of the namespace except the census streams, without the namespace prefix
    pub redis: BTreeMap<String, RedisEntry>,
    /// Every object, by bucket role and key
    pub s3: BTreeMap<(String, String), ObjectRepr>,
    /// Every census stream's members, by key without the namespace prefix
    pub census: BTreeMap<String, Vec<String>>,
}

/// A snapshot as a file holds it, with each object listed beside its bucket role and key, since
/// JSON object keys are text
#[derive(Debug, Serialize, Deserialize)]
struct SavedSnapshot {
    /// Rows per table and per view, by rendered primary key
    scylla: BTreeMap<String, BTreeMap<String, Row>>,
    /// The remaining TTL of each row of the tables whose TTL a restore keeps
    ttls: BTreeMap<String, BTreeMap<String, Option<i32>>>,
    /// The default TTL of each table whose row TTLs the snapshot records
    default_ttls: BTreeMap<String, i32>,
    /// Every key of the namespace except the census streams
    redis: BTreeMap<String, RedisEntry>,
    /// Every object, as its bucket role, key, and what the snapshot records of it
    s3: Vec<(String, String, ObjectRepr)>,
    /// Every census stream's members
    census: BTreeMap<String, Vec<String>>,
}

impl Snapshot {
    /// Writes the snapshot to a file as pretty JSON
    ///
    /// # Arguments
    ///
    /// * `path` - The file to write
    ///
    /// # Panics
    ///
    /// Panics if the file can't be written.
    pub fn save(&self, path: &std::path::Path) {
        // objects become a list, since their keys are pairs
        let saved = SavedSnapshot {
            scylla: self.scylla.clone(),
            ttls: self.ttls.clone(),
            default_ttls: self.default_ttls.clone(),
            redis: self.redis.clone(),
            s3: self
                .s3
                .iter()
                .map(|((role, key), object)| (role.clone(), key.clone(), object.clone()))
                .collect(),
            census: self.census.clone(),
        };
        let text = serde_json::to_string_pretty(&saved).expect("a snapshot serializes");
        std::fs::write(path, text + "\n")
            .unwrap_or_else(|err| panic!("failed to write {}: {err}", path.display()));
    }

    /// Reads a snapshot a [`Snapshot::save`] wrote
    ///
    /// # Arguments
    ///
    /// * `path` - The file to read
    ///
    /// # Panics
    ///
    /// Panics if the file can't be read or parsed.
    #[must_use]
    pub fn load(path: &std::path::Path) -> Self {
        // objects come back keyed by role and key
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let saved: SavedSnapshot = serde_json::from_str(&text)
            .unwrap_or_else(|err| panic!("{} isn't a saved snapshot: {err}", path.display()));
        Snapshot {
            scylla: saved.scylla,
            ttls: saved.ttls,
            default_ttls: saved.default_ttls,
            redis: saved.redis,
            s3: saved
                .s3
                .into_iter()
                .map(|(role, key, object)| ((role, key), object))
                .collect(),
            census: saved.census,
        }
    }
}

/// The tables whose row TTLs a snapshot records, and the regular column the TTL is read from
pub const TTL_TABLES: &[(&str, &str)] = &[("notifications", "msg")];

/// Renders one stored value the way snapshots compare it
///
/// # Arguments
///
/// * `value` - The stored value, `None` when unset
#[must_use]
pub fn render(value: Option<&CqlValue>) -> Option<String> {
    // Scylla stores an empty non-frozen collection as null, so both read as unset
    match value {
        None | Some(CqlValue::Empty) => None,
        Some(CqlValue::List(items) | CqlValue::Set(items)) if items.is_empty() => None,
        Some(CqlValue::Map(items)) if items.is_empty() => None,
        Some(value) => Some(format!("{value:?}")),
    }
}

/// Returns every table and view a snapshot reads: the backed-up and excluded tables and every
/// registered view
#[must_use]
pub fn snapshot_tables() -> Vec<&'static str> {
    // every registry table and view, whatever its coverage
    thoradm::backup::coverage::COVERAGE
        .iter()
        .filter(|entry| matches!(entry.store, Store::ScyllaTable | Store::ScyllaView))
        .map(|entry| entry.name)
        .collect()
}

/// Reads every row of a table or view, keyed by its rendered primary key
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `keyspace` - The keyspace
/// * `table` - The table's live columns
async fn read_table(scylla: &Session, keyspace: &str, table: &Table) -> BTreeMap<String, Row> {
    // render each row and key it by its primary key columns
    let keys: Vec<&str> = table
        .columns
        .iter()
        .filter(|column| column.is_key())
        .map(|column| column.name.as_str())
        .collect();
    let mut rows = BTreeMap::new();
    for raw in read_rows(scylla, keyspace, &table.name).await {
        let row: Row = raw
            .iter()
            .filter_map(|(name, value)| render(value.as_ref()).map(|text| (name.clone(), text)))
            .collect();
        let key = keys
            .iter()
            .map(|name| row.get(*name).cloned().unwrap_or_else(|| "null".to_owned()))
            .collect::<Vec<_>>()
            .join(" | ");
        rows.insert(key, row);
    }
    rows
}

/// Reads a table's default TTL in seconds, 0 when it has none
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `keyspace` - The namespace's keyspace
/// * `table` - The table
async fn read_default_ttl(scylla: &Session, keyspace: &str, table: &str) -> i32 {
    // the schema keeps it with the table's options
    scylla
        .query_unpaged(
            "SELECT default_time_to_live FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, table),
        )
        .await
        .expect("failed to read a table's default TTL")
        .into_rows_result()
        .expect("the schema returns rows")
        .maybe_first_row::<(i32,)>()
        .expect("the default TTL decodes")
        .map_or(0, |(ttl,)| ttl)
}

/// Reads the remaining TTL of every row of a table, keyed by rendered primary key
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `keyspace` - The keyspace
/// * `table` - The table's live columns
/// * `column` - The regular column whose TTL is read
async fn read_ttls(
    scylla: &Session,
    keyspace: &str,
    table: &Table,
    column: &str,
) -> BTreeMap<String, Option<i32>> {
    // select the key columns and the column's TTL
    let keys: Vec<&str> = table
        .columns
        .iter()
        .filter(|column| column.is_key())
        .map(|column| column.name.as_str())
        .collect();
    let query = format!(
        "SELECT {}, TTL({column}) FROM {keyspace}.{}",
        keys.join(", "),
        table.name
    );
    let mut ttls = BTreeMap::new();
    let mut pager = scylla
        .query_iter(query, ())
        .await
        .expect("failed to read TTLs")
        .rows_stream::<scylla::value::Row>()
        .expect("TTL rows decode");
    while let Some(row) = futures::StreamExt::next(&mut pager).await {
        let row = row.expect("a TTL row decodes");
        let (ttl, key_values) = row.columns.split_last().expect("a TTL row has columns");
        let key = key_values
            .iter()
            .map(|value| render(value.as_ref()).unwrap_or_else(|| "null".to_owned()))
            .collect::<Vec<_>>()
            .join(" | ");
        let ttl = match ttl {
            Some(CqlValue::Int(ttl)) => Some(*ttl),
            _ => None,
        };
        ttls.insert(key, ttl);
    }
    ttls
}

/// Reads one Redis key's raw value, or `None` when the key is gone
///
/// A key that exists but can't be read is kept as `RedisValueRepr::Unreadable`, so a snapshot never
/// silently leaves it out.
///
/// # Arguments
///
/// * `conn` - A Redis connection
/// * `key` - The full key
async fn read_redis(conn: &mut redis::aio::MultiplexedConnection, key: &str) -> Option<RedisEntry> {
    // a key that expired since it was listed is gone; any other failure is kept with its reason
    let unreadable = |error: redis::RedisError| RedisEntry {
        value: RedisValueRepr::Unreadable(error.to_string()),
        expires: false,
    };
    let kind: String = match redis::cmd("TYPE").arg(key).query_async(conn).await {
        Ok(kind) => kind,
        Err(error) => return Some(unreadable(error)),
    };
    // read the value by its type
    let value = match kind.as_str() {
        "none" => return None,
        "string" => redis::cmd("GET")
            .arg(key)
            .query_async(conn)
            .await
            .map(RedisValueRepr::String),
        "hash" => redis::cmd("HGETALL")
            .arg(key)
            .query_async(conn)
            .await
            .map(RedisValueRepr::Hash),
        "set" => redis::cmd("SMEMBERS")
            .arg(key)
            .query_async(conn)
            .await
            .map(RedisValueRepr::Set),
        "list" => redis::cmd("LRANGE")
            .arg(key)
            .arg(0)
            .arg(-1)
            .query_async(conn)
            .await
            .map(RedisValueRepr::List),
        "zset" => redis::cmd("ZRANGE")
            .arg(key)
            .arg(0)
            .arg(-1)
            .arg("WITHSCORES")
            .query_async(conn)
            .await
            .map(RedisValueRepr::ZSet),
        other => Ok(RedisValueRepr::Unreadable(format!(
            "a {other} key, which snapshots don't read"
        ))),
    };
    let value = match value {
        Ok(value) => value,
        Err(error) => return Some(unreadable(error)),
    };
    // whether the key expires
    let ttl: i64 = match redis::cmd("PTTL").arg(key).query_async(conn).await {
        Ok(ttl) => ttl,
        Err(error) => return Some(unreadable(error)),
    };
    Some(RedisEntry {
        value,
        expires: ttl >= 0,
    })
}

impl Snapshot {
    /// Takes a full snapshot of a harness's namespace
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the namespace
    ///
    /// # Panics
    ///
    /// Panics if a store can't be read.
    pub async fn take(harness: &Harness) -> Self {
        // every table and view, and the TTLs restore must keep
        let scylla = harness.scylla().await;
        let ns = harness.conf.thorium.namespace.clone();
        let mut snapshot = Snapshot::default();
        for name in snapshot_tables() {
            // a table or view this namespace lacks has nothing to read
            let table = live_table(&scylla, &ns, name).await;
            if table.columns.is_empty() {
                continue;
            }
            // read every row, then the TTLs of the tables whose TTLs restore keeps
            snapshot
                .scylla
                .insert(name.to_owned(), read_table(&scylla, &ns, &table).await);
            if let Some((_, column)) = TTL_TABLES.iter().find(|(listed, _)| *listed == name) {
                snapshot.ttls.insert(
                    name.to_owned(),
                    read_ttls(&scylla, &ns, &table, column).await,
                );
                snapshot
                    .default_ttls
                    .insert(name.to_owned(), read_default_ttl(&scylla, &ns, name).await);
            }
        }
        // every Redis key of the namespace, with census streams kept apart
        let pool = redis_pool(&harness.conf)
            .await
            .expect("failed to connect to Redis");
        let mut conn = pool
            .dedicated_connection()
            .await
            .expect("failed to connect to Redis");
        let keys: Vec<String> = super::harness::scan_keys(&mut conn, &format!("{ns}:*"))
            .await
            .unwrap();
        let prefix = format!("{ns}:");
        for key in keys {
            let Some(entry) = read_redis(&mut conn, &key).await else {
                continue;
            };
            let short = key.strip_prefix(&prefix).unwrap_or(&key).to_owned();
            if short.starts_with("census:") {
                let members = match entry.value {
                    RedisValueRepr::ZSet(members) => {
                        members.into_iter().map(|(member, _)| member).collect()
                    }
                    other => vec![format!("{other:?}")],
                };
                snapshot.census.insert(short, members);
            } else {
                snapshot.redis.insert(short, entry);
            }
        }
        // every object of every bucket
        let s3 = harness.s3();
        for role in BUCKET_ROLES {
            let bucket = bucket_for(&harness.conf, role);
            for key in list_keys(&s3, &bucket)
                .await
                .expect("failed to list a bucket")
            {
                let object = s3
                    .get_object()
                    .bucket(&bucket)
                    .key(&key)
                    .send()
                    .await
                    .expect("failed to read an object");
                let content_type = object.content_type().map(ToOwned::to_owned);
                let metadata: BTreeMap<String, String> =
                    object.metadata().map_or_else(BTreeMap::new, |metadata| {
                        metadata
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    });
                let bytes = object
                    .body
                    .collect()
                    .await
                    .expect("failed to read a body")
                    .into_bytes();
                snapshot.s3.insert(
                    ((*role).to_owned(), key),
                    ObjectRepr {
                        len: bytes.len() as u64,
                        sha256: HEXLOWER.encode(&Sha256::digest(&bytes)),
                        content_type,
                        metadata,
                    },
                );
            }
        }
        snapshot
    }
}

/// One difference the oracle found
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// The registry entry the location belongs to, named as in `entry_name`
    pub entry: String,
    /// Where: table and primary key and column, Redis key and field, or bucket and key and property
    pub location: String,
    /// The value in the expected snapshot, rendered
    pub expected: Option<String>,
    /// The value found, rendered
    pub found: Option<String>,
}

impl std::fmt::Display for Difference {
    /// Shows a difference on one line, with long values cut short
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // cut values to a readable length
        let short = |value: &Option<String>| match value {
            Some(text) if text.len() > 160 => format!(
                "{}... ({} bytes)",
                &text[..text.floor_char_boundary(160)],
                text.len()
            ),
            Some(text) => text.clone(),
            None => "<missing>".to_owned(),
        };
        write!(
            f,
            "[{}] {}: expected {}, found {}",
            self.entry,
            self.location,
            short(&self.expected),
            short(&self.found)
        )
    }
}

/// Returns the registry entry a Redis key belongs to, or the key itself when none matches
///
/// # Arguments
///
/// * `key` - The key without the namespace prefix
#[must_use]
pub fn redis_entry(key: &str) -> String {
    // the registry's most specific matching pattern names the family
    Registry::classify(Store::Redis, key)
        .map_or_else(|| key.to_owned(), |entry| entry.name.to_owned())
}

/// Returns whether a Redis key belongs to a family the registry marks EPHEMERAL
///
/// # Arguments
///
/// * `key` - The key without the namespace prefix
#[must_use]
pub fn is_ephemeral(key: &str) -> bool {
    // the matching entry's category decides
    Registry::classify(Store::Redis, key).is_some_and(|entry| entry.category == Category::Ephemeral)
}

/// Compares two snapshots under a set of rules
pub struct Oracle {
    /// The rules in force
    pub rules: &'static [OracleRule],
    /// The format version of the backup that was restored
    pub format: Option<u32>,
}

impl Default for Oracle {
    /// Builds the oracle with every rule of `ORACLE_RULES`, for a backup in today's format
    fn default() -> Self {
        // the one reviewed list of exceptions
        Oracle {
            rules: ORACLE_RULES,
            format: Some(CURRENT_FORMAT),
        }
    }
}

/// Returns whether format 1 archives leave a column of a table or view out
///
/// # Arguments
///
/// * `table` - The table or view
/// * `column` - The column
#[must_use]
pub fn format_two_column(table: &str, column: &str) -> bool {
    // a view's columns come from its base table
    let base = Registry::classify(Store::ScyllaView, table)
        .and_then(|view| view.base_table)
        .unwrap_or(table);
    thoradm::backup::coverage::FORMAT_2_COLUMNS.contains(&(base, column))
}

impl Oracle {
    /// Returns the rule for a location, if one covers it
    ///
    /// # Arguments
    ///
    /// * `location` - The rule location to look up
    fn rule(&self, location: &str) -> Option<&OracleRuleKind> {
        // locations match exactly
        self.rules
            .iter()
            .find(|rule| rule.location == location)
            .map(|rule| &rule.kind)
    }

    /// Says whether this oracle ignores a table, view, or Redis pattern outright
    ///
    /// # Arguments
    ///
    /// * `location` - The location to look up
    #[must_use]
    pub fn ignores(&self, location: &str) -> bool {
        // an ignore rule names the location exactly
        self.rule(location) == Some(&OracleRuleKind::Ignore)
    }

    /// Returns every difference between an expected and a found snapshot, empty when they match
    ///
    /// # Arguments
    ///
    /// * `expected` - The snapshot the restore should produce
    /// * `found` - The snapshot taken after the restore
    #[must_use]
    pub fn compare(&self, expected: &Snapshot, found: &Snapshot) -> Vec<Difference> {
        // walk every location of both snapshots, so a missing or extra entry is reported too
        let mut differences = Vec::new();
        self.compare_scylla(expected, found, &mut differences);
        self.compare_ttls(expected, found, &mut differences);
        self.compare_redis(expected, found, &mut differences);
        self.compare_s3(expected, found, &mut differences);
        compare_census(expected, found, &mut differences);
        differences
    }

    /// Compares every row and column of every table and view
    ///
    /// # Arguments
    ///
    /// * `expected` - The expected snapshot
    /// * `found` - The found snapshot
    /// * `differences` - Where differences are added
    fn compare_scylla(
        &self,
        expected: &Snapshot,
        found: &Snapshot,
        differences: &mut Vec<Difference>,
    ) {
        // every table either snapshot holds
        let empty = BTreeMap::new();
        let tables: BTreeSet<&String> = expected.scylla.keys().chain(found.scylla.keys()).collect();
        for table in tables {
            // a table the oracle ignores outright, such as the schema record a restore writes
            if self.rule(table) == Some(&OracleRuleKind::Ignore) {
                continue;
            }
            let before = expected.scylla.get(table).unwrap_or(&empty);
            let after = found.scylla.get(table).unwrap_or(&empty);
            // rows on one side only
            let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            for key in keys {
                let (old, new) = (before.get(key), after.get(key));
                if old.is_none() || new.is_none() {
                    differences.push(Difference {
                        entry: table.clone(),
                        location: format!("{table} [{key}]"),
                        expected: old.map(|row| format!("{row:?}")),
                        found: new.map(|row| format!("{row:?}")),
                    });
                    continue;
                }
                // each column of a row on both sides
                let (old, new) = (
                    old.unwrap_or(&BTreeMap::new()).clone(),
                    new.unwrap_or(&BTreeMap::new()).clone(),
                );
                let columns: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
                for column in columns {
                    let location = format!("{table}.{column}");
                    let format_loss = self.rule(FORMAT_2_COLUMNS)
                        == Some(&OracleRuleKind::FormatLoss(self.format))
                        && format_two_column(table, column);
                    let want = match self.rule(&location) {
                        _ if format_loss => None,
                        Some(OracleRuleKind::Ignore) => continue,
                        Some(OracleRuleKind::Reset(value)) => {
                            (*value != "null").then(|| (*value).to_owned())
                        }
                        Some(OracleRuleKind::Recomputed) => recompute_column(table, column, &new),
                        _ => old.get(column).cloned(),
                    };
                    let got = new.get(column).cloned();
                    if want != got {
                        differences.push(Difference {
                            entry: table.clone(),
                            location: format!("{table} [{key}] {column}"),
                            expected: want,
                            found: got,
                        });
                    }
                }
            }
        }
    }

    /// Compares row TTLs under their bound
    ///
    /// # Arguments
    ///
    /// * `expected` - The expected snapshot
    /// * `found` - The found snapshot
    /// * `differences` - Where differences are added
    fn compare_ttls(
        &self,
        expected: &Snapshot,
        found: &Snapshot,
        differences: &mut Vec<Difference>,
    ) {
        // a permanent row stays permanent; an expiring one keeps at most what it had left; a
        // format that doesn't carry TTLs gives every restored row its table's default
        let format_loss =
            self.rule(FORMAT_1_TTLS) == Some(&OracleRuleKind::FormatLoss(self.format));
        for (table, before) in &expected.ttls {
            let bounded = self.rule(&format!("{table}.ttl()")) == Some(&OracleRuleKind::Bound);
            let default = found
                .default_ttls
                .get(table)
                .or_else(|| expected.default_ttls.get(table))
                .copied()
                .unwrap_or(0);
            let after = found.ttls.get(table);
            let rows = found.scylla.get(table);
            for (key, ttl) in before {
                // an expiring row the restore lost has no TTL left to bound; a lost row is a
                // difference of its own already
                let Some(now) = after.and_then(|after| after.get(key)) else {
                    let lost = rows.is_none_or(|rows| !rows.contains_key(key));
                    if ttl.is_some() && !lost {
                        differences.push(Difference {
                            entry: table.clone(),
                            location: format!("{table} [{key}] ttl()"),
                            expected: Some(format!("{ttl:?}")),
                            found: None,
                        });
                    }
                    continue;
                };
                let holds = match (ttl, now) {
                    (_, None) if format_loss => default <= 0,
                    (_, Some(now)) if format_loss => *now > 0 && *now <= default,
                    (None, None) => true,
                    (Some(left), Some(now)) if bounded => *now > 0 && now <= left,
                    (left, now) => left == now,
                };
                if !holds {
                    differences.push(Difference {
                        entry: table.clone(),
                        location: format!("{table} [{key}] ttl()"),
                        expected: Some(format!("{ttl:?}")),
                        found: Some(format!("{now:?}")),
                    });
                }
            }
        }
    }

    /// Compares every Redis key and field
    ///
    /// # Arguments
    ///
    /// * `expected` - The expected snapshot
    /// * `found` - The found snapshot
    /// * `differences` - Where differences are added
    fn compare_redis(
        &self,
        expected: &Snapshot,
        found: &Snapshot,
        differences: &mut Vec<Difference>,
    ) {
        // every key either snapshot holds
        let keys: BTreeSet<&String> = expected.redis.keys().chain(found.redis.keys()).collect();
        for key in keys {
            let entry = redis_entry(key);
            let (old, new) = (expected.redis.get(key), found.redis.get(key));
            // a family's own rule comes first; other EPHEMERAL families must be gone
            let rule = match self.rule(&entry) {
                Some(rule) => Some(rule),
                None if is_ephemeral(key) => self.rule(EPHEMERAL_FAMILIES),
                None => None,
            };
            match rule {
                Some(OracleRuleKind::Ignore) => continue,
                Some(OracleRuleKind::Absent) => {
                    if let Some(new) = new {
                        differences.push(Difference {
                            entry,
                            location: format!("redis {key}"),
                            expected: None,
                            found: Some(format!("{:?}", new.value)),
                        });
                    }
                    continue;
                }
                _ => (),
            }
            let want = if rule == Some(&OracleRuleKind::Recomputed) {
                recompute_redis(&entry, key, found)
            } else {
                old.cloned()
            };
            let prefix = format!("{entry} field ");
            let settings = Comparison {
                json: self.rule(JSON_VALUES) == Some(&OracleRuleKind::Equivalent),
                ignored: self
                    .rules
                    .iter()
                    .filter(|rule| rule.kind == OracleRuleKind::Ignore)
                    .filter_map(|rule| rule.location.strip_prefix(prefix.as_str()))
                    .collect(),
                empty_maps: self
                    .rules
                    .iter()
                    .filter(|rule| rule.kind == OracleRuleKind::Equivalent)
                    .filter_map(|rule| rule.location.strip_prefix(prefix.as_str()))
                    .collect(),
                record: (self.rule(ABSENT_KEYS) == Some(&OracleRuleKind::Equivalent))
                    .then(|| record_node(&entry))
                    .flatten(),
            };
            compare_redis_value(&entry, key, want.as_ref(), new, &settings, differences);
        }
        // a recomputed key the found data implies but neither snapshot holds is missing
        for (key, value) in recomputed_keys(found) {
            let recomputed = self.rule(&redis_entry(&key)) == Some(&OracleRuleKind::Recomputed);
            if recomputed && !found.redis.contains_key(&key) && !expected.redis.contains_key(&key) {
                differences.push(Difference {
                    entry: redis_entry(&key),
                    location: format!("redis {key}"),
                    expected: Some(format!("{value:?}")),
                    found: None,
                });
            }
        }
    }

    /// Compares every object and its recorded properties
    ///
    /// # Arguments
    ///
    /// * `expected` - The expected snapshot
    /// * `found` - The found snapshot
    /// * `differences` - Where differences are added
    fn compare_s3(&self, expected: &Snapshot, found: &Snapshot, differences: &mut Vec<Difference>) {
        // every object either snapshot holds
        let keys: BTreeSet<&(String, String)> = expected.s3.keys().chain(found.s3.keys()).collect();
        for key in keys {
            let (role, name) = key;
            let entry = super::classes::entry_name(Store::S3, role);
            let (old, new) = (expected.s3.get(key), found.s3.get(key));
            match (old, new) {
                (Some(old), Some(new)) => {
                    for (property, want, got) in [
                        ("len", old.len.to_string(), new.len.to_string()),
                        ("sha256", old.sha256.clone(), new.sha256.clone()),
                        (
                            "content_type",
                            format!("{:?}", old.content_type),
                            format!("{:?}", new.content_type),
                        ),
                        (
                            "metadata",
                            format!("{:?}", old.metadata),
                            format!("{:?}", new.metadata),
                        ),
                    ] {
                        if self.rule(&format!("s3 {property}")) == Some(&OracleRuleKind::Ignore) {
                            continue;
                        }
                        if want != got {
                            differences.push(Difference {
                                entry: entry.clone(),
                                location: format!("s3 {role}/{name} {property}"),
                                expected: Some(want),
                                found: Some(got),
                            });
                        }
                    }
                }
                (old, new) => differences.push(Difference {
                    entry,
                    location: format!("s3 {role}/{name}"),
                    expected: old.map(|object| format!("{object:?}")),
                    found: new.map(|object| format!("{object:?}")),
                }),
            }
        }
    }
}

/// Returns whether a hash holds only fields a record may leave out, each at its default
///
/// # Arguments
///
/// * `value` - The found value
/// * `settings` - How the family's records compare
fn only_defaults(value: &RedisValueRepr, settings: &Comparison<'_>) -> bool {
    // only a record family's hash, whose every field reads as written when left out
    match value {
        RedisValueRepr::Hash(fields) if settings.record.is_some() => fields
            .iter()
            .all(|(field, after)| settings.same_field(field, None, Some(after))),
        _ => false,
    }
}

/// Compares one Redis value field by field or member by member
///
/// # Arguments
///
/// * `entry` - The key's registry entry
/// * `key` - The key without the namespace prefix
/// * `old` - The expected value
/// * `new` - The found value
/// * `settings` - How values compare
/// * `differences` - Where differences are added
fn compare_redis_value(
    entry: &str,
    key: &str,
    old: Option<&RedisEntry>,
    new: Option<&RedisEntry>,
    settings: &Comparison<'_>,
    differences: &mut Vec<Difference>,
) {
    // report a whole missing or extra key, then hash fields and set members one at a time
    let push = |location: String,
                expected: Option<String>,
                found: Option<String>,
                differences: &mut Vec<Difference>| {
        differences.push(Difference {
            entry: entry.to_owned(),
            location,
            expected,
            found,
        });
    };
    let (old, new) = match (old, new) {
        (Some(old), Some(new)) => (old, new),
        // a record the source holds no field of reads every field as its default, so a restore
        // that writes the defaults out loses nothing
        (None, Some(new)) if only_defaults(&new.value, settings) => return,
        (old, new) => {
            push(
                format!("redis {key}"),
                old.map(|old| format!("{:?}", old.value)),
                new.map(|new| format!("{:?}", new.value)),
                differences,
            );
            return;
        }
    };
    match (&old.value, &new.value) {
        // a value that couldn't be read can't be shown to match anything
        (RedisValueRepr::Unreadable(_), _) | (_, RedisValueRepr::Unreadable(_)) => {
            push(
                format!("redis {key}"),
                Some(format!("{:?}", old.value)),
                Some(format!("{:?}", new.value)),
                differences,
            );
        }
        (RedisValueRepr::Hash(before), RedisValueRepr::Hash(after)) => {
            // a user's theme is one property whether stored flat or inside `settings`
            let (before, after) = if entry == "user_data:*" {
                (
                    effective_user_settings(before),
                    effective_user_settings(after),
                )
            } else {
                (before.clone(), after.clone())
            };
            let fields: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            for field in fields
                .into_iter()
                .filter(|field| !settings.ignored.contains(field.as_str()))
            {
                if !settings.same_field(field, before.get(field), after.get(field)) {
                    push(
                        format!("redis {key} field {field}"),
                        before.get(field).cloned(),
                        after.get(field).cloned(),
                        differences,
                    );
                }
            }
        }
        (RedisValueRepr::Set(before), RedisValueRepr::Set(after)) => {
            for member in before.symmetric_difference(after) {
                push(
                    format!("redis {key} member {member}"),
                    before.contains(member).then(|| member.clone()),
                    after.contains(member).then(|| member.clone()),
                    differences,
                );
            }
        }
        (RedisValueRepr::String(before), RedisValueRepr::String(after))
            if same_text(Some(before), Some(after), settings.json) => {}
        (before, after) if before != after => {
            push(
                format!("redis {key}"),
                Some(format!("{before:?}")),
                Some(format!("{after:?}")),
                differences,
            );
        }
        _ => (),
    }
    // a key that expires only on one side differs too
    if old.expires != new.expires {
        push(
            format!("redis {key} ttl"),
            Some(old.expires.to_string()),
            Some(new.expires.to_string()),
            differences,
        );
    }
}

/// How one Redis key's values compare
struct Comparison<'a> {
    /// Whether texts that parse as JSON compare as parsed JSON
    json: bool,
    /// The hash fields not compared
    ignored: BTreeSet<&'a str>,
    /// The hash fields holding a map the API reads as empty when the field is missing
    empty_maps: BTreeSet<&'a str>,
    /// The record's path in `Backup` and its schema node, when missing optional keys read as
    /// their defaults
    record: Option<(&'static str, &'static Node)>,
}

impl Comparison<'_> {
    /// Returns whether one hash field holds the same value on both sides
    ///
    /// # Arguments
    ///
    /// * `field` - The field
    /// * `before` - The expected text
    /// * `after` - The found text
    fn same_field(&self, field: &str, before: Option<&String>, after: Option<&String>) -> bool {
        // a missing map field reads as an empty map
        if self.empty_maps.contains(field) {
            let empty = "{}".to_owned();
            return same_text(before.or(Some(&empty)), after.or(Some(&empty)), self.json);
        }
        // a field the record's schema has, compared knowing which keys are optional
        let (path, prop) = match self.record {
            Some((path, Node::Object(props))) => (
                format!("{path}.{field}"),
                props.iter().find(|(name, ..)| name == field),
            ),
            _ => (String::new(), None),
        };
        match (prop, before, after) {
            // a key the source lacks may only be written out as its default
            (Some((_, false, node)), None, Some(after)) => is_default(&path, node, &stored(after)),
            (Some((.., node)), Some(before), Some(after)) if before != after => {
                match (
                    serde_json::from_str::<Value>(before),
                    serde_json::from_str::<Value>(after),
                ) {
                    (Ok(before), Ok(after)) => json_equivalent(&before, &after, node, &path),
                    _ => false,
                }
            }
            _ => same_text(before, after, self.json),
        }
    }
}

/// Folds a user hash's flat `theme` field into its `settings` field, so users stored in either
/// layout compare by their effective settings
///
/// A user with no theme reads as the default theme, and absent or null AI settings are left out.
///
/// # Arguments
///
/// * `fields` - The user hash's fields
fn effective_user_settings(fields: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    // settings that don't parse stay as written so a corrupt value still shows as a difference
    let mut fields = fields.clone();
    let flat = fields.remove("theme");
    let stored = match fields.get("settings") {
        Some(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => map,
            Ok(_) | Err(_) => return fields,
        },
        None => serde_json::Map::new(),
    };
    // the flat theme wins over one inside the settings, and the default fills a missing theme
    let theme = flat
        .and_then(|flat| serde_json::from_str::<Value>(&flat).ok())
        .or_else(|| stored.get("theme").cloned())
        .unwrap_or_else(|| Value::String("Automatic".to_owned()));
    let mut effective = serde_json::Map::new();
    effective.insert("theme".to_owned(), theme);
    if let Some(ai) = stored.get("ai").filter(|ai| !ai.is_null()) {
        effective.insert("ai".to_owned(), ai.clone());
    }
    fields.insert("settings".to_owned(), Value::Object(effective).to_string());
    fields
}

/// Returns the schema node of the records a Redis family holds, if it holds `Backup` records
///
/// # Arguments
///
/// * `entry` - The family's registry entry
fn record_node(entry: &str) -> Option<(&'static str, &'static Node)> {
    /// The walked `Backup` schema, read once
    static TREE: OnceLock<Node> = OnceLock::new();
    let tree = TREE.get_or_init(|| BackupSchema::load().tree());
    if entry == "system_settings" {
        return Some((".settings", part_node(tree, "settings")));
    }
    RECORD_FAMILIES
        .iter()
        .find(|(_, family, _)| entry.strip_suffix(":*") == Some(*family))
        .map(|(part, _, path)| (*path, part_node(tree, part)))
}

/// Reads a stored hash field as JSON, or as a JSON string when it holds plain text
///
/// # Arguments
///
/// * `text` - The stored text
fn stored(text: &str) -> Value {
    // fields like a password hash are stored as plain text, not as JSON
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// One step of a field path
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// A named key of an object
    Key(String),
    /// An element of a list or set
    Item,
    /// A value of a map
    Entry,
    /// The alternative of a `oneOf` with this label
    Variant(String),
}

/// Splits a field path like `.images[].security_context<SecurityContext>.user` into its steps
///
/// # Arguments
///
/// * `path` - The field path
fn steps(path: &str) -> Vec<Step> {
    // read the path one character at a time
    let mut steps = Vec::new();
    let mut chars = path.chars().peekable();
    while let Some(next) = chars.next() {
        match next {
            // a key runs until the next marker
            '.' => {
                let mut key = String::new();
                while let Some(&c) = chars.peek() {
                    if matches!(c, '.' | '[' | '{' | '<') {
                        break;
                    }
                    key.push(c);
                    chars.next();
                }
                steps.push(Step::Key(key));
            }
            // `[]` and `{}` step into an element or a map value
            '[' => {
                chars.next();
                steps.push(Step::Item);
            }
            '{' => {
                chars.next();
                steps.push(Step::Entry);
            }
            // `<label>` names the alternative of a oneOf
            '<' => {
                let label: String = chars.by_ref().take_while(|c| *c != '>').collect();
                steps.push(Step::Variant(label));
            }
            _ => (),
        }
    }
    steps
}

/// Returns the smallest value a schema node accepts: its required keys only, empty
/// collections, the first name or alternative, and zero, false, or placeholder text
///
/// # Arguments
///
/// * `node` - The schema node
fn minimal(node: &Node) -> Value {
    // build each kind of node's smallest value
    match node {
        Node::Object(props) => Value::Object(
            props
                .iter()
                .filter(|(_, required, _)| *required)
                .map(|(name, _, inner)| (name.clone(), minimal(inner)))
                .collect(),
        ),
        Node::Array(_) | Node::Set(_) => Value::Array(Vec::new()),
        Node::Map(_) | Node::Opaque => Value::Object(serde_json::Map::new()),
        Node::OneOf(alternatives) => alternatives
            .iter()
            .find(|(_, alternative)| !matches!(alternative, Node::Null))
            .map_or(Value::Null, |(_, alternative)| minimal(alternative)),
        Node::Enum(names) => names.first().cloned().map_or(Value::Null, Value::String),
        Node::Scalar(_, true) | Node::Any | Node::Null => Value::Null,
        Node::Scalar(Scalar::String(format), false) => Value::String(
            match format.as_deref() {
                Some("date-time") => "1970-01-01T00:00:00Z",
                Some("date") => "1970-01-01",
                Some("uuid") => "00000000-0000-0000-0000-000000000000",
                _ => "x",
            }
            .to_owned(),
        ),
        Node::Scalar(Scalar::Integer { .. }, false) => Value::from(0),
        Node::Scalar(Scalar::Number, false) => Value::from(0.0),
        Node::Scalar(Scalar::Boolean, false) => Value::Bool(false),
    }
}

/// Builds the smallest value of a node that holds the parent of a path's last key, with that
/// key left out, or `None` when the path's last key is required or the path doesn't fit the node
///
/// # Arguments
///
/// * `node` - The node the first step applies to
/// * `steps` - The steps left to the left-out key
fn without_key(node: &Node, steps: &[Step]) -> Option<Value> {
    // a named alternative, or the first alternative the rest of the path fits
    if let Node::OneOf(alternatives) = node {
        return match steps.first() {
            Some(Step::Variant(label)) => alternatives
                .iter()
                .find(|(name, _)| name == label)
                .and_then(|(_, alternative)| without_key(alternative, &steps[1..])),
            _ => alternatives
                .iter()
                .find_map(|(_, alternative)| without_key(alternative, steps)),
        };
    }
    match (node, steps) {
        // the parent object, holding its required keys and the path's next key
        (Node::Object(props), [Step::Key(name), rest @ ..]) => {
            let (_, required, inner) = props.iter().find(|(prop, ..)| prop == name)?;
            let mut value = minimal(node);
            if rest.is_empty() {
                // the left-out key must be one a record may leave out
                return (!required).then_some(value);
            }
            value[name.as_str()] = without_key(inner, rest)?;
            Some(value)
        }
        // one element or one map value along the path
        (Node::Array(item) | Node::Set(item), [Step::Item, rest @ ..]) if !rest.is_empty() => {
            Some(Value::Array(vec![without_key(item, rest)?]))
        }
        (Node::Map(item), [Step::Entry, rest @ ..]) if !rest.is_empty() => {
            Some(serde_json::json!({ "k": without_key(item, rest)? }))
        }
        _ => None,
    }
}

/// Returns the value at the end of a path in a value `without_key` built and serde wrote back
///
/// # Arguments
///
/// * `value` - The written-back value
/// * `steps` - The path's steps
fn read_at<'a>(value: &'a Value, steps: &[Step]) -> Option<&'a Value> {
    // follow each step, which variant labels don't move
    steps.iter().try_fold(value, |value, step| match step {
        Step::Key(name) => value.get(name),
        Step::Item => value.get(0),
        Step::Entry => value.get("k"),
        Step::Variant(_) => Some(value),
    })
}

/// Reads a record through the API type a `Backup` part holds and writes it back out
///
/// # Arguments
///
/// * `part` - The `Backup` part (`users`, `groups`, `images`, `pipelines`, or `settings`)
/// * `record` - The record
fn through_api(part: &str, record: Value) -> Result<Value, String> {
    /// Reads a record as one API type and writes it back out
    ///
    /// # Arguments
    ///
    /// * `record` - The record
    fn round<T: serde::de::DeserializeOwned + Serialize>(record: Value) -> Result<Value, String> {
        let typed = T::deserialize(record).map_err(|err| err.to_string())?;
        serde_json::to_value(typed).map_err(|err| err.to_string())
    }
    use thorium::models::{Group, Image, Pipeline, SystemSettings, User};
    match part {
        "users" => round::<User>(record),
        "groups" => round::<Group>(record),
        "images" => round::<Image>(record),
        "pipelines" => round::<Pipeline>(record),
        "settings" => round::<SystemSettings>(record),
        other => Err(format!("{other} isn't a part of Backup")),
    }
}

/// Derives the value a key reads as when a record leaves it out, from the API's own types
///
/// A record holding only its required keys and the key's parents is read through the API type
/// and written back; the key's value there is what serde fills in. A key serde skips when
/// writing reads as null when it may be null, or else as its type's empty value.
///
/// # Arguments
///
/// * `path` - The key's field path, starting at a `Backup` part (`.images[]`, `.settings`)
/// * `node` - The key's schema node
fn derive_default(path: &str, node: &Node) -> Result<Value, String> {
    /// The walked `Backup` schema, read once
    static TREE: OnceLock<Node> = OnceLock::new();
    // find the record's part and the steps inside the record
    let steps = steps(path);
    let (part, inside) = match steps.as_slice() {
        [Step::Key(part), Step::Item, inside @ ..] if part != "settings" => (part, inside),
        [Step::Key(part), inside @ ..] if part == "settings" => (part, inside),
        _ => return Err(format!("{path} doesn't start at a Backup part")),
    };
    // build the record without the key and read it as the API does
    let tree = TREE.get_or_init(|| BackupSchema::load().tree());
    let record = without_key(part_node(tree, part), inside)
        .ok_or_else(|| format!("{path} isn't an optional key of the schema"))?;
    let written = through_api(part, record.clone())
        .map_err(|err| format!("{path}: the API can't read {record}: {err}"))?;
    // a key written back holds its default; a skipped one is null or empty
    match read_at(&written, inside) {
        Some(value) => Ok(value.clone()),
        None if node.nullable() => Ok(Value::Null),
        None => match node {
            Node::Array(_) | Node::Set(_) => Ok(Value::Array(Vec::new())),
            Node::Map(_) => Ok(Value::Object(serde_json::Map::new())),
            _ => Err(format!(
                "{path}: serde skips the key, so its default is unknown"
            )),
        },
    }
}

/// Returns the derived default of a key a record may leave out, derived once per path
///
/// # Arguments
///
/// * `path` - The key's field path
/// * `node` - The key's schema node
fn absent_default(path: &str, node: &Node) -> Result<Value, String> {
    /// The defaults derived so far, by path
    static DERIVED: OnceLock<Mutex<HashMap<String, Result<Value, String>>>> = OnceLock::new();
    let derived = DERIVED.get_or_init(Mutex::default);
    let mut derived = derived.lock().unwrap_or_else(PoisonError::into_inner);
    derived
        .entry(path.to_owned())
        .or_insert_with(|| derive_default(path, node))
        .clone()
}

/// Returns every key a record may leave out whose default can't be derived, with the reason
///
/// # Panics
///
/// Panics if a top-level part of `Backup` is missing from the schema.
#[must_use]
pub fn underivable_defaults() -> Vec<String> {
    /// Collects the optional keys under one node that have no derived default
    ///
    /// # Arguments
    ///
    /// * `path` - The node's field path
    /// * `node` - The node
    /// * `depth` - How many nodes deep the walk is, to stop at deep recursion
    /// * `problems` - Where problems are added
    fn walk(path: &str, node: &Node, depth: usize, problems: &mut Vec<String>) {
        // stop at a depth no record reaches
        if depth > 12 {
            return;
        }
        match node {
            Node::Object(props) => {
                for (name, required, inner) in props {
                    let child = format!("{path}.{name}");
                    if !required && let Err(err) = absent_default(&child, inner) {
                        problems.push(err);
                    }
                    walk(&child, inner, depth + 1, problems);
                }
            }
            Node::Array(item) | Node::Set(item) => {
                walk(&format!("{path}[]"), item, depth + 1, problems);
            }
            Node::Map(item) => walk(&format!("{path}{{}}"), item, depth + 1, problems),
            Node::OneOf(alternatives) => {
                for (label, alternative) in alternatives {
                    walk(
                        &format!("{path}<{label}>"),
                        alternative,
                        depth + 1,
                        problems,
                    );
                }
            }
            _ => (),
        }
    }
    // walk every record part of Backup
    let tree = BackupSchema::load().tree();
    let mut problems = Vec::new();
    for (part, path) in [
        ("users", ".users[]"),
        ("groups", ".groups[]"),
        ("images", ".images[]"),
        ("pipelines", ".pipelines[]"),
        ("settings", ".settings"),
    ] {
        walk(path, part_node(&tree, part), 0, &mut problems);
    }
    problems
}

/// Drops the null keys of a value's objects, since a key left out reads as null too
///
/// # Arguments
///
/// * `value` - The value
fn without_nulls(value: Value) -> Value {
    // walk objects and lists, keeping every key that isn't null
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, without_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nulls).collect()),
        other => other,
    }
}

/// Returns whether a found value is the default a missing optional key reads as, which is
/// derived from the API's own types
///
/// # Arguments
///
/// * `path` - The key's field path
/// * `node` - The key's schema node
/// * `found` - The found value
#[must_use]
pub fn is_default(path: &str, node: &Node, found: &Value) -> bool {
    // compare against the derived default, or accept only null when there is none
    match absent_default(path, node) {
        Ok(default) => json_equivalent(&without_nulls(default), found, node, path),
        Err(_) => found.is_null() && node.nullable(),
    }
}

/// Returns whether a found JSON value is the expected one, with missing optional keys of the
/// expected value allowed to hold only their defaults in the found one
///
/// # Arguments
///
/// * `expected` - The expected value
/// * `found` - The found value
/// * `node` - Their schema node
/// * `path` - Their field path, which names the defaults of missing keys
fn json_equivalent(expected: &Value, found: &Value, node: &Node, path: &str) -> bool {
    // walk both values under the schema
    match (node, expected, found) {
        (Node::Object(props), Value::Object(before), Value::Object(after)) => {
            let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            keys.into_iter().all(|key| {
                let prop = props.iter().find(|(name, ..)| name == key);
                let child = format!("{path}.{key}");
                match (prop, before.get(key), after.get(key)) {
                    (Some((.., inner)), Some(old), Some(new)) => {
                        json_equivalent(old, new, inner, &child)
                    }
                    (Some((_, false, inner)), None, Some(new)) => is_default(&child, inner, new),
                    (_, old, new) => old == new,
                }
            })
        }
        (Node::Array(item), Value::Array(before), Value::Array(after)) => {
            let child = format!("{path}[]");
            before.len() == after.len()
                && before
                    .iter()
                    .zip(after)
                    .all(|(old, new)| json_equivalent(old, new, item, &child))
        }
        // a serialized set's order means nothing, so each element matches one on the other side
        (Node::Set(item), Value::Array(before), Value::Array(after)) => {
            let child = format!("{path}[]");
            before.len() == after.len()
                && before.iter().all(|old| {
                    after
                        .iter()
                        .any(|new| json_equivalent(old, new, item, &child))
                })
        }
        (Node::Map(item), Value::Object(before), Value::Object(after)) => {
            let child = format!("{path}{{}}");
            before.len() == after.len()
                && before.iter().all(|(key, old)| {
                    after
                        .get(key)
                        .is_some_and(|new| json_equivalent(old, new, item, &child))
                })
        }
        (Node::OneOf(alternatives), expected, found) => alternatives
            .iter()
            .find(|(label, alternative)| matches_variant(label, alternative, expected))
            .map_or(expected == found, |(label, alternative)| {
                json_equivalent(expected, found, alternative, &format!("{path}<{label}>"))
            }),
        (_, expected, found) => expected == found,
    }
}

/// Returns whether two stored texts are the same value: equal, or equal as JSON when allowed
///
/// # Arguments
///
/// * `before` - The expected text
/// * `after` - The found text
/// * `json` - Whether texts that parse as JSON compare as parsed JSON
fn same_text(before: Option<&String>, after: Option<&String>, json: bool) -> bool {
    // plain equality first, then parsed JSON
    match (before, after) {
        (Some(before), Some(after)) if before != after && json => {
            match (
                serde_json::from_str::<serde_json::Value>(before),
                serde_json::from_str::<serde_json::Value>(after),
            ) {
                (Ok(before), Ok(after)) => before == after,
                _ => false,
            }
        }
        (before, after) => before == after,
    }
}

/// Compares every census stream
///
/// # Arguments
///
/// * `expected` - The expected snapshot, whose census was rebuilt on the source
/// * `found` - The found snapshot
/// * `differences` - Where differences are added
fn compare_census(expected: &Snapshot, found: &Snapshot, differences: &mut Vec<Difference>) {
    // a census rebuilt on the source and one rebuilt after the restore hold the same buckets
    let keys: BTreeSet<&String> = expected.census.keys().chain(found.census.keys()).collect();
    for key in keys {
        let (old, new) = (expected.census.get(key), found.census.get(key));
        if old != new {
            differences.push(Difference {
                entry: census_entry(key),
                location: format!("census {key}"),
                expected: old.map(|members| format!("{} buckets {members:?}", members.len())),
                found: new.map(|members| format!("{} buckets {members:?}", members.len())),
            });
        }
    }
}

/// Returns the table a census stream counts, or `census:*` for a stream of another kind
///
/// # Arguments
///
/// * `key` - The census key without the namespace prefix
#[must_use]
pub fn census_entry(key: &str) -> String {
    // the second segment names what the stream counts
    let counted = key.split(':').nth(1).unwrap_or_default();
    match counted {
        "samples" => "samples_list",
        "repos" => "repos_list",
        "tags" | "tags_case_insensitive" => "tags",
        "commitish" => "commitish_list",
        "entities" => "entities",
        "associations" => "associations",
        _ => "census:*",
    }
    .to_owned()
}

/// Reverses the escapes `{:?}` adds to a string
///
/// # Arguments
///
/// * `escaped` - The text between the quotes of a `{:?}` rendering
fn unescape_debug(escaped: &str) -> String {
    // walk the escapes Rust's Debug writes for strings
    let mut out = String::new();
    let mut chars = escaped.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let code: String = chars.by_ref().skip(1).take_while(|c| *c != '}').collect();
                if let Some(decoded) = u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
                {
                    out.push(decoded);
                }
            }
            Some(other) => out.push(other),
            None => (),
        }
    }
    out
}

/// Returns the rendered text inside a rendered text value
///
/// # Arguments
///
/// * `rendered` - A rendered `CqlValue::Text`
#[must_use]
pub fn text_of(rendered: &str) -> Option<String> {
    // a rendered text value is `Text("...")` with Rust's escapes
    let inner = rendered.strip_prefix("Text(\"")?.strip_suffix("\")")?;
    Some(unescape_debug(inner))
}

/// Recomputes a derived column from the found row
///
/// # Arguments
///
/// * `table` - The table
/// * `column` - The derived column
/// * `row` - The found row
fn recompute_column(table: &str, column: &str, row: &Row) -> Option<String> {
    // lower-cased tag keys and values
    let source = match (table, column) {
        ("tags", "key_lower") => "key",
        ("tags", "value_lower") => "value",
        _ => return row.get(column).cloned(),
    };
    let text = text_of(row.get(source)?)?;
    Some(format!("{:?}", CqlValue::Text(text.to_lowercase())))
}

/// Returns every key the found Redis data implies for the recomputed families, with its value
///
/// # Arguments
///
/// * `found` - The found snapshot
fn recomputed_keys(found: &Snapshot) -> BTreeMap<String, RedisEntry> {
    // each image is used by the pipelines whose stages run it
    let mut keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, entry) in &found.redis {
        let RedisValueRepr::Hash(fields) = &entry.value else {
            continue;
        };
        if redis_entry(key) == "pipeline_data:*" {
            let (Some(group), Some(name), Some(order)) =
                (fields.get("group"), fields.get("name"), fields.get("order"))
            else {
                continue;
            };
            let stages: Vec<Vec<String>> = serde_json::from_str(order).unwrap_or_default();
            for image in stages.into_iter().flatten() {
                keys.entry(format!("image_used_by:{group}:{image}"))
                    .or_default()
                    .insert(name.clone());
            }
        }
        // each network policy is used by the images that name it
        if redis_entry(key) == "image_data:*" {
            let (Some(group), Some(name), Some(policies)) = (
                fields.get("group"),
                fields.get("name"),
                fields.get("network_policies"),
            ) else {
                continue;
            };
            let policies: Vec<String> = serde_json::from_str(policies).unwrap_or_default();
            for policy in policies {
                keys.entry(format!("netpol_used_by:{group}:{policy}"))
                    .or_default()
                    .insert(name.clone());
            }
        }
    }
    keys.into_iter()
        .map(|(key, members)| {
            (
                key,
                RedisEntry {
                    value: RedisValueRepr::Set(members),
                    expires: false,
                },
            )
        })
        .collect()
}

/// Recomputes one key of a recomputed family from the found data
///
/// # Arguments
///
/// * `entry` - The key's registry entry
/// * `key` - The key without the namespace prefix
/// * `found` - The found snapshot
fn recompute_redis(entry: &str, key: &str, found: &Snapshot) -> Option<RedisEntry> {
    // the families recomputed from records, or the found value for anything else
    match entry {
        "image_used_by:*" | "netpol_used_by:*" => recomputed_keys(found).remove(key),
        _ => found.redis.get(key).cloned(),
    }
}

/// Returns every `ORACLE_RULES` location that names nothing that exists
///
/// # Arguments
///
/// * `rules` - The rules to check
/// * `tables` - The live columns of every table the rules may name
#[must_use]
pub fn stale_rules(rules: &[OracleRule], tables: &[Table]) -> Vec<String> {
    // each kind of location is checked against its own source
    let column_exists = |table: &str, column: &str| {
        tables.iter().any(|candidate| {
            candidate.name == table
                && (column == "ttl()" || candidate.columns.iter().any(|c| c.name == column))
        })
    };
    rules
        .iter()
        .filter(|rule| {
            let location = rule.location;
            let exists = if location == ABSENT_KEYS {
                true
            } else if let Some((family, _)) = location.split_once(" field ") {
                thoradm::backup::coverage::COVERAGE
                    .iter()
                    .any(|entry| entry.store == Store::Redis && entry.name == family)
            } else if location == JSON_VALUES {
                true
            } else if location == FORMAT_1_TTLS {
                !TTL_TABLES.is_empty()
            } else if location == FORMAT_2_COLUMNS {
                !thoradm::backup::coverage::FORMAT_2_COLUMNS.is_empty()
            } else if location == EPHEMERAL_FAMILIES {
                thoradm::backup::coverage::COVERAGE.iter().any(|entry| {
                    entry.store == Store::Redis && entry.category == Category::Ephemeral
                })
            } else if let Some(property) = location.strip_prefix("s3 ") {
                UNRECORDED_S3_PROPERTIES.contains(&property)
                    || ["len", "sha256", "content_type", "metadata"].contains(&property)
            } else if let Some(role) = location.strip_prefix("s3:") {
                BUCKET_ROLES.contains(&role)
            } else if let Some((table, column)) = location.split_once('.') {
                column_exists(table, column)
            } else {
                thoradm::backup::coverage::COVERAGE.iter().any(|entry| {
                    entry.name == location
                        && (entry.store == Store::Redis
                            || (entry.store == Store::ScyllaTable
                                && matches!(entry.coverage, Coverage::Excluded { .. })))
                })
            };
            !exists
        })
        .map(|rule| format!("ORACLE_RULES names {}, which doesn't exist", rule.location))
        .collect()
}
