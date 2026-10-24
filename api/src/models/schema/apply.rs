//! What `thoradm schema apply` converges to, what one run did, and why it can stop, and the
//! applier itself: it creates the keyspace and `schema_migrations` first, then every missing
//! declared object in creation order, adds the table columns the keyspace lacks, waits for the
//! views to build, and records the applied fingerprints and layout versions

use chrono::Utc;
use scylla::client::session::Session;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::config_record::{self, ConfigAccept};
use super::queries::migrations::SchemaMigrationsQueries;
use super::record::{self, RecordError};
use super::scylla::{SchemaMigrationsTable, SchemaMigrationsTableRef};
use super::{
    DriftReport, Fingerprint, MigrationRecordKind, ObjectDef, ObservedSchema, RevisionSnapshot,
    SchemaDefinition, SchemaObject, ScyllaObject, SnapshotObject, probe_scylla,
};
use crate::Conf;
use crate::models::upgrades::{Catalog, RevisionId, SchemaBackend};

mod snapshot;

pub use snapshot::{SnapshotRun, apply_snapshot, table_cql, view_cql};

/// What `thoradm schema apply` converges to, and whether it runs anything
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOptions {
    /// The definitions to converge to
    pub target: ApplyTarget,
    /// Return the statements without executing them (`print`, `diff`, and `--dry-run`)
    pub dry_run: bool,
}

/// The definitions the applier converges to; either way it only creates and adds, never drops
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyTarget {
    /// The registry this build declares
    Declared,
    /// A committed revision snapshot, newer or older than the live schema (an older one re-adds
    /// what a contract step dropped)
    Snapshot(RevisionId),
}

/// What one `schema apply` run did, or would do on a dry run
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Every statement issued (or, on a dry run, that would be), in order
    pub statements: Vec<String>,
    /// The objects created
    pub created: Vec<String>,
    /// The columns and UDT fields added, as `object.column`
    pub added: Vec<String>,
    /// Table settings changed from the config, which move no fingerprint
    pub config_changes: Vec<ConfigChange>,
    /// The deployment fingerprint recorded in `schema_migrations`; the operator records this one
    /// and raises `SchemaSkew` when it differs from its own
    pub fingerprint: Fingerprint,
    /// The trigger key this run applied for (the image reference, the target revision, and the
    /// hashes of the data-shaping and TTL settings), recorded beside the fingerprint so later
    /// reconciles compare keys, not fingerprints; the files password's hash is left out
    pub trigger_key: String,
    /// The views this run created, each reported only once `system.built_views` lists it, so no
    /// reader moves to a view still being built
    #[serde(default)]
    pub views_built: Vec<String>,
    /// Warnings for the status that don't stop the apply, such as a files password left at its
    /// public default
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Whether nothing was executed
    pub dry_run: bool,
}

/// A table setting that `schema apply` brings in line with the config
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChange {
    /// The table changed
    pub table: String,
    /// The setting changed, such as `default_time_to_live`
    pub setting: String,
    /// The live value before the change
    pub from: String,
    /// The configured value applied
    pub to: String,
}

/// Why `schema apply` stopped without converging
#[derive(Debug)]
pub enum ApplyError {
    /// A data-shaping setting differs from the recorded one while its data exists
    ConfigChanged {
        /// The setting, such as `thorium.files.partition_size`
        setting: String,
        /// The recorded value (`None` for the password, which is never printed)
        recorded: Option<String>,
        /// The configured value (`None` for the password)
        configured: Option<String>,
    },
    /// `--seed-revision` named a revision while a different one is already recorded
    SeedRefused {
        /// The recorded revision
        recorded: String,
    },
    /// The live schema disagrees with the snapshot of the revision `--seed-revision` named, so it
    /// isn't recorded: an object exists but differs, or is missing and `schema apply` can't add
    /// it; objects the next apply would add, and unexpected extras, don't refuse it
    SeedMismatch {
        /// The revision named
        revision: String,
        /// The changed or unaddable objects, as `thoradm schema diff` names them
        differences: Vec<String>,
    },
    /// The revision `--seed-revision` named is below the catalog's support floor
    SeedBelowMinimum {
        /// The revision named
        revision: String,
        /// The oldest revision this build upgrades from
        minimum: String,
    },
    /// Before the first data-shaping record, sampled rows sit in buckets another partition size
    /// gives; nothing is recorded until the step is approved
    SampledSizeMismatch {
        /// The data type, such as `files`
        data_type: String,
        /// How many sampled rows sit in a bucket the configured size doesn't give
        mismatched: u64,
        /// The partition size those rows suggest, when one size explains them all
        suggested: Option<u16>,
    },
    /// A recorded or restored files-password hash isn't argon2id with `Params::default()`, so it
    /// is refused before any hash is computed
    PasswordHashParams,
    /// Several settings were refused in one run, so the operator sees every one at once; each
    /// entry is one refusal, in the order the settings were checked
    Refused(Vec<ApplyError>),
    /// The run asked for something this build can't do yet, such as converging to a revision
    /// snapshot; thoradm exits with its "not available" code, so a Job isn't retried
    Unavailable {
        /// What isn't available
        what: String,
    },
    /// A table holds the name of a declared materialized view, so the view can't be created or
    /// built until the table is dropped by hand
    ViewHeldByTable {
        /// The declared views whose names tables hold
        views: Vec<String>,
    },
    /// Views being waited for were dropped before they finished building, so their builds can
    /// never finish
    ViewVanished {
        /// The views that no longer exist
        views: Vec<String>,
    },
    /// A statement failed
    Query {
        /// The statement that failed
        statement: String,
        /// The error Scylla returned
        error: String,
    },
}

impl std::fmt::Display for ApplyError {
    /// Describe why the apply stopped, never printing a password
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::ConfigChanged {
                setting,
                recorded,
                configured,
            } => {
                // name the setting and, except for the password, both values
                match (recorded, configured) {
                    (Some(recorded), Some(configured)) => write!(
                        f,
                        "`{setting}` is {configured} but data was written with {recorded}; \
                         change it back"
                    )?,
                    _ => write!(
                        f,
                        "`{setting}` differs from the value data was written with; change it back"
                    )?,
                }
                // then say how a deliberate change goes through, where one does
                let id = config_record::setting_id(setting);
                if let Some(data_type) = id.strip_prefix("partition_size.") {
                    write!(f, ", or run the `rebucket-{data_type}` step")
                } else if id == config_record::FILES_PASSWORD {
                    write!(
                        f,
                        ", since changing it once files are stored isn't supported"
                    )
                } else {
                    Ok(())
                }
            }
            ApplyError::SeedRefused { recorded } => write!(
                f,
                "the deployment already records revision {recorded}, so no other revision is \
                 seeded"
            ),
            ApplyError::SeedMismatch {
                revision,
                differences,
            } => write!(
                f,
                "the live schema doesn't match revision {revision}'s snapshot: {}",
                differences.join(", ")
            ),
            ApplyError::SeedBelowMinimum { revision, minimum } => write!(
                f,
                "revision {revision} is below the oldest revision this build upgrades from, \
                 {minimum}"
            ),
            ApplyError::SampledSizeMismatch {
                data_type,
                mismatched,
                suggested,
            } => {
                write!(
                    f,
                    "{mismatched} sampled {data_type} rows sit in buckets the configured \
                     partition size doesn't give"
                )?;
                // name the size the rows suggest, when one explains them all
                if let Some(size) = suggested {
                    write!(f, "; they suggest a partition size of {size}")?;
                }
                // then say how to record the configured sizes anyway
                write!(
                    f,
                    "; to record the configured sizes anyway, run `thoradm schema apply \
                     --accept-partition-sizes` (the operator passes it once the schema-apply \
                     step is approved)"
                )
            }
            ApplyError::PasswordHashParams => write!(
                f,
                "the recorded files password hash isn't argon2id with the default parameters"
            ),
            ApplyError::Refused(refusals) => {
                // name every refusal, one after another
                let reasons = refusals.iter().map(ToString::to_string).collect::<Vec<_>>();
                write!(f, "{}", reasons.join("; "))
            }
            ApplyError::Unavailable { what } => write!(f, "{what} is not available yet"),
            ApplyError::ViewHeldByTable { views } => write!(
                f,
                "a table holds the name of each declared materialized view {}; drop each table \
                 by hand (after checking it holds nothing needed), then `thoradm schema apply` \
                 creates the view",
                views
                    .iter()
                    .map(|view| format!("`{view}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ApplyError::ViewVanished { views } => write!(
                f,
                "the materialized views {} were dropped while their builds were awaited; \
                 `thoradm schema apply` creates them again",
                views
                    .iter()
                    .map(|view| format!("`{view}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ApplyError::Query { statement, error } => write!(f, "`{statement}` failed: {error}"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl ApplyError {
    /// Build the error for a statement that failed
    ///
    /// # Arguments
    ///
    /// * `statement` - The statement, or what was being done
    /// * `error` - The error Scylla returned
    pub(super) fn query(statement: &str, error: impl std::fmt::Display) -> Self {
        // keep the statement and the driver's message
        ApplyError::Query {
            statement: statement.to_owned(),
            error: error.to_string(),
        }
    }
}

/// The id of the deployment's backend row
const DEPLOYMENT_ID: &str = "deployment";

/// How often to check whether the views being waited for have built
const VIEW_POLL: Duration = Duration::from_secs(1);

/// How often to report the views still building
const VIEW_REPORT: Duration = Duration::from_secs(10);

/// What `system_schema` holds for Thorium's keyspace
#[derive(Debug, Default)]
struct LiveSchema {
    /// Whether the keyspace exists
    keyspace: bool,
    /// Each table's columns, by table
    tables: BTreeMap<String, BTreeSet<String>>,
    /// Every view
    views: BTreeSet<String>,
    /// Each table's live `default_time_to_live` in seconds, by table; zero means none
    ttls: BTreeMap<String, i64>,
}

impl LiveSchema {
    /// Whether an object of this name exists, as a table or a view
    ///
    /// # Arguments
    ///
    /// * `name` - The object's name
    fn has(&self, name: &str) -> bool {
        // either kind of object takes the name
        self.tables.contains_key(name) || self.views.contains(name)
    }
}

/// One schema statement an apply issues
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    /// Create the keyspace
    Keyspace(String),
    /// Create a missing table or view
    Create {
        /// The object's name
        name: String,
        /// Whether the object is a view, whose build the apply waits for
        view: bool,
        /// The statement
        cql: String,
    },
    /// Add a missing column to a table
    Add {
        /// The table's name
        table: String,
        /// The column's name
        column: String,
        /// The statement
        cql: String,
    },
    /// Set a table's default TTL to the configured value
    Ttl {
        /// The table's name
        table: String,
        /// The live default TTL in seconds
        from: i64,
        /// The configured default TTL in seconds
        to: u64,
        /// The statement
        cql: String,
    },
}

impl Change {
    /// The statement this change issues
    fn cql(&self) -> &str {
        match self {
            Change::Keyspace(cql)
            | Change::Create { cql, .. }
            | Change::Add { cql, .. }
            | Change::Ttl { cql, .. } => cql,
        }
    }
}

/// Render the statement that creates Thorium's keyspace, with replication from the config
///
/// # Arguments
///
/// * `conf` - The Thorium config
#[must_use]
pub fn keyspace_cql(conf: &Conf) -> String {
    // the statement every Thorium release has created its keyspace with
    format!(
        "CREATE KEYSPACE IF NOT EXISTS {ns} WITH REPLICATION = \
         {{'class' : 'NetworkTopologyStrategy', 'replication_factor': {repl_factor}}}",
        ns = conf.thorium.namespace,
        repl_factor = conf.scylla.replication
    )
}

/// Remove the files password's hash from a trigger key, keeping every other segment in order
///
/// The operator's key carries an unsalted hash of the files password in a `password=<hex>`
/// segment. Only the operator's own state keeps that hash: `schema_migrations` is backed up, and
/// the run's report reaches pod logs and the termination message, so neither carries it.
///
/// # Arguments
///
/// * `key` - The trigger key, as `;`-separated `name=value` segments
#[must_use]
pub fn redact_trigger_key(key: &str) -> String {
    // keep each segment whose name isn't `password`, in any case, in its place
    key.split(';')
        .filter(|segment| {
            let name = segment.split_once('=').map_or(*segment, |(name, _)| name);
            !name.trim().eq_ignore_ascii_case("password")
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Get the Scylla objects of a schema in the order `schema apply` creates them: `schema_migrations`
/// first, then every other object in declaration order
///
/// # Arguments
///
/// * `objects` - The schema's objects
fn scylla_objects(objects: &[SchemaObject]) -> Vec<ObjectDef> {
    // keep every Scylla definition, in declaration order
    let scylla = objects
        .iter()
        .filter_map(|object| match object.definition {
            SchemaDefinition::Scylla(def) => Some(def),
            _ => None,
        })
        .collect::<Vec<_>>();
    // the record table comes first, as the schema declares it or as this build does
    let record = <SchemaMigrationsTable as ScyllaObject>::NAME;
    let first = scylla
        .iter()
        .find(|def| def.name() == record)
        .copied()
        .unwrap_or(<SchemaMigrationsTable as ScyllaObject>::DEF);
    std::iter::once(first)
        .chain(scylla.into_iter().filter(|def| def.name() != record))
        .collect()
}

/// Plan the statements that converge the live schema to a set of objects: create the keyspace
/// when it's missing, create each missing object, and add each missing table column; nothing is
/// ever dropped, and the only table option altered is a default TTL that differs from the config
///
/// # Arguments
///
/// * `conf` - The Thorium config, for the keyspace and table TTLs
/// * `live` - What the keyspace holds
/// * `objects` - The Scylla objects to converge to, in creation order
fn plan(conf: &Conf, live: &LiveSchema, objects: &[ObjectDef]) -> Vec<Change> {
    // collect the changes in the order they're issued
    let keyspace = &conf.thorium.namespace;
    let mut changes = Vec::new();
    // create the keyspace first when it doesn't exist
    if !live.keyspace {
        changes.push(Change::Keyspace(keyspace_cql(conf)));
    }
    for object in objects {
        match object {
            ObjectDef::Table(table) => match live.tables.get(table.name) {
                // a missing table is created with its configured default TTL
                None if !live.has(table.name) => {
                    let ttl = table
                        .ttl
                        .and_then(|source| conf.table_ttl(source.config_path));
                    changes.push(Change::Create {
                        name: table.name.to_owned(),
                        view: false,
                        cql: table.create_cql(keyspace, ttl),
                    });
                }
                // a view holding the table's name is drift for the probe, never replaced here
                None => (),
                // an existing table gains each declared column it lacks
                Some(columns) => {
                    for column in table.columns {
                        if columns.contains(column.name) {
                            continue;
                        }
                        if let Some(cql) = table.alter_add_cql(keyspace, column.name) {
                            changes.push(Change::Add {
                                table: table.name.to_owned(),
                                column: column.name.to_owned(),
                                cql,
                            });
                        }
                    }
                    // a configured default TTL that differs from the live one is altered
                    let configured = table
                        .ttl
                        .and_then(|source| conf.table_ttl(source.config_path));
                    if let (Some(to), Some(from)) = (configured, live.ttls.get(table.name))
                        && i64::try_from(to).ok() != Some(*from)
                    {
                        changes.push(Change::Ttl {
                            table: table.name.to_owned(),
                            from: *from,
                            to,
                            cql: format!(
                                "ALTER TABLE {keyspace}.{} WITH default_time_to_live = {to}",
                                table.name
                            ),
                        });
                    }
                }
            },
            // a missing view is created; an existing one is never altered
            ObjectDef::View(view) => {
                if !live.has(view.name) {
                    changes.push(Change::Create {
                        name: view.name.to_owned(),
                        view: true,
                        cql: view.create_cql(keyspace),
                    });
                }
            }
        }
    }
    changes
}

/// Render every statement that creates a schema of objects in an empty keyspace, in the order
/// `schema apply` issues them
///
/// # Arguments
///
/// * `conf` - The Thorium config, for the keyspace and table TTLs
/// * `objects` - The objects to render
#[must_use]
pub fn render(conf: &Conf, objects: &[SchemaObject]) -> Vec<String> {
    // plan against a keyspace that holds nothing yet
    plan(conf, &LiveSchema::default(), &scylla_objects(objects))
        .iter()
        .map(|change| change.cql().to_owned())
        .collect()
}

/// Compute the deployment fingerprint and each backend's fingerprint of a set of objects
///
/// # Arguments
///
/// * `objects` - The objects to fingerprint
#[must_use]
pub fn fingerprints(
    objects: &[SchemaObject],
) -> (Fingerprint, BTreeMap<SchemaBackend, Fingerprint>) {
    // fingerprint each object, then each backend, then the deployment
    let snapshot = objects.iter().map(SnapshotObject::of).collect::<Vec<_>>();
    let backends = RevisionSnapshot::backend_fingerprints(&snapshot);
    (
        RevisionSnapshot::deployment_fingerprint(&backends),
        backends,
    )
}

/// Run a read of text columns, failing with the statement on error
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `cql` - The statement, selecting `N` text columns
/// * `binds` - The values to bind
pub(super) async fn read_texts<const N: usize>(
    session: &Session,
    cql: &str,
    binds: &[&str],
) -> Result<Vec<[String; N]>, ApplyError> {
    // run the read and take its rows
    let rows = session
        .query_unpaged(cql, binds)
        .await
        .map_err(|err| ApplyError::query(cql, err))?
        .into_rows_result()
        .map_err(|err| ApplyError::query(cql, err))?;
    // take each row's columns as text
    let mut out = Vec::with_capacity(rows.rows_num());
    for row in rows
        .rows::<scylla::value::Row>()
        .map_err(|err| ApplyError::query(cql, err))?
    {
        // a row of text columns, each set in the schema tables
        let row = row.map_err(|err| ApplyError::query(cql, err))?;
        let mut values = row.columns.into_iter().map(|value| match value {
            Some(scylla::value::CqlValue::Text(text) | scylla::value::CqlValue::Ascii(text)) => {
                text
            }
            _ => String::new(),
        });
        out.push(std::array::from_fn(|_| values.next().unwrap_or_default()));
    }
    Ok(out)
}

/// Read what the keyspace holds from `system_schema`
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
async fn live_schema(session: &Session, keyspace: &str) -> Result<LiveSchema, ApplyError> {
    // find the keyspace itself
    let found = read_texts::<1>(
        session,
        "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
        &[keyspace],
    )
    .await?;
    if found.is_empty() {
        return Ok(LiveSchema::default());
    }
    // every table, then every view
    let mut live = LiveSchema {
        keyspace: true,
        ..LiveSchema::default()
    };
    let tables = read_texts::<1>(
        session,
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        &[keyspace],
    )
    .await?;
    for [table] in tables {
        live.tables.insert(table, BTreeSet::new());
    }
    let views = read_texts::<1>(
        session,
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
        &[keyspace],
    )
    .await?;
    live.views.extend(views.into_iter().map(|[view]| view));
    // every table's default TTL; Scylla stores no TTL as zero
    let ttls = session
        .query_unpaged(
            "SELECT table_name, default_time_to_live FROM system_schema.tables \
             WHERE keyspace_name = ?",
            (keyspace,),
        )
        .await
        .map_err(|err| ApplyError::query("read table default TTLs", err))?
        .into_rows_result()
        .map_err(|err| ApplyError::query("read table default TTLs", err))?;
    for row in ttls
        .rows::<(String, Option<i32>)>()
        .map_err(|err| ApplyError::query("read table default TTLs", err))?
    {
        // take each table's TTL, with none read as zero
        let (table, ttl) = row.map_err(|err| ApplyError::query("read table default TTLs", err))?;
        live.ttls.insert(table, i64::from(ttl.unwrap_or(0)));
    }
    // every table's columns; views' columns are listed too and skipped
    let columns = read_texts::<2>(
        session,
        "SELECT table_name, column_name FROM system_schema.columns WHERE keyspace_name = ?",
        &[keyspace],
    )
    .await?;
    for [table, column] in columns {
        if let Some(names) = live.tables.get_mut(&table) {
            names.insert(column);
        }
    }
    Ok(live)
}

/// Issue one change, accepting a column another writer added at the same moment
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `change` - The change to issue
async fn execute(session: &Session, keyspace: &str, change: &Change) -> Result<(), ApplyError> {
    // issue the statement; the driver waits for schema agreement after it
    let Err(err) = session.query_unpaged(change.cql(), ()).await else {
        return Ok(());
    };
    // an add that lost a race to another writer leaves the column in place
    if let Change::Add { table, column, .. } = change {
        let exists = read_texts::<1>(
            session,
            "SELECT column_name FROM system_schema.columns \
             WHERE keyspace_name = ? AND table_name = ? AND column_name = ?",
            &[keyspace, table.as_str(), column.as_str()],
        )
        .await?;
        if !exists.is_empty() {
            return Ok(());
        }
    }
    Err(ApplyError::query(change.cql(), err))
}

/// How far one view's build has come
#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewBuild<'a> {
    /// The view's name
    name: &'a str,
    /// Whether the view is built everywhere
    built: bool,
    /// On a cluster, how many nodes have built it and how many nodes there are; `None` on one node
    nodes: Option<(usize, usize)>,
}

/// Get the build state of a view on a single node, which lists each view it finished building
///
/// # Arguments
///
/// * `name` - The view's name
/// * `listed` - Whether `system.built_views` lists the view
fn single_node_build(name: &str, listed: bool) -> ViewBuild<'_> {
    // one node has no peers to count, so the list alone says whether the view is built
    ViewBuild {
        name,
        built: listed,
        nodes: None,
    }
}

/// Get the build state of a view on a cluster from the statuses its nodes reported
///
/// # Arguments
///
/// * `name` - The view's name
/// * `nodes` - How many nodes the cluster has
/// * `statuses` - Each reporting node's status for the view, from `view_build_status`
fn cluster_build<'a>(name: &'a str, nodes: usize, statuses: &[String]) -> ViewBuild<'a> {
    // count the nodes that finished; a node that hasn't reported yet hasn't finished
    let done = statuses
        .iter()
        .filter(|status| status.as_str() == "SUCCESS")
        .count();
    // the view is built once every node has reported and every report is a success
    ViewBuild {
        name,
        built: statuses.len() >= nodes && done == statuses.len(),
        nodes: Some((done, nodes)),
    }
}

/// Write the progress line for views still building, saying which are finished
///
/// # Arguments
///
/// * `elapsed` - How many seconds the wait has lasted
/// * `builds` - The build state of every view waited for
fn progress_line(elapsed: u64, builds: &[ViewBuild<'_>]) -> String {
    // split the views into those finished and those still building
    let built = builds.iter().filter(|build| build.built).count();
    // name each view still building, with the nodes that finished it where a cluster reports them
    let building = builds
        .iter()
        .filter(|build| !build.built)
        .map(|build| match build.nodes {
            Some((done, total)) => format!("{} ({done} of {total} nodes)", build.name),
            None => build.name.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{built} of {} views built after {elapsed}s; building: {building}",
        builds.len()
    )
}

/// Get how far each view of a list has built: on one node, from `system.built_views`; on a
/// cluster, from the status each node reported in `system_distributed.view_build_status`
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - The views to check
async fn view_builds<'a>(
    session: &Session,
    keyspace: &str,
    views: &[&'a str],
) -> Result<Vec<ViewBuild<'a>>, ApplyError> {
    // count the cluster's other nodes, since `built_views` only describes the node it's read on
    let peers = read_texts::<1>(session, "SELECT CAST(peer AS text) FROM system.peers", &[])
        .await?
        .len();
    let mut builds = Vec::with_capacity(views.len());
    for view in views {
        let build = if peers == 0 {
            // a single node lists each view it finished building
            let listed = !read_texts::<1>(
                session,
                "SELECT view_name FROM system.built_views \
                 WHERE keyspace_name = ? AND view_name = ?",
                &[keyspace, view],
            )
            .await?
            .is_empty();
            single_node_build(view, listed)
        } else {
            // a cluster reports each node's build, and every node must be done
            let statuses = read_texts::<1>(
                session,
                "SELECT status FROM system_distributed.view_build_status \
                 WHERE keyspace_name = ? AND view_name = ?",
                &[keyspace, view],
            )
            .await?
            .into_iter()
            .map(|[status]| status)
            .collect::<Vec<_>>();
            cluster_build(view, peers + 1, &statuses)
        };
        builds.push(build);
    }
    Ok(builds)
}

/// Get the views of a list that aren't built yet
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - The views to check
async fn unbuilt<'a>(
    session: &Session,
    keyspace: &str,
    views: &[&'a str],
) -> Result<Vec<&'a str>, ApplyError> {
    // keep the views whose build isn't finished everywhere
    Ok(view_builds(session, keyspace, views)
        .await?
        .into_iter()
        .filter(|build| !build.built)
        .map(|build| build.name)
        .collect())
}

/// Get the views still building that the keyspace no longer holds
///
/// # Arguments
///
/// * `builds` - The build state of every view waited for
/// * `existing` - The names of the views the keyspace holds
fn vanished_views(builds: &[ViewBuild<'_>], existing: &[[String; 1]]) -> Vec<String> {
    // a built view is done whether or not it still exists; one still building must exist to finish
    builds
        .iter()
        .filter(|build| !build.built && !existing.iter().any(|[name]| name == build.name))
        .map(|build| build.name.to_owned())
        .collect()
}

/// Wait until every listed view is built, reporting which views are built and which are still
/// building every few seconds, with no limit on the wait; the wait ends with an error only when a
/// view it waits for no longer exists
///
/// `CREATE MATERIALIZED VIEW` returns before Scylla has built the view, so a reader moved to a
/// new view would otherwise see a partial one.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - The views to wait for
/// * `progress` - Receives each progress report
pub(super) async fn wait_for_views(
    session: &Session,
    keyspace: &str,
    views: &[&str],
    progress: &(dyn Fn(&str) + Sync),
) -> Result<(), ApplyError> {
    // time the wait from here, with no report made yet
    let started = Instant::now();
    let mut reported: Option<Instant> = None;
    loop {
        // stop once nothing is left to build
        let builds = view_builds(session, keyspace, views).await?;
        if builds.iter().all(|build| build.built) {
            if reported.is_some() {
                progress(&format!(
                    "every view built after {}s: {}",
                    started.elapsed().as_secs(),
                    views.join(", ")
                ));
            }
            return Ok(());
        }
        // a view dropped meanwhile, or a keyspace dropped under it, can never finish building
        let existing = read_texts::<1>(
            session,
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
            &[keyspace],
        )
        .await?;
        let vanished = vanished_views(&builds, &existing);
        if !vanished.is_empty() {
            return Err(ApplyError::ViewVanished { views: vanished });
        }
        // report what's still building at the start and every few seconds after
        if reported.is_none_or(|at| at.elapsed() >= VIEW_REPORT) {
            progress(&progress_line(started.elapsed().as_secs(), &builds));
            reported = Some(Instant::now());
        }
        tokio::time::sleep(VIEW_POLL).await;
    }
}

/// Get the name a backend's row has in `schema_migrations`
///
/// # Arguments
///
/// * `backend` - The backend
fn backend_id(backend: SchemaBackend) -> &'static str {
    match backend {
        SchemaBackend::Scylla => "scylla",
        SchemaBackend::Elastic => "elastic",
        SchemaBackend::Redis => "redis",
        SchemaBackend::S3 => "s3",
        SchemaBackend::Unknown => "unknown",
    }
}

/// Get the layout version of each Redis family and S3 bucket a set of objects declares, by backend
///
/// # Arguments
///
/// * `objects` - The schema's objects
fn layouts(objects: &[SchemaObject]) -> BTreeMap<SchemaBackend, BTreeMap<String, u32>> {
    // group each layout's version under its backend
    let mut layouts: BTreeMap<SchemaBackend, BTreeMap<String, u32>> = BTreeMap::new();
    for object in objects {
        if let SchemaDefinition::Layout(layout) = &object.definition {
            layouts
                .entry(layout.backend)
                .or_default()
                .insert(layout.family.to_owned(), layout.version);
        }
    }
    layouts
}

/// Write one backend row of the record
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `queries` - The prepared `schema_migrations` statements
/// * `id` - The row's id
/// * `fingerprint` - The applied fingerprint
/// * `apply_key` - The trigger key, for the deployment's row
/// * `layouts` - The layout versions, for the Redis and S3 rows
async fn write_backend_row(
    session: &Session,
    queries: &SchemaMigrationsQueries,
    id: &str,
    fingerprint: &Fingerprint,
    apply_key: Option<&str>,
    layouts: &BTreeMap<String, u32>,
) -> Result<(), ApplyError> {
    // the row describes only the applied schema; every pass column stays unset
    let row = SchemaMigrationsTableRef {
        kind: &MigrationRecordKind::Backend,
        id,
        fingerprint: Some(fingerprint.as_str()),
        apply_key,
        layouts,
        revision: None,
        version: None,
        entry: None,
        result: None,
        value: None,
        status: None,
        attempt: None,
        lease: None,
        started: None,
        finished: None,
        job: None,
        backup: None,
        counts: None,
        report: None,
        updated: Utc::now(),
    };
    queries
        .insert(session, &row)
        .await
        .map_err(|err| ApplyError::query(&format!("record the {id} schema"), err))?;
    Ok(())
}

/// Write one data-shaping row of the record
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `queries` - The prepared `schema_migrations` statements
/// * `id` - The setting's record id
/// * `value` - The recorded value
async fn write_config_row(
    session: &Session,
    queries: &SchemaMigrationsQueries,
    id: &str,
    value: &str,
) -> Result<(), ApplyError> {
    // the row holds only the setting's value; every other column stays unset
    let none = BTreeMap::new();
    let row = SchemaMigrationsTableRef {
        kind: &MigrationRecordKind::Config,
        id,
        fingerprint: None,
        apply_key: None,
        layouts: &none,
        revision: None,
        version: None,
        entry: None,
        result: None,
        value: Some(value),
        status: None,
        attempt: None,
        lease: None,
        started: None,
        finished: None,
        job: None,
        backup: None,
        counts: None,
        report: None,
        updated: Utc::now(),
    };
    queries
        .insert(session, &row)
        .await
        .map_err(|err| ApplyError::query(&format!("record {id}"), err))?;
    Ok(())
}

/// Record the data-shaping settings, the applied fingerprints, and the layout versions in
/// `schema_migrations`: the config rows, one row per backend, and the deployment's row with the
/// trigger key
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `objects` - The applied schema's objects
/// * `trigger_key` - The trigger key this run applied for, if it was given one
/// * `config` - The data-shaping rows to record, as `(id, value)` pairs
async fn record(
    session: &Session,
    conf: &Conf,
    objects: &[SchemaObject],
    trigger_key: &str,
    config: &[(String, String)],
) -> Result<Fingerprint, ApplyError> {
    // prepare the record's statements in the converged keyspace
    let queries = SchemaMigrationsQueries::prepare(session, &conf.thorium.namespace)
        .await
        .map_err(|err| ApplyError::query("prepare the schema_migrations statements", err))?;
    // write the data-shaping settings first, so a recorded fingerprint implies a full record
    for (id, value) in config {
        write_config_row(session, &queries, id, value).await?;
    }
    // write each backend's fingerprint, with the layout versions of the Redis and S3 rows
    let (deployment, backends) = fingerprints(objects);
    let layouts = layouts(objects);
    let none = BTreeMap::new();
    for (backend, fingerprint) in &backends {
        let versions = layouts.get(backend).unwrap_or(&none);
        let id = backend_id(*backend);
        write_backend_row(session, &queries, id, fingerprint, None, versions).await?;
    }
    // then the deployment's row, written last so it only names a fully recorded schema
    let key = (!trigger_key.is_empty()).then_some(trigger_key);
    write_backend_row(session, &queries, DEPLOYMENT_ID, &deployment, key, &none).await?;
    Ok(deployment)
}

/// Record the data-shaping settings and a snapshot's applied fingerprints and layout versions in
/// `schema_migrations`: the config rows, one row per backend, and the deployment's row with the
/// trigger key
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `snapshot` - The applied snapshot
/// * `trigger_key` - The trigger key this run applied for, if it was given one
/// * `config` - The data-shaping rows to record, as `(id, value)` pairs
async fn record_snapshot(
    session: &Session,
    conf: &Conf,
    snapshot: &RevisionSnapshot,
    trigger_key: &str,
    config: &[(String, String)],
) -> Result<(), ApplyError> {
    // prepare the record's statements in the converged keyspace
    let queries = SchemaMigrationsQueries::prepare(session, &conf.thorium.namespace)
        .await
        .map_err(|err| ApplyError::query("prepare the schema_migrations statements", err))?;
    // write the data-shaping settings first, so a recorded fingerprint implies a full record
    for (id, value) in config {
        write_config_row(session, &queries, id, value).await?;
    }
    // each Redis family and S3 bucket role's layout version, from the snapshot's layout objects
    let mut versions: BTreeMap<SchemaBackend, BTreeMap<String, u32>> = BTreeMap::new();
    for object in &snapshot.objects {
        if let Some(layout) = object.canonical.as_object() {
            for (family, version) in layout {
                if let Some(version) = version.as_u64().and_then(|v| u32::try_from(v).ok()) {
                    versions
                        .entry(object.backend)
                        .or_default()
                        .insert(family.clone(), version);
                }
            }
        }
    }
    // write each backend's fingerprint, with the layout versions of the Redis and S3 rows
    let none = BTreeMap::new();
    for (backend, fingerprint) in &snapshot.backends {
        let layouts = match backend {
            SchemaBackend::Redis | SchemaBackend::S3 => versions.get(backend).unwrap_or(&none),
            _ => &none,
        };
        write_backend_row(
            session,
            &queries,
            backend_id(*backend),
            fingerprint,
            None,
            layouts,
        )
        .await?;
    }
    // then the deployment's row, written last so it only names a fully recorded schema
    let key = (!trigger_key.is_empty()).then_some(trigger_key);
    write_backend_row(
        session,
        &queries,
        DEPLOYMENT_ID,
        &snapshot.deployment,
        key,
        &none,
    )
    .await
}

/// Converge the live schema to the target the options name, and record what was applied,
/// refusing every changed data-shaping setting whose data exists
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `conf` - The Thorium config
/// * `options` - What to converge to, and whether to execute anything
/// * `trigger_key` - The trigger key this run applies for, recorded with the fingerprint
///   without the files password's hash
/// * `progress` - Receives a line for each progress report while views build
pub async fn apply(
    session: &Session,
    conf: &Conf,
    options: &ApplyOptions,
    trigger_key: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, ApplyError> {
    // accept nothing the data-shaping check would refuse
    let accept = ConfigAccept::default();
    apply_accepting(session, conf, options, &accept, trigger_key, progress).await
}

/// Converge the live schema to the target the options name, and record what was applied,
/// recording the data-shaping settings `accept` names instead of refusing them
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `conf` - The Thorium config
/// * `options` - What to converge to, and whether to execute anything
/// * `accept` - What to record instead of refusing
/// * `trigger_key` - The trigger key this run applies for, recorded with the fingerprint
///   without the files password's hash
/// * `progress` - Receives a line for each progress report while views build
pub async fn apply_accepting(
    session: &Session,
    conf: &Conf,
    options: &ApplyOptions,
    accept: &ConfigAccept,
    trigger_key: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, ApplyError> {
    // no revision is seeded unless the run is a fresh install
    apply_seeding(session, conf, options, accept, None, trigger_key, progress).await
}

/// The revision `schema apply` records when `schema_migrations` holds none, after probing the live
/// schema against its snapshot
#[derive(Debug, Clone)]
pub struct SeedRequest {
    /// The revision to record
    pub revision: RevisionId,
    /// Where the revision came from, written into the seed's history entry (such as
    /// `spec.upgrade.starting_revision`, or `--seed-revision` given by hand)
    pub source: String,
    /// The catalog whose support floor the revision must not be below
    pub catalog: Catalog,
}

/// Converge the live schema to the target the options name, recording the data-shaping settings
/// `accept` names instead of refusing them, and seeding the revision when a seed is requested
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `conf` - The Thorium config
/// * `options` - What to converge to, and whether to execute anything
/// * `accept` - What to record instead of refusing
/// * `seed` - The revision to record when `schema_migrations` holds none
/// * `trigger_key` - The trigger key this run applies for, recorded with the fingerprint
///   without the files password's hash
/// * `progress` - Receives a line for each progress report while views build
pub async fn apply_seeding(
    session: &Session,
    conf: &Conf,
    options: &ApplyOptions,
    accept: &ConfigAccept,
    seed: Option<&SeedRequest>,
    trigger_key: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, ApplyError> {
    match &options.target {
        // the declared schema is this build's registry
        ApplyTarget::Declared => {
            let objects = SchemaObject::declared();
            let run = RunSettings {
                dry_run: options.dry_run,
                accept,
                trigger_key,
                seed,
            };
            converge(session, conf, &objects, &run, progress).await
        }
        // a committed snapshot's own definitions are rendered and applied
        ApplyTarget::Snapshot(revision) => {
            let snapshot = match RevisionSnapshot::committed(revision.as_str()) {
                Some(Ok(snapshot)) => snapshot,
                Some(Err(err)) => {
                    return Err(ApplyError::Unavailable {
                        what: format!("converging to revision {revision}'s snapshot ({err:?})"),
                    });
                }
                None => {
                    return Err(ApplyError::Unavailable {
                        what: format!(
                            "converging to revision {revision}, which has no committed snapshot"
                        ),
                    });
                }
            };
            let run = SnapshotRun {
                dry_run: options.dry_run,
                accept: accept.clone(),
                trigger_key,
                skip: BTreeSet::new(),
            };
            apply_snapshot(session, conf, &snapshot, &run, progress).await
        }
    }
}

/// Converge the live schema to a set of objects, and record their fingerprints
///
/// Checks the data-shaping settings against the record first, then creates the keyspace and
/// `schema_migrations`, then every missing object in creation order, adds the table columns the
/// keyspace lacks, waits until every declared view is built, and records the data-shaping
/// settings the record lacks, the applied fingerprints, and the layout versions. It never drops
/// or alters an existing object, so running it again on a converged keyspace issues no schema
/// statement.
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `conf` - The Thorium config
/// * `objects` - The objects to converge to
/// * `dry_run` - Return the statements without executing them
/// * `trigger_key` - The trigger key this run applies for, recorded with the fingerprint
///   without the files password's hash
/// * `progress` - Receives a line for each progress report while views build
pub async fn apply_objects(
    session: &Session,
    conf: &Conf,
    objects: &[SchemaObject],
    dry_run: bool,
    trigger_key: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, ApplyError> {
    // accept nothing the data-shaping check would refuse
    let accept = ConfigAccept::default();
    let run = RunSettings {
        dry_run,
        accept: &accept,
        trigger_key,
        seed: None,
    };
    converge(session, conf, objects, &run, progress).await
}

/// A revision a run records once it converged
struct Seeding {
    /// Whether the revision is recorded as soon as `schema_migrations` exists, which a fresh
    /// install needs so a run that stops partway is recorded by its retry
    early: bool,
    /// The revision to record
    revision: RevisionId,
    /// The step of the history entry the revision is recorded with
    step: &'static str,
    /// What the entry says about where the revision came from
    message: String,
}

/// Convert a record error into the applier's
///
/// # Arguments
///
/// * `err` - The record error
fn record_error(err: RecordError) -> ApplyError {
    // keep what was being done and why it failed
    match err {
        RecordError::Query { what, error } => ApplyError::Query {
            statement: what,
            error,
        },
        RecordError::Recorded { revision } => ApplyError::SeedRefused { recorded: revision },
        other => ApplyError::Query {
            statement: "record the revision".to_owned(),
            error: other.to_string(),
        },
    }
}

/// Decide which revision a run records, refusing a seed that can't be recorded before anything
/// changes
///
/// A fresh install, one that creates the keyspace or finds it without tables, records the latest
/// revision. A requested seed is recorded only when no other revision is, the revision isn't below
/// the catalog's support floor, and the live schema agrees with the revision's snapshot: what the
/// apply would add or an extra object doesn't refuse it, anything that exists but differs does.
///
/// # Arguments
///
/// * `session` - The Scylla session to probe with
/// * `conf` - The Thorium config, for the keyspace
/// * `live` - What the keyspace holds before this run changes it
/// * `seed` - The revision asked for, if any
async fn plan_seed(
    session: &Session,
    conf: &Conf,
    live: &LiveSchema,
    seed: Option<&SeedRequest>,
) -> Result<Option<Seeding>, ApplyError> {
    // the revision already recorded, when the record table exists
    let recorded = if live
        .tables
        .contains_key(<SchemaMigrationsTable as ScyllaObject>::NAME)
    {
        record::recorded_revision(session, conf)
            .await
            .map_err(record_error)?
    } else {
        None
    };
    let Some(seed) = seed else {
        // a fresh install records the latest revision; a keyspace with data and no record waits
        let fresh = !live.keyspace || live.tables.is_empty();
        return Ok((recorded.is_none() && fresh).then(|| Seeding {
            early: true,
            revision: crate::models::upgrades::latest(),
            step: record::RECORD_REVISION_STEP,
            message: "fresh install".to_owned(),
        }));
    };
    // a recorded revision is never replaced by another, and the same one needs no seed
    if let Some(recorded) = recorded {
        return if recorded == seed.revision.as_str() {
            Ok(None)
        } else {
            Err(ApplyError::SeedRefused { recorded })
        };
    }
    // a keyspace without tables has no data to seed: a fresh install records the latest revision
    if !live.keyspace || live.tables.is_empty() {
        return Err(ApplyError::SeedMismatch {
            revision: seed.revision.to_string(),
            differences: vec![
                "the keyspace holds no tables, so there is no data to seed; a fresh install \
                 records the latest revision without a seed"
                    .to_owned(),
            ],
        });
    }
    // the revision must be one this build upgrades from
    if let Err(err) = seed.catalog.check_floor(&seed.revision) {
        let crate::models::upgrades::UpgradeError::BelowMinimum { minimum, .. } = err else {
            return Err(ApplyError::SeedMismatch {
                revision: seed.revision.to_string(),
                differences: vec![err.to_string()],
            });
        };
        return Err(ApplyError::SeedBelowMinimum {
            revision: seed.revision.to_string(),
            minimum: minimum.to_string(),
        });
    }
    // and its snapshot must be committed, so there is something to compare the live schema with
    let mismatch = |difference: String| ApplyError::SeedMismatch {
        revision: seed.revision.to_string(),
        differences: vec![difference],
    };
    let snapshot = match RevisionSnapshot::committed(seed.revision.as_str()) {
        Some(Ok(snapshot)) => snapshot,
        Some(Err(err)) => {
            return Err(mismatch(format!(
                "its committed snapshot is unusable: {err:?}"
            )));
        }
        None => return Err(mismatch("no snapshot is committed for it".to_owned())),
    };
    // the live schema must agree with the snapshot
    if live.keyspace {
        let (objects, probed) = probe_scylla(session, &conf.thorium.namespace)
            .await
            .map_err(|err| ApplyError::query("probe the live schema", err))?;
        let observed = ObservedSchema::new(objects, &probed);
        let report = DriftReport::compare(&snapshot, &observed, &record::is_registered_legacy);
        let differences = record::seed_differences(&report);
        if !differences.is_empty() {
            return Err(ApplyError::SeedMismatch {
                revision: seed.revision.to_string(),
                differences,
            });
        }
    }
    Ok(Some(Seeding {
        early: false,
        revision: seed.revision.clone(),
        step: record::SEED_REVISION_STEP,
        message: format!("seeded from {}", seed.source),
    }))
}

/// How one apply runs: whether it executes, what it accepts, and the key it records
struct RunSettings<'a> {
    /// Return the statements without executing them
    dry_run: bool,
    /// What to record instead of refusing
    accept: &'a ConfigAccept,
    /// The trigger key this run applies for
    trigger_key: &'a str,
    /// The revision to record when none is recorded
    seed: Option<&'a SeedRequest>,
}

/// Pick the views a stopped earlier run left building: unbuilt views that exist and that this run
/// didn't create
///
/// # Arguments
///
/// * `unbuilt` - The declared views not yet built, whether or not they exist
/// * `created` - The views this run created
/// * `existing` - The names of the views the keyspace holds, one per row
pub(super) fn left_building<'a>(
    unbuilt: &[&'a str],
    created: &[&str],
    existing: &[[String; 1]],
) -> Vec<&'a str> {
    // drop the views this run created, which are waited for anyway, and the views that don't exist
    unbuilt
        .iter()
        .copied()
        .filter(|view| !created.contains(view))
        .filter(|view| existing.iter().any(|[name]| name == view))
        .collect()
}

/// Wait for the views a run created and for any a stopped earlier run left building, and return
/// the names of the views waited for
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - Every view the run's schema holds
/// * `changes` - The changes the run issued
/// * `progress` - Receives a line for each progress report
async fn wait_for_changed_views(
    session: &Session,
    keyspace: &str,
    views: &[&str],
    changes: &[Change],
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<String>, ApplyError> {
    // the views this run created
    let created = changes
        .iter()
        .filter_map(|change| match change {
            Change::Create {
                name, view: true, ..
            } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    // the views a stopped earlier run left building, which must still exist: a declared view the
    // keyspace lacks, because a table holds its name, never builds and drift reports it instead
    let pending = unbuilt(session, keyspace, views).await?;
    let existing = read_texts::<1>(
        session,
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
        &[keyspace],
    )
    .await?;
    let waiting = left_building(&pending, &created, &existing);
    // wait for both kinds
    let waited = views
        .iter()
        .copied()
        .filter(|view| created.contains(view) || waiting.contains(view))
        .collect::<Vec<_>>();
    wait_for_views(session, keyspace, &waited, progress).await?;
    Ok(waited.into_iter().map(ToOwned::to_owned).collect())
}

/// List the declared views whose names tables hold in the live schema
///
/// # Arguments
///
/// * `live` - What the keyspace holds
/// * `objects` - The Scylla objects to converge to
fn views_held_by_tables(live: &LiveSchema, objects: &[ObjectDef]) -> Vec<String> {
    // a view is held when a table of its name exists
    objects
        .iter()
        .filter_map(|object| match object {
            ObjectDef::View(view) if live.tables.contains_key(view.name) => {
                Some(view.name.to_owned())
            }
            _ => None,
        })
        .collect()
}

/// Converge the live schema to a set of objects after the data-shaping check, and record them
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `conf` - The Thorium config
/// * `objects` - The objects to converge to
/// * `run` - Whether to execute, what to accept, and the trigger key
/// * `progress` - Receives a line for each progress report
async fn converge(
    session: &Session,
    conf: &Conf,
    objects: &[SchemaObject],
    run: &RunSettings<'_>,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, ApplyError> {
    // whether this run only plans
    let dry_run = run.dry_run;
    // report and record the key without the files password's hash
    let trigger_key = redact_trigger_key(run.trigger_key);
    // plan against what the keyspace holds now
    let keyspace = conf.thorium.namespace.to_lowercase();
    let scylla = scylla_objects(objects);
    let live = live_schema(session, &keyspace).await?;
    // decide whether this run records a revision, refusing before anything changes
    let seeding = plan_seed(session, conf, &live, run.seed).await?;
    // check the data-shaping settings against the record before changing anything
    let has_table = |table: &str| live.tables.contains_key(table);
    let config = config_record::plan(
        session, conf, &scylla, &has_table, run.accept, dry_run, progress,
    )
    .await?;
    let changes = plan(conf, &live, &scylla);
    let mut report = ApplyReport {
        statements: changes
            .iter()
            .map(|change| change.cql().to_owned())
            .collect(),
        created: Vec::new(),
        added: Vec::new(),
        config_changes: Vec::new(),
        fingerprint: fingerprints(objects).0,
        trigger_key: trigger_key.clone(),
        views_built: Vec::new(),
        warnings: config.warnings.clone(),
        dry_run,
    };
    for change in &changes {
        match change {
            Change::Keyspace(_) => (),
            Change::Create { name, .. } => report.created.push(name.clone()),
            Change::Add { table, column, .. } => report.added.push(format!("{table}.{column}")),
            Change::Ttl {
                table, from, to, ..
            } => report.config_changes.push(ConfigChange {
                table: table.clone(),
                setting: "default_time_to_live".to_owned(),
                from: from.to_string(),
                to: to.to_string(),
            }),
        }
    }
    // a dry run stops before changing anything
    if dry_run {
        return Ok(report);
    }
    // refuse before changing anything when a table holds a declared view's name, since the view
    // can't be created or built until the table is dropped
    let held = views_held_by_tables(&live, &scylla);
    if !held.is_empty() {
        return Err(ApplyError::ViewHeldByTable { views: held });
    }
    // issue each change in order
    for change in &changes {
        execute(session, &keyspace, change).await?;
        // a fresh install is recorded as soon as its record table exists, before the rest of the
        // schema, so a run that stops partway leaves a record its retry finds
        if let (Some(seeding), Change::Create { name, .. }) = (&seeding, change)
            && seeding.early
            && *name == <SchemaMigrationsTable as ScyllaObject>::NAME
        {
            record::record_seed(
                session,
                conf,
                &seeding.revision,
                seeding.step,
                &seeding.message,
            )
            .await
            .map_err(record_error)?;
        }
    }
    // wait for the views this run created, and for any a stopped earlier run left building
    let views = scylla
        .iter()
        .filter_map(|def| match def {
            ObjectDef::View(view) => Some(view.name),
            ObjectDef::Table(_) => None,
        })
        .collect::<Vec<_>>();
    report.views_built =
        wait_for_changed_views(session, &keyspace, &views, &changes, progress).await?;
    // record what was applied
    report.fingerprint = record(session, conf, objects, &trigger_key, &config.rows).await?;
    // a requested seed is recorded last, so a stopped run records none and seeds again
    if let Some(seeding) = seeding.as_ref().filter(|seeding| !seeding.early) {
        record::record_seed(
            session,
            conf,
            &seeding.revision,
            seeding.step,
            &seeding.message,
        )
        .await
        .map_err(record_error)?;
    }
    Ok(report)
}

/// Read the deployment fingerprint the keyspace was applied at
///
/// Returns `None` when the keyspace, `schema_migrations`, or the deployment's row doesn't exist,
/// which means no `schema apply` has recorded anything. It reads only and issues no DDL.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
pub(super) async fn applied_fingerprint(
    session: &Session,
    conf: &Conf,
) -> Result<Option<String>, ApplyError> {
    // the record table must exist before it can be read
    let live = live_schema(session, &conf.thorium.namespace.to_lowercase()).await?;
    if !live
        .tables
        .contains_key(<SchemaMigrationsTable as ScyllaObject>::NAME)
    {
        return Ok(None);
    }
    // read every backend row of the record
    let queries = SchemaMigrationsQueries::prepare(session, &conf.thorium.namespace)
        .await
        .map_err(|err| ApplyError::query("prepare the schema_migrations statements", err))?;
    let rows = queries
        .list(session, &MigrationRecordKind::Backend)
        .await
        .map_err(|err| ApplyError::query("read the applied schema", err))?;
    // the deployment's row names the fingerprint of the whole schema
    for row in rows {
        let row = row.map_err(|err| ApplyError::query("read the applied schema", err))?;
        if row.id == DEPLOYMENT_ID {
            return Ok(row.fingerprint);
        }
    }
    Ok(None)
}

/// Check the configured data-shaping settings against the record, changing nothing
///
/// This is the check `schema apply` makes before it converges, run without recording anything:
/// a partition size or files password that differs from the record while its data exists is
/// refused, naming every such setting.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config
pub(super) async fn check_config(session: &Session, conf: &Conf) -> Result<(), ApplyError> {
    // read what the keyspace holds and the Scylla objects this build declares
    let live = live_schema(session, &conf.thorium.namespace.to_lowercase()).await?;
    let scylla = scylla_objects(&SchemaObject::declared());
    // run the check as a dry run and drop what it would have recorded
    let has_table = |table: &str| live.tables.contains_key(table);
    let accept = ConfigAccept::default();
    config_record::plan(session, conf, &scylla, &has_table, &accept, true, &|_| ()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ViewBuild, cluster_build, progress_line, single_node_build, vanished_views};

    /// Collect statuses from literals
    fn statuses(listed: &[&str]) -> Vec<String> {
        listed.iter().map(|status| (*status).to_owned()).collect()
    }

    /// A single node's views are built when `built_views` lists them, and carry no node counts
    #[test]
    fn single_node_builds_follow_the_built_views_list() {
        // a listed view is built and an unlisted one is not
        let done = single_node_build("samples", true);
        assert!(done.built);
        assert_eq!(done.nodes, None);
        let building = single_node_build("tags_by_item", false);
        assert!(!building.built);
        assert_eq!(building.nodes, None);
    }

    /// A cluster's view is built only once every node reported success, and the count says how many did
    #[test]
    fn cluster_builds_count_the_nodes_that_finished() {
        // every node reported success
        let done = cluster_build("samples", 3, &statuses(&["SUCCESS", "SUCCESS", "SUCCESS"]));
        assert!(done.built);
        assert_eq!(done.nodes, Some((3, 3)));
        // one node still building
        let partial = cluster_build("samples", 3, &statuses(&["SUCCESS", "STARTED", "SUCCESS"]));
        assert!(!partial.built);
        assert_eq!(partial.nodes, Some((2, 3)));
        // a node that has not reported yet leaves the view unbuilt
        let silent = cluster_build("samples", 3, &statuses(&["SUCCESS", "SUCCESS"]));
        assert!(!silent.built);
        assert_eq!(silent.nodes, Some((2, 3)));
        // no node has reported
        let none = cluster_build("samples", 3, &[]);
        assert!(!none.built);
        assert_eq!(none.nodes, Some((0, 3)));
    }

    /// On one node the line says how many views are built and names those still building
    #[test]
    fn the_line_splits_finished_from_building_on_one_node() {
        let builds = [
            single_node_build("a", true),
            single_node_build("b", false),
            single_node_build("c", true),
            single_node_build("d", false),
            single_node_build("e", true),
        ];
        assert_eq!(
            progress_line(12, &builds),
            "3 of 5 views built after 12s; building: b, d"
        );
    }

    /// On a cluster each view still building shows how many nodes have finished it
    #[test]
    fn the_line_shows_node_counts_on_a_cluster() {
        let builds = [
            cluster_build("a", 3, &statuses(&["SUCCESS", "SUCCESS", "SUCCESS"])),
            cluster_build("b", 3, &statuses(&["SUCCESS", "STARTED", "STARTED"])),
            cluster_build("c", 3, &[]),
        ];
        assert_eq!(
            progress_line(0, &builds),
            "1 of 3 views built after 0s; building: b (1 of 3 nodes), c (0 of 3 nodes)"
        );
    }

    /// Only a view still building that the keyspace no longer holds counts as vanished
    #[test]
    fn only_unbuilt_views_missing_from_the_keyspace_have_vanished() {
        let builds = [
            single_node_build("built_and_kept", true),
            single_node_build("built_and_dropped", true),
            single_node_build("building_and_kept", false),
            single_node_build("building_and_dropped", false),
        ];
        let existing = [
            ["built_and_kept".to_owned()],
            ["building_and_kept".to_owned()],
        ];
        assert_eq!(vanished_views(&builds, &existing), ["building_and_dropped"]);
        // a dropped keyspace holds no views, so everything still building has vanished
        assert_eq!(
            vanished_views(&builds, &[]),
            ["building_and_kept", "building_and_dropped"]
        );
    }

    /// A line with nothing built says so rather than leaving the count out
    #[test]
    fn the_line_reports_zero_built() {
        let builds: Vec<ViewBuild<'_>> = vec![single_node_build("a", false)];
        assert_eq!(
            progress_line(30, &builds),
            "0 of 1 views built after 30s; building: a"
        );
    }
}
