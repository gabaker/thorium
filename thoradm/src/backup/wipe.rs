//! `backup restore --wipe <namespace>`: replacing the target's backed-up data with the backup
//!
//! The wipe empties exactly what a restore writes, the backed-up tables and buckets of the
//! [`Target`], and keeps the excluded data. Before anything is deleted the preflight checks the
//! namespace given to `--wipe` against thoradm's config, the namespace the API serves against
//! thoradm's, and the guards below, then builds the plan the admin confirms. The wipe itself drops
//! each table's registered views, truncates the table, and deletes the held objects of each bucket
//! in batches, rewriting `wipe.json` after each table and bucket so a re-run can resume.

use aws_sdk_s3::Client;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use chrono::{DateTime, Utc};
use scylla::client::session::Session;
use scylla::cluster::Node;
use scylla::policies::load_balancing::{NodeIdentifier, SingleTargetLoadBalancingPolicy};
use scylla::statement::unprepared::Statement;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thorium::Conf;
use thorium::conf::{NAMESPACE_MAX_LEN, validate_namespace};
use thorium::models::SystemInfo;
use tracing::{info, warn};

use super::coverage::{Coverage, Registry, Store};
use super::failures::{FailureLog, FailureRecord, Operation};
use super::manifest::write_json_atomic;
use super::sink::SinkFactory;
use super::target::{Target, TargetBucket, bucket_for};
use super::utils::drop_materialized_view;
use crate::Error;
use crate::shared::retry::{RetryPolicy, Transient};

/// The name of the file a wipe records what it planned and deleted in, in the report directory
pub const WIPE_FILE: &str = "wipe.json";

/// The component a wipe records its failures under
pub const WIPE_COMPONENT: &str = "wipe";

/// The most keys one `DeleteObjects` request removes
const DELETE_BATCH: usize = 1_000;

/// How long one `TRUNCATE` may take before the request times out
const TRUNCATE_TIMEOUT: Duration = Duration::from_secs(300);

/// Every bucket role in thoradm's config, so a bucket another role shares is found
const CONFIG_ROLES: &[&str] = &[
    "files",
    "repos",
    "attachments",
    "results",
    "graphics",
    "ephemeral",
    "reaction_cache",
];

/// Each bucket role's default name, which every namespace uses unless its config names another
const DEFAULT_BUCKETS: &[(&str, &str)] = &[
    ("files", "thorium-files"),
    ("repos", "thorium-repos-files"),
    ("attachments", "thorium-attachment-files"),
    ("results", "thorium-result-files"),
    ("graphics", "thorium-graphics"),
    ("ephemeral", "thorium-ephemeral-files"),
    ("reaction_cache", "thorium-reaction-cache-files"),
];

/// The keyspaces Scylla keeps for itself, which are never Thorium namespaces; a user keyspace may
/// still start with `system`
const SYSTEM_KEYSPACES: &[&str] = &[
    "system",
    "system_auth",
    "system_distributed",
    "system_distributed_everywhere",
    "system_replicated_keys",
    "system_schema",
    "system_traces",
    "system_views",
    "system_virtual_schema",
];

/// What a `--wipe` restore planned, deleted, and kept, written as `wipe.json`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WipeReport {
    /// The namespace (keyspace and Redis prefix) that was wiped, from the loaded config, which the
    /// value given to `--wipe` matched
    pub namespace: String,
    /// The namespace the API reported serving, checked equal to `namespace` before the wipe
    pub api_namespace: String,
    /// The backup being restored, as given on the command line
    pub backup: PathBuf,
    /// The report directory of the earlier unfinished wipe this run resumed, if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<PathBuf>,
    /// When the wipe started
    pub started: DateTime<Utc>,
    /// When the wipe finished; `None` while it runs or after it failed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<DateTime<Utc>>,
    /// One entry per backed-up table in the plan
    pub tables: Vec<WipedTable>,
    /// One entry per backed-up bucket in the plan
    pub buckets: Vec<WipedBucket>,
    /// The excluded tables and buckets left in place, by registry name
    pub kept: Vec<String>,
    /// Whether Scylla's `auto_snapshot` is on, so each truncate leaves a snapshot: `None` when
    /// the nodes couldn't say or disagree
    #[serde(default)]
    pub auto_snapshot: Option<bool>,
    /// The snapshots of the wiped tables Scylla already held when the plan was built
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub existing_snapshots: Vec<TruncateSnapshot>,
    /// The snapshots of the wiped tables the wipe's truncates left, which hold the wiped data
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<TruncateSnapshot>,
}

/// A snapshot Scylla holds of a table on one node, as `system.snapshots` lists it
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TruncateSnapshot {
    /// The node holding it, by address
    pub node: String,
    /// The table it is a snapshot of
    pub table: String,
    /// The snapshot's name (a truncate names it `<milliseconds>-<table>`)
    pub snapshot: String,
    /// The bytes on disk only the snapshot keeps
    pub live: u64,
    /// The bytes on disk the snapshot holds
    pub total: u64,
}

/// One table in a wipe plan
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WipedTable {
    /// The table name
    pub table: String,
    /// The partition estimate from `system.size_estimates` when the plan was built: 0 for a
    /// table that holds no row, and `None` for one that holds rows Scylla hasn't estimated yet
    #[serde(default)]
    pub estimated_partitions: Option<u64>,
    /// The bytes the table's rows take, estimated from `system.size_estimates` when the plan
    /// was built, which its truncate snapshot keeps; `None` when Scylla has no estimate
    #[serde(default)]
    pub estimated_bytes: Option<u64>,
    /// The views dropped before the table was truncated
    pub views_dropped: Vec<String>,
    /// Whether the `TRUNCATE` completed
    pub truncated: bool,
}

/// One bucket in a wipe plan
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WipedBucket {
    /// The bucket role from the registry (`files`, `results`, …)
    pub role: String,
    /// The bucket name from the loaded config
    pub bucket: String,
    /// The objects listed when the plan was built
    pub planned_objects: u64,
    /// The bytes listed when the plan was built
    pub planned_bytes: u64,
    /// The objects deleted so far
    pub deleted_objects: u64,
    /// The bytes deleted so far
    pub deleted_bytes: u64,
    /// Whether every listed object is gone
    pub done: bool,
}

impl WipedTable {
    /// Describes how much the table holds, as the plan estimated it
    #[must_use]
    pub fn describe_size(&self) -> String {
        // an estimate, an empty table, or rows Scylla hasn't estimated yet
        match self.estimated_partitions {
            Some(0) => "empty".to_owned(),
            Some(partitions) => format!("about {partitions} partitions"),
            None => "holds rows; partition count unknown (no estimate yet)".to_owned(),
        }
    }
}

impl WipeReport {
    /// Describes the plan, a line per table and bucket, for the confirmation and the log
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        // what will be removed
        let mut lines = vec![format!(
            "--wipe {} deletes the target's backed-up data before restoring:",
            self.namespace
        )];
        for table in &self.tables {
            lines.push(format!(
                "  - table {}.{}: {}",
                self.namespace,
                table.table,
                table.describe_size()
            ));
        }
        for bucket in &self.buckets {
            lines.push(format!(
                "  - bucket {} ({}): {} objects, {} bytes",
                bucket.bucket, bucket.role, bucket.planned_objects, bucket.planned_bytes
            ));
        }
        // the truncate snapshots the wipe leaves behind
        lines.extend(self.describe_snapshots());
        // what stays
        lines.push(format!(
            "The wipe keeps the data the backup doesn't hold: {}",
            self.kept.join(", ")
        ));
        if let Some(earlier) = &self.resumed_from {
            lines.push(format!(
                "This wipe resumes the unfinished wipe recorded in {}",
                earlier.display()
            ));
        }
        lines
    }

    /// Describes the truncate snapshots the plan's truncates leave: each table's with the size
    /// Scylla estimates, the snapshots the tables already have, what they hold, and how to clear
    /// them
    #[must_use]
    pub fn describe_snapshots(&self) -> Vec<String> {
        // with auto_snapshot off nothing is kept
        let keeps = match self.auto_snapshot {
            Some(false) => {
                return vec![
                    "Scylla's auto_snapshot is off, so the truncates leave no snapshots and the \
                     wiped rows can't be recovered from Scylla"
                        .to_owned(),
                ];
            }
            Some(true) => "keeps",
            None => "may keep (its setting couldn't be read on every node)",
        };
        let mut lines = vec![format!(
            "Scylla's auto_snapshot {keeps} a snapshot of each table the wipe truncates, on every \
             node; the snapshots hold the wiped data and its disk until they are cleared, though \
             rows written shortly before the wipe can be missing from them, and thoradm never \
             clears them:"
        )];
        for table in &self.tables {
            let size = table.estimated_bytes.map_or_else(
                || "size unknown until the truncate (no estimate yet)".to_owned(),
                |bytes| format!("about {bytes} bytes (estimated)"),
            );
            lines.push(format!(
                "  - snapshot of table {}.{}: {size}",
                self.namespace, table.table
            ));
        }
        for snapshot in &self.existing_snapshots {
            lines.push(format!("  - {}", self.describe_snapshot(snapshot)));
        }
        lines.push(self.clear_snapshots_step());
        lines
    }

    /// Describes one snapshot with its node and size
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The snapshot
    #[must_use]
    pub fn describe_snapshot(&self, snapshot: &TruncateSnapshot) -> String {
        // the name, the table, where it is, and what it holds
        format!(
            "snapshot {} of table {}.{} on node {}: {} bytes",
            snapshot.snapshot, self.namespace, snapshot.table, snapshot.node, snapshot.total
        )
    }

    /// Says how to clear the truncate snapshots once the restored cluster checks out
    #[must_use]
    pub fn clear_snapshots_step(&self) -> String {
        // the keyspace's snapshots, on every node
        format!(
            "Once `thoradm backup verify --cluster` passes, run `nodetool clearsnapshot {}` on \
             every Scylla node to free their disk; it clears every snapshot of keyspace {}",
            self.namespace, self.namespace
        )
    }

    /// Writes the report to `wipe.json` in a report directory, replacing any earlier version
    ///
    /// # Arguments
    ///
    /// * `reports` - The report directory
    /// * `sinks` - The file-system seam to write through
    pub async fn save(&self, reports: &Path, sinks: &dyn SinkFactory) -> Result<(), Error> {
        // temp file, sync, rename, so an interrupted wipe leaves the last whole report
        write_json_atomic(reports, WIPE_FILE, self, sinks).await?;
        Ok(())
    }

    /// Returns how many objects and bytes the wipe deleted across every bucket
    #[must_use]
    pub fn deleted(&self) -> (u64, u64) {
        // sum every bucket
        self.buckets
            .iter()
            .fold((0, 0), |(objects, bytes), bucket| {
                (
                    objects + bucket.deleted_objects,
                    bytes + bucket.deleted_bytes,
                )
            })
    }
}

/// Checks the namespace given to `--wipe` against the one thoradm's config names
///
/// # Arguments
///
/// * `given` - The value given to `--wipe`
/// * `conf` - Thoradm's config
pub fn check_given(given: &str, conf: &Conf) -> Result<(), String> {
    // an exact match only: the value binds the command to one namespace
    let configured = &conf.thorium.namespace;
    if given == configured {
        return Ok(());
    }
    Err(format!(
        "--wipe names namespace {given}, but thoradm's config (--cluster-conf) names namespace \
         {configured}; nothing was deleted. Pass --wipe {configured} to wipe the namespace the \
         config names, or point --cluster-conf at the config of namespace {given}"
    ))
}

/// Checks that the API thoradm talks to serves the namespace thoradm's config names, returning it
///
/// # Arguments
///
/// * `conf` - Thoradm's config
/// * `info` - The API's system info, or why it couldn't be read
pub fn check_api(conf: &Conf, info: Result<SystemInfo, String>) -> Result<String, String> {
    // the API must say which namespace it serves, and it must be thoradm's
    let configured = &conf.thorium.namespace;
    let info = info.map_err(|error| {
        format!(
            "couldn't ask the API (--ctl-conf) which namespace it serves, so the wipe of \
             namespace {configured} can't be confirmed to act on one namespace; nothing was \
             deleted: {error}"
        )
    })?;
    match info.namespace {
        Some(served) if served == *configured => Ok(served),
        Some(served) => Err(format!(
            "the API (--ctl-conf) serves namespace {served}, but thoradm's config \
             (--cluster-conf) names namespace {configured}; the Redis restore would go to {served} \
             while the tables and buckets of {configured} are wiped, so nothing was deleted. \
             Point both configs at one namespace"
        )),
        None => Err(format!(
            "the API (--ctl-conf) doesn't report the namespace it serves (it is older than this \
             thoradm), so the wipe of namespace {configured} can't be confirmed to act on one \
             namespace; nothing was deleted. Upgrade the API, or restore without --wipe"
        )),
    }
}

/// Returns whether a namespace is a keyspace name the wipe may truncate in
///
/// # Arguments
///
/// * `namespace` - The namespace
fn valid_keyspace(namespace: &str) -> bool {
    // a namespace the config rule accepts, and no Scylla system keyspace
    validate_namespace(namespace).is_ok() && !namespace.to_ascii_lowercase().starts_with("system")
}

/// Returns how many parts of a bucket name a namespace names, when the name holds the namespace
/// as a whole run of its `-`- or `.`-separated parts, reading the namespace's `_` as `-`
///
/// # Arguments
///
/// * `bucket` - The bucket name
/// * `namespace` - The namespace
fn named_parts(bucket: &str, namespace: &str) -> Option<usize> {
    // compare part runs, so `prod` doesn't match a bucket named for `prod2`
    let parts: Vec<&str> = bucket.split(['-', '.']).collect();
    let wanted: Vec<String> = namespace
        .to_ascii_lowercase()
        .split('_')
        .map(ToOwned::to_owned)
        .collect();
    let named = parts.windows(wanted.len()).any(|window| {
        window
            .iter()
            .zip(&wanted)
            .all(|(part, want)| part.eq_ignore_ascii_case(want))
    });
    named.then_some(wanted.len())
}

/// Returns the namespaces a bucket name names most fully (in the most parts), among some
/// namespaces; a bucket belongs to one namespace only when it alone is returned
///
/// # Arguments
///
/// * `bucket` - The bucket name
/// * `namespaces` - The namespaces that may own it
fn closest_namespaces<'a, I>(bucket: &str, namespaces: I) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    // every namespace the name holds, by how many parts it names
    let named: Vec<(usize, &str)> = namespaces
        .into_iter()
        .filter_map(|namespace| named_parts(bucket, namespace).map(|parts| (parts, namespace)))
        .collect();
    // the ones that name the most parts
    let most = named.iter().map(|(parts, _)| *parts).max().unwrap_or(0);
    named
        .into_iter()
        .filter(|(parts, _)| *parts == most)
        .map(|(_, namespace)| namespace)
        .collect()
}

/// Returns the keyspaces of a cluster that hold a backed-up table, by keyspace, leaving out only
/// Scylla's own keyspaces
///
/// # Arguments
///
/// * `scylla` - A session to the cluster
async fn thorium_keyspaces(scylla: &Session) -> Result<BTreeSet<String>, String> {
    // every table of every keyspace, kept when the registry backs it up
    let backed_up: BTreeSet<&str> = Registry::backed_up(Store::ScyllaTable)
        .map(|entry| entry.name)
        .collect();
    let rows = scylla
        .query_unpaged(
            "SELECT keyspace_name, table_name FROM system_schema.tables",
            &[],
        )
        .await
        .map_err(|error| format!("couldn't list the cluster's tables: {error}"))?
        .into_rows_result()
        .map_err(|error| format!("couldn't list the cluster's tables: {error}"))?;
    let keyspaces = rows
        .rows::<(String, String)>()
        .map_err(|error| format!("couldn't list the cluster's tables: {error}"))?
        .filter_map(Result::ok)
        .filter(|(keyspace, table)| {
            !SYSTEM_KEYSPACES.contains(&keyspace.as_str()) && backed_up.contains(table.as_str())
        })
        .map(|(keyspace, _)| keyspace)
        .collect();
    Ok(keyspaces)
}

/// Refuses a target the wipe can't safely delete from, naming the cause
///
/// # Arguments
///
/// * `conf` - Thoradm's config, which names the keyspace and buckets
/// * `scylla` - A session to the target's Scylla
/// * `target` - The tables and buckets the wipe would empty
pub async fn guards(conf: &Conf, scylla: &Session, target: &Target) -> Result<(), String> {
    let namespace = &conf.thorium.namespace;
    // the namespace is a keyspace the wipe may truncate in
    if !valid_keyspace(namespace) {
        return Err(format!(
            "namespace {namespace} isn't a keyspace --wipe may truncate in: it must be lowercase \
             ASCII letters (a-z), digits and underscores, start with a lowercase letter, be at \
             most {NAMESPACE_MAX_LEN} characters, and not name a Scylla system keyspace (system*)"
        ));
    }
    // the keyspace is a Thorium namespace
    let keyspaces = thorium_keyspaces(scylla).await?;
    if !keyspaces.contains(namespace) {
        return Err(format!(
            "keyspace {namespace} holds none of the tables a backup holds, so thoradm's config \
             doesn't point at a Thorium namespace; nothing was deleted"
        ));
    }
    // no wiped bucket is shared with a role the wipe doesn't empty, since object keys carry no
    // role
    let wiped: BTreeSet<&str> = target.buckets.iter().map(|bucket| bucket.role).collect();
    for bucket in &target.buckets {
        for other in CONFIG_ROLES.iter().filter(|other| **other != bucket.role) {
            if bucket_for(conf, other).as_deref() != Some(bucket.bucket.as_str()) {
                continue;
            }
            if !wiped.contains(other) {
                return Err(format!(
                    "bucket {} is named by both the {} and the {other} roles in thoradm's config, \
                     and object keys carry no role, so --wipe can't empty one role's objects \
                     without the other's; give each role its own bucket",
                    bucket.bucket, bucket.role
                ));
            }
        }
    }
    // with another Thorium namespace on the cluster, each wiped bucket must be this namespace's
    let others: Vec<&String> = keyspaces
        .iter()
        .filter(|other| *other != namespace)
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    let shown = others
        .iter()
        .map(|other| other.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    for bucket in &target.buckets {
        let default = DEFAULT_BUCKETS
            .iter()
            .any(|(role, name)| *role == bucket.role && *name == bucket.bucket);
        if default {
            return Err(format!(
                "the cluster holds other Thorium namespaces ({shown}) and the {} role uses its \
                 default bucket {}, which every namespace shares unless configured otherwise; \
                 --wipe would delete their objects too. Name each namespace's buckets for it",
                bucket.role, bucket.bucket
            ));
        }
        // the namespace the name holds in the most parts owns the bucket; a tie may be either's
        let closest = closest_namespaces(&bucket.bucket, keyspaces.iter().map(String::as_str));
        if !closest.contains(&namespace.as_str()) {
            return Err(format!(
                "the cluster holds other Thorium namespaces ({shown}) and bucket {} ({}) doesn't \
                 carry namespace {namespace} in its name, so it may be theirs; --wipe would \
                 delete their objects too. Name each namespace's buckets for it",
                bucket.bucket, bucket.role
            ));
        }
        if closest.len() > 1 {
            let rivals = closest
                .iter()
                .filter(|other| **other != namespace)
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "bucket {} ({}) carries namespace {rivals} in its name as fully as namespace \
                 {namespace}, so it may be theirs; --wipe would delete their objects too. Name \
                 each namespace's buckets so that no other namespace's name fits them as well",
                bucket.bucket, bucket.role
            ));
        }
    }
    Ok(())
}

/// Returns the partition estimate of a table from `system.size_estimates`: 0 when the table holds
/// no row, and `None` when it holds rows but has no estimate yet (a table written since the last
/// estimate)
///
/// # Arguments
///
/// * `scylla` - A session to the target's Scylla
/// * `keyspace` - The keyspace
/// * `table` - The table
async fn estimated_partitions(scylla: &Session, keyspace: &str, table: &str) -> Option<u64> {
    // the estimates of every token range, summed
    let query = "SELECT partitions_count FROM system.size_estimates \
                 WHERE keyspace_name = ? AND table_name = ?";
    let estimate: u64 = match scylla.query_unpaged(query, (keyspace, table)).await {
        Ok(result) => result
            .into_rows_result()
            .ok()
            .and_then(|rows| {
                rows.rows::<(i64,)>().ok().map(|rows| {
                    rows.filter_map(Result::ok)
                        .map(|(count,)| u64::try_from(count).unwrap_or(0))
                        .sum()
                })
            })
            .unwrap_or(0),
        Err(_) => 0,
    };
    if estimate > 0 {
        return Some(estimate);
    }
    // no estimate: one row tells an empty table from one Scylla hasn't estimated
    let probe = format!("SELECT * FROM {keyspace}.{table} LIMIT 1");
    let holds_rows = scylla
        .query_unpaged(probe, &[])
        .await
        .ok()
        .and_then(|result| result.into_rows_result().ok())
        .is_none_or(|rows| rows.rows_num() > 0);
    if holds_rows { None } else { Some(0) }
}

/// Returns the bytes a table's rows take from `system.size_estimates`, or `None` when Scylla has
/// no estimate for it
///
/// # Arguments
///
/// * `scylla` - A session to the target's Scylla
/// * `keyspace` - The keyspace
/// * `table` - The table
async fn estimated_bytes(scylla: &Session, keyspace: &str, table: &str) -> Option<u64> {
    // each token range's mean partition size times its partitions, summed
    let query = "SELECT mean_partition_size, partitions_count FROM system.size_estimates \
                 WHERE keyspace_name = ? AND table_name = ?";
    let rows = scylla
        .query_unpaged(query, (keyspace, table))
        .await
        .ok()?
        .into_rows_result()
        .ok()?;
    let bytes: u64 = rows
        .rows::<(i64, i64)>()
        .ok()?
        .filter_map(Result::ok)
        .map(|(size, count)| {
            u64::try_from(size)
                .unwrap_or(0)
                .saturating_mul(u64::try_from(count).unwrap_or(0))
        })
        .sum();
    (bytes > 0).then_some(bytes)
}

/// Builds a statement only one node answers, for Scylla's node-local system tables
///
/// # Arguments
///
/// * `query` - The query
/// * `node` - The node to ask
fn on_node(query: &str, node: &Arc<Node>) -> Statement {
    // pin the statement to the node
    let mut statement = Statement::new(query);
    statement.set_load_balancing_policy(Some(SingleTargetLoadBalancingPolicy::new(
        NodeIdentifier::Node(node.clone()),
        None,
    )));
    statement
}

/// Reads Scylla's `auto_snapshot` setting on every node: `None` when a node can't say or the
/// nodes disagree
///
/// # Arguments
///
/// * `scylla` - A session to the target's Scylla
async fn auto_snapshot(scylla: &Session) -> Option<bool> {
    // each node's own config table
    let mut found = BTreeSet::new();
    for node in scylla.get_cluster_state().get_nodes_info() {
        let statement = on_node(
            "SELECT value FROM system.config WHERE name = 'auto_snapshot'",
            node,
        );
        let (value,): (String,) = scylla
            .query_unpaged(statement, &[])
            .await
            .ok()?
            .into_rows_result()
            .ok()?
            .first_row()
            .ok()?;
        found.insert(value.trim_matches('"') == "true");
    }
    // one answer from every node, or none
    match (found.len(), found.first()) {
        (1, Some(on)) => Some(*on),
        _ => None,
    }
}

/// Lists the snapshots every node holds of a keyspace's tables, sorted
///
/// # Arguments
///
/// * `scylla` - A session to the target's Scylla
/// * `keyspace` - The keyspace
/// * `tables` - The tables whose snapshots are listed
pub async fn list_snapshots(
    scylla: &Session,
    keyspace: &str,
    tables: &[&str],
) -> Result<Vec<TruncateSnapshot>, Error> {
    // each node lists only its own snapshots
    let mut snapshots = Vec::new();
    for node in scylla.get_cluster_state().get_nodes_info() {
        let statement = on_node(
            "SELECT table_name, snapshot_name, live, total FROM system.snapshots \
             WHERE keyspace_name = ?",
            node,
        );
        let rows = scylla
            .query_unpaged(statement, (keyspace,))
            .await?
            .into_rows_result()
            .map_err(|error| Error::new(error.to_string()))?;
        let typed = rows
            .rows::<(String, String, i64, i64)>()
            .map_err(|error| Error::new(error.to_string()))?;
        for row in typed {
            let (table, snapshot, live, total) =
                row.map_err(|error| Error::new(error.to_string()))?;
            if tables.contains(&table.as_str()) {
                snapshots.push(TruncateSnapshot {
                    node: node.address.to_string(),
                    table,
                    snapshot,
                    live: u64::try_from(live).unwrap_or(0),
                    total: u64::try_from(total).unwrap_or(0),
                });
            }
        }
    }
    snapshots.sort();
    Ok(snapshots)
}

/// Lists the keys and sizes of every object of a bucket
///
/// # Arguments
///
/// * `s3` - A client for the target's S3
/// * `bucket` - The bucket
async fn held_objects(s3: &Client, bucket: &TargetBucket) -> Result<Vec<(String, u64)>, Error> {
    // page through the whole listing
    let mut found = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = s3
            .list_objects_v2()
            .bucket(&bucket.bucket)
            .set_continuation_token(token.take())
            .send()
            .await
            .map_err(|error| Error::from_s3(&error))?;
        for object in page.contents() {
            if let Some(key) = object.key() {
                let size = object
                    .size()
                    .map_or(0, |size| u64::try_from(size).unwrap_or(0));
                found.push((key.to_owned(), size));
            }
        }
        match page.next_continuation_token() {
            Some(next) if page.is_truncated() == Some(true) => token = Some(next.to_owned()),
            _ => return Ok(found),
        }
    }
}

/// Returns the excluded tables and buckets the wipe leaves, by registry name
#[must_use]
pub fn kept() -> Vec<String> {
    // the excluded entries of the stores the wipe deletes from
    Registry::excluded()
        .filter(|entry| matches!(entry.store, Store::ScyllaTable | Store::S3))
        .filter(|entry| matches!(entry.coverage, Coverage::Excluded { .. }))
        .map(|entry| entry.name.to_owned())
        .collect()
}

/// Builds the wipe plan: each table's partition estimate and each bucket's held objects and bytes
///
/// # Arguments
///
/// * `scylla` - A session to the target's Scylla
/// * `s3` - A client for the target's S3
/// * `target` - The tables and buckets the wipe empties
/// * `api_namespace` - The namespace the API reported serving
/// * `backup` - The backup being restored
/// * `resumed_from` - The report directory of an earlier unfinished wipe, if any
pub async fn plan(
    scylla: &Session,
    s3: &Client,
    target: &Target,
    api_namespace: String,
    backup: &Path,
    resumed_from: Option<PathBuf>,
) -> Result<WipeReport, Error> {
    // each table with its estimate
    let mut tables = Vec::with_capacity(target.tables.len());
    for table in &target.tables {
        tables.push(WipedTable {
            table: (*table).to_owned(),
            estimated_partitions: estimated_partitions(scylla, &target.keyspace, table).await,
            estimated_bytes: estimated_bytes(scylla, &target.keyspace, table).await,
            views_dropped: Vec::new(),
            truncated: false,
        });
    }
    // each bucket with what it holds now
    let mut buckets = Vec::with_capacity(target.buckets.len());
    for bucket in &target.buckets {
        let objects = held_objects(s3, bucket).await?;
        buckets.push(WipedBucket {
            role: bucket.role.to_owned(),
            bucket: bucket.bucket.clone(),
            planned_objects: objects.len() as u64,
            planned_bytes: objects.iter().map(|(_, size)| size).sum(),
            deleted_objects: 0,
            deleted_bytes: 0,
            done: false,
        });
    }
    // the truncate snapshots: whether the truncates leave them, and those the tables already have
    let auto_snapshot = auto_snapshot(scylla).await;
    let existing_snapshots = match list_snapshots(scylla, &target.keyspace, &target.tables).await {
        Ok(snapshots) => snapshots,
        Err(error) => {
            warn!("Couldn't list the snapshots Scylla holds of the tables to wipe: {error}");
            Vec::new()
        }
    };
    Ok(WipeReport {
        namespace: target.keyspace.clone(),
        api_namespace,
        backup: backup.to_path_buf(),
        resumed_from,
        started: Utc::now(),
        finished: None,
        tables,
        buckets,
        kept: kept(),
        auto_snapshot,
        existing_snapshots,
        snapshots: Vec::new(),
    })
}

/// Finds the report directory of the newest earlier wipe of a namespace that didn't finish: among
/// the backup's earlier restores and the directories in and beside a report directory
///
/// # Arguments
///
/// * `backup` - The backup being restored
/// * `reports` - This run's report directory
/// * `namespace` - The namespace being wiped
pub async fn find_unfinished(backup: &Path, reports: &Path, namespace: &str) -> Option<PathBuf> {
    // the directories earlier runs wrote their reports into
    let mut dirs = Vec::new();
    if let Ok(mut listing) = tokio::fs::read_dir(backup.join("reports")).await {
        while let Ok(Some(entry)) = listing.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with("restore-") {
                dirs.push(entry.path());
            }
        }
    }
    if let Some(parent) = reports.parent()
        && let Ok(mut listing) = tokio::fs::read_dir(parent).await
    {
        while let Ok(Some(entry)) = listing.next_entry().await {
            dirs.push(entry.path());
        }
    }
    // the newest unfinished wipe.json of this namespace, never this run's own
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        if dir == reports {
            continue;
        }
        let file = dir.join(WIPE_FILE);
        let Ok(bytes) = tokio::fs::read(&file).await else {
            continue;
        };
        let Ok(report) = serde_json::from_slice::<WipeReport>(&bytes) else {
            continue;
        };
        if report.finished.is_some() || report.namespace != namespace {
            continue;
        }
        let Ok(modified) = tokio::fs::metadata(&file)
            .await
            .and_then(|meta| meta.modified())
        else {
            continue;
        };
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, dir));
        }
    }
    newest.map(|(_, dir)| dir)
}

/// Runs a wipe plan against the target
pub struct Wiper<'a> {
    /// A session to the target's Scylla
    pub scylla: &'a Session,
    /// A client for the target's S3
    pub s3: &'a Client,
    /// The tables and buckets the wipe empties
    pub target: &'a Target,
    /// The restore's failure log
    pub log: &'a FailureLog,
    /// The restore's report directory, where `wipe.json` is rewritten
    pub reports: &'a Path,
    /// The file-system seam `wipe.json` is written through
    pub sinks: &'a dyn SinkFactory,
    /// How a failed truncate or delete is retried
    pub retry: RetryPolicy,
}

impl Wiper<'_> {
    /// Wipes every table, then every bucket, of the plan, returning the finished report or `None`
    /// when a failure stopped the wipe, with the failure recorded
    ///
    /// # Arguments
    ///
    /// * `report` - The plan, which records what was deleted as the wipe goes
    pub async fn run(&self, mut report: WipeReport) -> Result<Option<WipeReport>, Error> {
        // record the plan before the first delete
        report.save(self.reports, self.sinks).await?;
        // the tables first, each after its views are dropped
        for index in 0..report.tables.len() {
            if !self.wipe_table(&mut report, index).await? {
                return Ok(None);
            }
        }
        // the snapshots the truncates left, which hold the wiped rows
        self.record_snapshots(&mut report).await?;
        // then each bucket's held objects
        for index in 0..report.buckets.len() {
            if !self.wipe_bucket(&mut report, index).await? {
                return Ok(None);
            }
        }
        // the wipe is done; the restore goes on
        report.finished = Some(Utc::now());
        report.save(self.reports, self.sinks).await?;
        Ok(Some(report))
    }

    /// Records the snapshots of the wiped tables that weren't there when the plan was built, and
    /// warns about them with the command that clears them
    ///
    /// # Arguments
    ///
    /// * `report` - The report to record the snapshots in
    async fn record_snapshots(&self, report: &mut WipeReport) -> Result<(), Error> {
        // the wiped tables' snapshots now, less those the plan found
        let tables: Vec<&str> = report
            .tables
            .iter()
            .map(|table| table.table.as_str())
            .collect();
        let now = match list_snapshots(self.scylla, &self.target.keyspace, &tables).await {
            Ok(snapshots) => snapshots,
            Err(error) => {
                warn!(
                    "Couldn't list the truncate snapshots the wipe left: {error}; {}",
                    report.clear_snapshots_step()
                );
                return Ok(());
            }
        };
        report.snapshots = now
            .into_iter()
            .filter(|snapshot| {
                !report.existing_snapshots.iter().any(|existing| {
                    (&existing.node, &existing.table, &existing.snapshot)
                        == (&snapshot.node, &snapshot.table, &snapshot.snapshot)
                })
            })
            .collect();
        report.save(self.reports, self.sinks).await?;
        // each one, and how to clear them
        for snapshot in &report.snapshots {
            warn!("The wipe left {}", report.describe_snapshot(snapshot));
        }
        Ok(())
    }

    /// Drops one table's registered views and truncates it, returning whether it was truncated
    ///
    /// # Arguments
    ///
    /// * `report` - The report to record the table in
    /// * `index` - The table's place in the report
    async fn wipe_table(&self, report: &mut WipeReport, index: usize) -> Result<bool, Error> {
        // the table's keyspace and name
        let keyspace = &self.target.keyspace;
        let table = report.tables[index].table.clone();
        // drop the views first, so no table is truncated while it has views
        for view in Registry::views_of(&table) {
            let dropped = self
                .retry
                .run(|| drop_materialized_view(keyspace, view, self.scylla))
                .await;
            // a view that can't be dropped stops the wipe before the truncate
            if let Err(error) = dropped.result {
                return self
                    .table_failed(report, &table, &error, dropped.attempts)
                    .await;
            }
            info!("Dropped view {keyspace}.{view}");
            report.tables[index].views_dropped.push(view.to_owned());
        }
        // truncate, which may take long on a large table
        let mut statement = Statement::new(format!("TRUNCATE {keyspace}.{table}"));
        statement.set_request_timeout(Some(TRUNCATE_TIMEOUT));
        let truncated = self
            .retry
            .run(|| self.scylla.query_unpaged(statement.clone(), &[]))
            .await;
        // a truncate that still fails stops the wipe
        if let Err(error) = truncated.result {
            return self
                .table_failed(report, &table, &error, truncated.attempts)
                .await;
        }
        // record the truncate in wipe.json before going on
        report.tables[index].truncated = true;
        report.save(self.reports, self.sinks).await?;
        warn!(
            "Wiped table {keyspace}.{table} (planned as {})",
            report.tables[index].describe_size()
        );
        Ok(true)
    }

    /// Records a table the wipe couldn't empty and saves the report
    ///
    /// # Arguments
    ///
    /// * `report` - The report so far
    /// * `table` - The table
    /// * `error` - Why it failed
    /// * `attempts` - How many attempts were made
    async fn table_failed<E: std::fmt::Display>(
        &self,
        report: &WipeReport,
        table: &str,
        error: E,
        attempts: u32,
    ) -> Result<bool, Error> {
        // the failure, then the report without a finished time
        let record = FailureRecord::failed(
            WIPE_COMPONENT,
            Operation::Wipe,
            format!(
                "couldn't wipe table {}.{table}: {error}",
                self.target.keyspace
            ),
        )
        .table(table)
        .attempts(attempts);
        self.log.record(&record).await?;
        report.save(self.reports, self.sinks).await?;
        Ok(false)
    }

    /// Deletes every held object of one bucket in batches, returning whether all are gone
    ///
    /// # Arguments
    ///
    /// * `report` - The report to record the bucket in
    /// * `index` - The bucket's place in the report
    async fn wipe_bucket(&self, report: &mut WipeReport, index: usize) -> Result<bool, Error> {
        // the bucket the plan names, as the target knows it
        let name = report.buckets[index].bucket.clone();
        let Some(bucket) = self
            .target
            .buckets
            .iter()
            .find(|bucket| bucket.bucket == name)
        else {
            return Err(Error::new(format!(
                "bucket {name} isn't in the wipe's target"
            )));
        };
        // list what is left, which on a re-run is only what the earlier run didn't delete
        let objects = match held_objects(self.s3, bucket).await {
            Ok(objects) => objects,
            Err(error) => return self.bucket_failed(report, &name, None, &error, 1).await,
        };
        // delete in batches, retrying the keys a batch couldn't delete
        for batch in objects.chunks(DELETE_BATCH) {
            match self.delete_batch(&name, batch).await {
                Ok(()) => {
                    let entry = &mut report.buckets[index];
                    entry.deleted_objects += batch.len() as u64;
                    entry.deleted_bytes += batch.iter().map(|(_, size)| size).sum::<u64>();
                }
                Err((key, error, attempts)) => {
                    return self
                        .bucket_failed(report, &name, Some(&key), &error, attempts)
                        .await;
                }
            }
        }
        // record the bucket as emptied in wipe.json
        report.buckets[index].done = true;
        report.save(self.reports, self.sinks).await?;
        let entry = &report.buckets[index];
        warn!(
            "Wiped bucket {name} ({}): {} objects, {} bytes",
            entry.role, entry.deleted_objects, entry.deleted_bytes
        );
        Ok(true)
    }

    /// Deletes one batch of keys, retrying what wasn't deleted, and returns the first key that
    /// still couldn't be with its error and attempts
    ///
    /// # Arguments
    ///
    /// * `bucket` - The bucket
    /// * `batch` - The keys and sizes to delete
    async fn delete_batch(
        &self,
        bucket: &str,
        batch: &[(String, u64)],
    ) -> Result<(), (String, Error, u32)> {
        // every key starts pending
        let mut pending: Vec<String> = batch.iter().map(|(key, _)| key.clone()).collect();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let result = self.delete_keys(bucket, &pending).await;
            // keep the keys S3 couldn't delete, which are retried, or every key when the request
            // failed, which is retried only for a transient error
            let (left, error, retryable) = match result {
                Ok(left) if left.is_empty() => return Ok(()),
                Ok(left) => {
                    let error = Error::new(format!("S3 didn't delete {}", left[0].1));
                    (
                        left.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
                        error,
                        true,
                    )
                }
                Err(error) => {
                    let retryable = error.is_transient();
                    (pending.clone(), error, retryable)
                }
            };
            if !retryable || attempts >= self.retry.attempts {
                return Err((left[0].clone(), error, attempts));
            }
            pending = left;
            tokio::time::sleep(self.retry.delay(attempts)).await;
        }
    }

    /// Sends one `DeleteObjects` request, returning the keys it couldn't delete with the reason
    ///
    /// # Arguments
    ///
    /// * `bucket` - The bucket
    /// * `keys` - The keys to delete
    async fn delete_keys(
        &self,
        bucket: &str,
        keys: &[String],
    ) -> Result<Vec<(String, String)>, Error> {
        // one identifier per key; quiet mode lists only the failures
        let objects = keys
            .iter()
            .map(|key| ObjectIdentifier::builder().key(key).build())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| Error::new(format!("couldn't build a delete request: {error}")))?;
        let delete = Delete::builder()
            .set_objects(Some(objects))
            .quiet(true)
            .build()
            .map_err(|error| Error::new(format!("couldn't build a delete request: {error}")))?;
        // send it
        let output = self
            .s3
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await
            .map_err(|error| Error::from_s3(&error))?;
        // the keys S3 couldn't delete, each with its error code and message
        let left = output
            .errors()
            .iter()
            .map(|error| {
                (
                    error.key().unwrap_or_default().to_owned(),
                    format!(
                        "{}: {} {}",
                        error.key().unwrap_or_default(),
                        error.code().unwrap_or_default(),
                        error.message().unwrap_or_default()
                    ),
                )
            })
            .collect();
        Ok(left)
    }

    /// Records a bucket the wipe couldn't empty and saves the report
    ///
    /// # Arguments
    ///
    /// * `report` - The report so far
    /// * `bucket` - The bucket
    /// * `key` - The key that couldn't be deleted, if the failure was one key's
    /// * `error` - Why it failed
    /// * `attempts` - How many attempts were made
    async fn bucket_failed(
        &self,
        report: &WipeReport,
        bucket: &str,
        key: Option<&str>,
        error: &Error,
        attempts: u32,
    ) -> Result<bool, Error> {
        // the failure, then the report without a finished time
        let mut record = FailureRecord::failed(
            WIPE_COMPONENT,
            Operation::Wipe,
            format!("couldn't wipe bucket {bucket}: {error}"),
        )
        .bucket(bucket)
        .attempts(attempts);
        // name the key when one key failed
        if let Some(key) = key {
            record = record.key(key);
        }
        self.log.record(&record).await?;
        report.save(self.reports, self.sinks).await?;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{closest_namespaces, named_parts, valid_keyspace};

    /// A bucket names a namespace only as a whole run of its parts
    #[test]
    fn bucket_names_carry_the_namespace_as_whole_parts() {
        // whole runs match, with `_` read as `-`
        assert_eq!(
            named_parts("testing-ab12-wipe-files", "testing_ab12_wipe"),
            Some(3)
        );
        assert_eq!(named_parts("corp.prod.files", "prod"), Some(1));
        // a part that only starts with the namespace doesn't
        assert_eq!(named_parts("prod2-files", "prod"), None);
        assert_eq!(named_parts("thorium-files", "testing_ab12"), None);
    }

    /// The namespace a bucket name holds in the most parts owns it, and a tie is shared
    #[test]
    fn the_namespace_named_most_fully_owns_a_bucket() {
        // a neighbor whose name extends the target's names more parts
        assert_eq!(
            closest_namespaces("thorium-dev-files", ["thorium", "thorium_dev"]),
            vec!["thorium_dev"]
        );
        assert_eq!(
            closest_namespaces("prod-files", ["prod", "prod_eu"]),
            vec!["prod"]
        );
        // two namespaces of one part each tie
        assert_eq!(
            closest_namespaces("thorium-staging-files", ["thorium", "staging"]),
            vec!["thorium", "staging"]
        );
        // a name holding neither belongs to neither
        assert_eq!(
            closest_namespaces("other-files", ["thorium", "staging"]),
            Vec::<&str>::new()
        );
    }

    /// System keyspaces and names with other characters aren't wiped
    #[test]
    fn only_plain_non_system_keyspaces_are_wiped() {
        // lowercase letters, digits, and underscores outside system*
        assert!(valid_keyspace("thorium"));
        assert!(valid_keyspace("testing_ab12"));
        assert!(!valid_keyspace("system_schema"));
        assert!(!valid_keyspace("System"));
        assert!(!valid_keyspace("Thorium"));
        assert!(!valid_keyspace("testing_Ab12"));
        assert!(!valid_keyspace("a-b"));
        assert!(!valid_keyspace(""));
    }

    /// A table's size reads as empty, estimated, or unknown, never as 0 partitions for rows
    #[test]
    fn a_table_without_an_estimate_isnt_called_empty() {
        // the three states of a planned table
        let table = |estimated_partitions| super::WipedTable {
            table: "samples_list".to_owned(),
            estimated_partitions,
            estimated_bytes: None,
            views_dropped: Vec::new(),
            truncated: false,
        };
        assert_eq!(table(Some(0)).describe_size(), "empty");
        assert_eq!(table(Some(12)).describe_size(), "about 12 partitions");
        assert!(table(None).describe_size().contains("unknown"));
    }

    /// Builds a plan of one table with a size estimate, and an earlier snapshot of it
    ///
    /// # Arguments
    ///
    /// * `auto_snapshot` - Whether Scylla's `auto_snapshot` is on
    fn snapshot_plan(auto_snapshot: Option<bool>) -> super::WipeReport {
        // one table, estimated at 4,096 bytes, with a snapshot an earlier truncate left
        super::WipeReport {
            namespace: "thorium".to_owned(),
            api_namespace: "thorium".to_owned(),
            backup: "backup".into(),
            resumed_from: None,
            started: chrono::Utc::now(),
            finished: None,
            tables: vec![super::WipedTable {
                table: "samples_list".to_owned(),
                estimated_partitions: Some(3),
                estimated_bytes: Some(4_096),
                views_dropped: Vec::new(),
                truncated: false,
            }],
            buckets: Vec::new(),
            kept: vec!["logs".to_owned()],
            auto_snapshot,
            existing_snapshots: vec![super::TruncateSnapshot {
                node: "10.0.0.1:9042".to_owned(),
                table: "samples_list".to_owned(),
                snapshot: "1790000000000-samples_list".to_owned(),
                live: 100,
                total: 200,
            }],
            snapshots: Vec::new(),
        }
    }

    /// With `auto_snapshot` on, or unknown, the plan names each table's snapshot with its
    /// estimated size and each earlier snapshot with its size, says they hold the wiped data and
    /// its disk, and gives the command to clear them once `verify --cluster` passes; with it off,
    /// it says the truncates leave none
    #[test]
    fn the_plan_names_the_truncate_snapshots_and_how_to_clear_them() {
        // on, and unknown, name the snapshots and the command
        for auto in [Some(true), None] {
            let text = snapshot_plan(auto).describe().join("\n");
            for expected in [
                "snapshot of table thorium.samples_list: about 4096 bytes",
                "snapshot 1790000000000-samples_list of table thorium.samples_list on node \
                 10.0.0.1:9042: 200 bytes",
                "hold the wiped data and its disk",
                "thoradm backup verify --cluster",
                "nodetool clearsnapshot thorium",
            ] {
                assert!(text.contains(expected), "{auto:?}: {expected}:\n{text}");
            }
        }
        // off says nothing is kept
        let text = snapshot_plan(Some(false)).describe().join("\n");
        assert!(text.contains("auto_snapshot is off"), "{text}");
        assert!(!text.contains("nodetool clearsnapshot"), "{text}");
    }
}
