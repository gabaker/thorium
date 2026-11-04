//! What `thoradm schema apply` converges to, what one run did, and why it can stop, and the
//! applier itself: it creates the keyspace and `schema_migrations` first, then every missing
//! declared object in creation order, adds the table columns the keyspace lacks, drops and
//! creates again each declared view whose live definition differs from this build's, waits for
//! the views to build, and records the applied fingerprints and layout versions

use chrono::Utc;
use scylla::client::session::Session;
use scylla::statement::Consistency;
use scylla::statement::unprepared::Statement;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::config_record::{self, ConfigAccept};
use super::queries::migrations::SchemaMigrationsQueries;
use super::record::{self, RecordError};
use super::scylla::{SchemaMigrationsTable, SchemaMigrationsTableRef};
use super::views::{StaleView, ViewComparison, recreate_statements, stale_views};
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
    /// The longest the run waits for views to build before failing with the views and hosts
    /// still building (`--view-wait-secs`); `None` waits for as long as the builds take
    pub view_wait: Option<Duration>,
}

/// The definitions the applier converges to: the declared schema creates and adds, and replaces
/// only a declared view whose definition differs, while a snapshot only creates and adds
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
    /// The views this run created or recreated, each reported only once every node has built it,
    /// so no reader moves to a view still being built
    #[serde(default)]
    pub views_built: Vec<String>,
    /// The declared views this run dropped and created again because their live definitions
    /// differed from this build's, with how each differed (on a dry run, the views it would)
    #[serde(default)]
    pub views_recreated: Vec<StaleView>,
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
    /// A materialized view holds the name of a declared table, so the table can't be created
    /// until the view is dropped by hand
    TableHeldByView {
        /// The declared tables whose names views hold
        tables: Vec<String>,
    },
    /// Views being waited for were dropped before they finished building, so their builds can
    /// never finish
    ViewVanished {
        /// The views that no longer exist
        views: Vec<String>,
    },
    /// Views being waited for didn't finish building within the wait's limit, so nothing was
    /// recorded
    ViewWaitExpired {
        /// How long the wait lasted, in seconds
        waited: u64,
        /// Each view still building, with the hosts that haven't reported it built
        stuck: Vec<StuckView>,
    },
    /// Scylla still reported dropped views as built when the wait's limit passed, so they weren't
    /// created again: a view created under the name meanwhile would be taken as built
    DropUnfinished {
        /// How long the wait lasted, in seconds
        waited: u64,
        /// The dropped views Scylla still reported as built
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
    // one arm per error reads as the list of reasons an apply stops
    #[allow(clippy::too_many_lines)]
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
                quoted(views)
            ),
            ApplyError::TableHeldByView { tables } => write!(
                f,
                "a materialized view holds the name of each declared table {}; drop each view \
                 by hand (after checking nothing needs it), then `thoradm schema apply` creates \
                 the table",
                quoted(tables)
            ),
            ApplyError::ViewVanished { views } => write!(
                f,
                "the materialized views {} were dropped while their builds were awaited; \
                 `thoradm schema apply` creates them again",
                quoted(views)
            ),
            ApplyError::ViewWaitExpired { waited, stuck } => {
                write!(f, "{}", StuckView::expired(*waited, stuck))
            }
            ApplyError::DropUnfinished { waited, views } => write!(
                f,
                "Scylla still reported the dropped views {} as built after {waited}s, so they \
                 weren't created again (a busy Scylla clears a dropped view's build state late; \
                 a longer `--view-wait-secs` gives it time)",
                quoted(views)
            ),
            ApplyError::Query { statement, error } => write!(f, "`{statement}` failed: {error}"),
        }
    }
}

/// A view still building when the wait for it ended, with the hosts that haven't built it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckView {
    /// The view's name
    pub view: String,
    /// The cluster members that haven't reported the view built, each as its address and host
    /// id; empty on a single node, where `system.built_views` doesn't list the view yet
    pub hosts: Vec<String>,
}

impl StuckView {
    /// Describe a view wait that reached its limit, naming each view still building and the hosts
    /// it waits on, and saying what to do
    ///
    /// # Arguments
    ///
    /// * `waited` - How long the wait lasted, in seconds
    /// * `stuck` - Each view still building
    fn expired(waited: u64, stuck: &[StuckView]) -> String {
        // name each view still building and the hosts it waits on
        let views = stuck
            .iter()
            .map(StuckView::describe)
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "the materialized views didn't finish building within {waited}s, so nothing was \
             recorded: {views}. A Scylla node that is down or still building holds them: bring it \
             back, or remove it from the cluster, then run `thoradm schema apply` again (with a \
             longer `--view-wait-secs` if the builds are only slow)"
        )
    }

    /// Describe the view and the hosts it waits on, for an error message
    fn describe(&self) -> String {
        // a single node has no hosts to name, only its own list of built views
        if self.hosts.is_empty() {
            format!("`{}` (not yet in system.built_views)", self.view)
        } else {
            format!("`{}` (not built on {})", self.view, self.hosts.join(", "))
        }
    }
}

/// Join object names in backticks, separated by commas, for an error message
///
/// # Arguments
///
/// * `names` - The names to join
fn quoted(names: &[String]) -> String {
    // quote each name, then join them
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
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

/// How often to check whether Scylla has cleared a dropped view's build state, which is usually
/// gone within moments of the drop
const DROP_POLL: Duration = Duration::from_millis(200);

/// How long `thoradm schema apply` waits for views to build unless `--view-wait-secs` says
/// otherwise, in seconds: five hours, which leaves the operator's six hour schema-apply Job time to
/// fail with the stuck views and hosts instead of being killed without a summary
pub const DEFAULT_VIEW_WAIT_SECS: u64 = 5 * 3600;

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
    /// Drop a declared view whose live definition differs from this build's and create it again,
    /// which the apply then waits for like a created view
    Recreate {
        /// The view and how its live definition differs
        stale: StaleView,
        /// The view's base table, which its build scans
        base: String,
        /// The statement that drops the view
        drop: String,
        /// The statement that creates it from the registry
        create: String,
    },
}

impl Change {
    /// The statements this change issues, in order
    fn statements(&self) -> Vec<&str> {
        match self {
            Change::Keyspace(cql)
            | Change::Create { cql, .. }
            | Change::Add { cql, .. }
            | Change::Ttl { cql, .. } => vec![cql],
            Change::Recreate { drop, create, .. } => vec![drop, create],
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
/// when it's missing, create each missing object, add each missing table column, and drop and
/// create again each view listed as stale; nothing else is ever dropped, and the only table
/// option altered is a default TTL that differs from the config
///
/// # Arguments
///
/// * `conf` - The Thorium config, for the keyspace and table TTLs
/// * `live` - What the keyspace holds
/// * `objects` - The Scylla objects to converge to, in creation order
/// * `stale` - The existing views whose definitions differ from their declarations
fn plan(conf: &Conf, live: &LiveSchema, objects: &[ObjectDef], stale: &[StaleView]) -> Vec<Change> {
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
            // a missing view is created, and an existing view whose definition differs is
            // dropped and created again, after its base table in creation order
            ObjectDef::View(view) => {
                if !live.has(view.name) {
                    changes.push(Change::Create {
                        name: view.name.to_owned(),
                        view: true,
                        cql: view.create_cql(keyspace),
                    });
                } else if let Some(found) = stale.iter().find(|found| found.name == view.name)
                    && live.views.contains(view.name)
                {
                    let [drop, create] = recreate_statements(keyspace, view);
                    changes.push(Change::Recreate {
                        stale: found.clone(),
                        base: view.base.to_owned(),
                        drop,
                        create,
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
    plan(conf, &LiveSchema::default(), &scylla_objects(objects), &[])
        .iter()
        .flat_map(Change::statements)
        .map(ToOwned::to_owned)
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
/// * `started` - When the run's view wait started counting, for a recreated view's drop
/// * `limit` - The longest the run waits for views from `started`; `None` waits for as long as
///   they take
async fn execute(
    session: &Session,
    keyspace: &str,
    change: &Change,
    started: Instant,
    limit: Option<Duration>,
) -> Result<(), ApplyError> {
    // a view is dropped, then created again only once Scylla has cleared the dropped view's build
    // state, which the build wait would otherwise take as the new view's; a run that stops after
    // the drop leaves the view missing, which the next apply creates
    if let Change::Recreate {
        stale,
        drop,
        create,
        ..
    } = change
    {
        session
            .query_unpaged(drop.as_str(), ())
            .await
            .map_err(|err| ApplyError::query(drop, err))?;
        wait_for_drop(session, keyspace, &stale.name, started, limit).await?;
        session
            .query_unpaged(create.as_str(), ())
            .await
            .map_err(|err| ApplyError::query(create, err))?;
        return Ok(());
    }
    // issue the change's one statement; the driver waits for schema agreement after it
    let statement = change.statements().first().copied().unwrap_or_default();
    let Err(err) = session.query_unpaged(statement, ()).await else {
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
    Err(ApplyError::query(statement, err))
}

/// How far one view's build has come
#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewBuild<'a> {
    /// The view's name
    name: &'a str,
    /// Whether the view is built everywhere
    built: bool,
    /// On a cluster, how many member nodes have built it and how many members there are; `None`
    /// on one node
    nodes: Option<(usize, usize)>,
    /// On a cluster, the members that haven't reported the view built, each as its address and
    /// host id
    waiting: Vec<String>,
}

/// One node's build status for a view, as `system_distributed.view_build_status` holds it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatus {
    /// The host id of the node that wrote the row
    pub host_id: String,
    /// The node's status for the view, such as `STARTED` or `SUCCESS`
    pub status: String,
}

/// How a view's build stands across the cluster's members
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberBuild {
    /// The members that reported the view built, each as its address and host id
    pub built: Vec<String>,
    /// The members that haven't reported the view built, each as its address and host id
    pub waiting: Vec<String>,
    /// The host ids with a status row for the view that aren't cluster members (a removed or
    /// replaced node), whose rows are ignored
    pub ignored: Vec<String>,
}

/// Judge a view's build from its status rows, counting only the cluster's members: a member is
/// done once its own row says `SUCCESS`, and a row left by a host that is no longer a member
/// (a removed or replaced node) neither holds the view back nor counts toward it
///
/// # Arguments
///
/// * `hosts` - The cluster's members, by host id, each named by its address and host id
/// * `statuses` - The view's status rows, from `view_build_status`
#[must_use]
pub fn member_build(hosts: &BTreeMap<String, String>, statuses: &[HostStatus]) -> MemberBuild {
    // split the members by whether their own row reports success
    let mut build = MemberBuild::default();
    for (id, name) in hosts {
        let done = statuses
            .iter()
            .any(|row| &row.host_id == id && row.status == "SUCCESS");
        if done {
            build.built.push(name.clone());
        } else {
            build.waiting.push(name.clone());
        }
    }
    // name the hosts whose rows were set aside, once each
    build.ignored = statuses
        .iter()
        .filter(|row| !hosts.contains_key(&row.host_id))
        .map(|row| row.host_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    build
}

/// Get the cluster's members by host id, each named by its address and host id
///
/// The driver reads them from `system.local` and `system.peers` on one connection, so the list
/// is consistent; it keeps a member that is down and drops one removed from the cluster.
///
/// # Arguments
///
/// * `session` - The Scylla session whose view of the cluster to read
#[must_use]
pub fn cluster_hosts(session: &Session) -> BTreeMap<String, String> {
    // name each member by its address, with the host id its status rows carry
    session
        .get_cluster_state()
        .get_nodes_info()
        .iter()
        .map(|node| {
            let id = node.host_id.to_string();
            let name = format!("{} ({id})", node.address.ip());
            (id, name)
        })
        .collect()
}

/// Read each node's build status for a view from `system_distributed.view_build_status`
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The view's keyspace, as Scylla stores its name
/// * `view` - The view
pub async fn view_build_statuses(
    session: &Session,
    keyspace: &str,
    view: &str,
) -> Result<Vec<HostStatus>, ApplyError> {
    // each node writes its own row, so one replica answers; the keyspace's replication factor of
    // three is more than a small cluster can give a quorum read
    let cql = "SELECT CAST(host_id AS text), status FROM system_distributed.view_build_status \
               WHERE keyspace_name = ? AND view_name = ?";
    let mut statement = Statement::new(cql);
    statement.set_consistency(Consistency::One);
    let rows = session
        .query_unpaged(statement, (keyspace, view))
        .await
        .map_err(|err| ApplyError::query(cql, err))?
        .into_rows_result()
        .map_err(|err| ApplyError::query(cql, err))?;
    // keep each row's host and status
    rows.rows::<(String, String)>()
        .map_err(|err| ApplyError::query(cql, err))?
        .map(|row| {
            row.map(|(host_id, status)| HostStatus { host_id, status })
                .map_err(|err| ApplyError::query(cql, err))
        })
        .collect()
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
        waiting: Vec::new(),
    }
}

/// Get the build state of a view on a cluster from the statuses its members reported, ignoring
/// rows of hosts that are no longer members
///
/// # Arguments
///
/// * `name` - The view's name
/// * `hosts` - The cluster's members, by host id, each named by its address and host id
/// * `statuses` - The view's status rows, from `view_build_status`
fn cluster_build<'a>(
    name: &'a str,
    hosts: &BTreeMap<String, String>,
    statuses: &[HostStatus],
) -> ViewBuild<'a> {
    // judge the members' rows alone
    let build = member_build(hosts, statuses);
    // the view is built once every member has reported success
    ViewBuild {
        name,
        built: build.waiting.is_empty(),
        nodes: Some((build.built.len(), hosts.len())),
        waiting: build.waiting,
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
    // name each view still building, with the nodes that finished it and the members it waits
    // on where a cluster reports them
    let building = builds
        .iter()
        .filter(|build| !build.built)
        .map(|build| match build.nodes {
            Some((done, total)) => format!(
                "{} ({done} of {total} nodes, waiting on {})",
                build.name,
                build.waiting.join(", ")
            ),
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
/// cluster, from the status each member reported in `system_distributed.view_build_status`
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
    // the members whose rows count, read once for every view
    let hosts = if peers == 0 {
        BTreeMap::new()
    } else {
        cluster_hosts(session)
    };
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
            // a cluster reports each node's build, and every member must be done
            let statuses = view_build_statuses(session, keyspace, view).await?;
            cluster_build(view, &hosts, &statuses)
        };
        builds.push(build);
    }
    Ok(builds)
}

/// Whether every node has finished building each of a list of views, so reads of them see every
/// row; a view still building anywhere returns partial results
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace
/// * `views` - The views to check
pub async fn views_built(
    session: &Session,
    keyspace: &str,
    views: &[&str],
) -> Result<bool, ApplyError> {
    // every listed view must be built everywhere
    Ok(unbuilt(session, &keyspace.to_lowercase(), views)
        .await?
        .is_empty())
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

/// Get the error for a wait that reached its limit with views still building, or `None` while
/// the wait has time left or has no limit
///
/// # Arguments
///
/// * `builds` - The build state of every view waited for
/// * `elapsed` - How long the wait has lasted
/// * `limit` - The longest the wait may last, if it has a limit
fn wait_expired(
    builds: &[ViewBuild<'_>],
    elapsed: Duration,
    limit: Option<Duration>,
) -> Option<ApplyError> {
    // a wait with no limit, or with time left, carries on
    if limit.is_none_or(|limit| elapsed < limit) {
        return None;
    }
    // name each view still building and the members it waits on
    let stuck = builds
        .iter()
        .filter(|build| !build.built)
        .map(|build| StuckView {
            view: build.name.to_owned(),
            hosts: build.waiting.clone(),
        })
        .collect::<Vec<_>>();
    Some(ApplyError::ViewWaitExpired {
        waited: elapsed.as_secs(),
        stuck,
    })
}

/// Wait until every listed view is built, reporting which views are built and which are still
/// building every few seconds; the wait ends with an error when a view it waits for no longer
/// exists, or when the limit passes with views still building
///
/// `CREATE MATERIALIZED VIEW` returns before Scylla has built the view, so a reader moved to a
/// new view would otherwise see a partial one. On a cluster every member must report the view
/// built, including one that is down, so the wait can't finish while a node is down.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - The views to wait for
/// * `limit` - The longest the wait may last; `None` waits for as long as the builds take
/// * `progress` - Receives each progress report
pub(super) async fn wait_for_views(
    session: &Session,
    keyspace: &str,
    views: &[&str],
    limit: Option<Duration>,
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
        // fail hard once the limit passes, naming what is still building and where
        if let Some(expired) = wait_expired(&builds, started.elapsed(), limit) {
            progress(&progress_line(started.elapsed().as_secs(), &builds));
            return Err(expired);
        }
        // report what's still building at the start and every few seconds after
        if reported.is_none_or(|at| at.elapsed() >= VIEW_REPORT) {
            progress(&progress_line(started.elapsed().as_secs(), &builds));
            reported = Some(Instant::now());
        }
        tokio::time::sleep(VIEW_POLL).await;
    }
}

/// Wait until a dropped view no longer reads as built anywhere, so a view created again under its
/// name can't be taken as built from what the dropped one left behind
///
/// `DROP MATERIALIZED VIEW` returns before Scylla's view builder clears the dropped view's
/// `system.built_views` row and `system_distributed.view_build_status` rows, which takes seconds
/// while the builder is busy. On a cluster no member may still report the view built, since a
/// member's stale row would stand in for its new build.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `view` - The dropped view
/// * `started` - When the limit started counting
/// * `limit` - The longest to wait from `started`; `None` waits for as long as clearing takes
pub(super) async fn wait_for_drop(
    session: &Session,
    keyspace: &str,
    view: &str,
    started: Instant,
    limit: Option<Duration>,
) -> Result<(), ApplyError> {
    loop {
        // stop once neither the node's list nor any member's status row reports the view built
        let builds = view_builds(session, keyspace, &[view]).await?;
        let cleared = builds
            .iter()
            .all(|build| !build.built && build.nodes.is_none_or(|(done, _)| done == 0));
        if cleared {
            return Ok(());
        }
        // fail once the limit passes with the dropped view still reported as built
        if limit.is_some_and(|limit| started.elapsed() >= limit) {
            return Err(ApplyError::DropUnfinished {
                waited: started.elapsed().as_secs(),
                views: vec![view.to_owned()],
            });
        }
        tokio::time::sleep(DROP_POLL).await;
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
/// * `options` - What to converge to, whether to execute anything, and how long to wait for
///   views to build
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
/// * `options` - What to converge to, whether to execute anything, and how long to wait for
///   views to build
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
/// * `options` - What to converge to, whether to execute anything, and how long to wait for
///   views to build
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
                view_wait: options.view_wait,
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
                view_wait: options.view_wait,
            };
            apply_snapshot(session, conf, &snapshot, &run, progress).await
        }
    }
}

/// Converge the live schema to a set of objects, and record their fingerprints
///
/// Checks the data-shaping settings against the record first, then creates the keyspace and
/// `schema_migrations`, then every missing object in creation order, adds the table columns the
/// keyspace lacks, drops and creates again each declared view whose live definition differs,
/// waits with no limit until every declared view is built, and records the data-shaping settings
/// the record lacks, the applied fingerprints, and the layout versions. It never drops or alters
/// any other existing object, so running it again on a converged keyspace issues no schema
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
        view_wait: None,
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
    /// The longest the run waits for views to build; `None` waits for as long as they take
    view_wait: Option<Duration>,
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

/// Wait for the views a run created or recreated and for any a stopped earlier run left
/// building, and return the names of the views waited for
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - Every view the run's schema holds
/// * `changes` - The changes the run issued
/// * `limit` - The longest the wait may last; `None` waits for as long as the builds take
/// * `progress` - Receives a line for each progress report
async fn wait_for_changed_views(
    session: &Session,
    keyspace: &str,
    views: &[&str],
    changes: &[Change],
    limit: Option<Duration>,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<String>, ApplyError> {
    // the views this run created or dropped and created again
    let created = changes
        .iter()
        .filter_map(|change| match change {
            Change::Create {
                name, view: true, ..
            } => Some(name.as_str()),
            Change::Recreate { stale, .. } => Some(stale.name.as_str()),
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
    wait_for_views(session, keyspace, &waited, limit, progress).await?;
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

/// List the declared tables whose names views hold in the live schema
///
/// # Arguments
///
/// * `live` - What the keyspace holds
/// * `objects` - The Scylla objects to converge to
fn tables_held_by_views(live: &LiveSchema, objects: &[ObjectDef]) -> Vec<String> {
    // a table is held when a view of its name exists
    objects
        .iter()
        .filter_map(|object| match object {
            ObjectDef::Table(table) if live.views.contains(table.name) => {
                Some(table.name.to_owned())
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
    // find the declared views whose live definitions differ from this build's
    let compared = if live.keyspace {
        let views = scylla
            .iter()
            .filter_map(|def| match def {
                ObjectDef::View(view) => Some(*view),
                ObjectDef::Table(_) => None,
            })
            .collect::<Vec<_>>();
        stale_views(session, &keyspace, &views).await?
    } else {
        ViewComparison::default()
    };
    let changes = plan(conf, &live, &scylla, &compared.stale);
    // a view whose live definition can't be read is left as it is and reported
    let mut warnings = config.warnings.clone();
    for view in &compared.unreadable {
        let warning = format!(
            "view `{}`'s live definition can't be read ({}), so it was left as it is; \
             `thoradm schema apply --recreate-view {}` replaces it if it is out of date",
            view.name, view.reason, view.name
        );
        progress(&warning);
        warnings.push(warning);
    }
    let mut report = ApplyReport {
        statements: changes
            .iter()
            .flat_map(Change::statements)
            .map(ToOwned::to_owned)
            .collect(),
        created: Vec::new(),
        added: Vec::new(),
        config_changes: Vec::new(),
        fingerprint: fingerprints(objects).0,
        trigger_key: trigger_key.clone(),
        views_built: Vec::new(),
        views_recreated: Vec::new(),
        warnings,
        dry_run,
    };
    for change in &changes {
        match change {
            Change::Keyspace(_) => (),
            Change::Create { name, .. } => report.created.push(name.clone()),
            Change::Recreate { stale, base, .. } => {
                // a dry run says why it would recreate the view, as a real run does below
                if dry_run {
                    progress(&format!(
                        "view `{}` differs from this build's definition ({}); a real run drops it \
                         and creates it again, then waits for Scylla to build it from `{base}`",
                        stale.name,
                        stale.differences.join("; ")
                    ));
                }
                report.views_recreated.push(stale.clone());
            }
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
    // refuse the same way when a view holds a declared table's name, since the table can't be
    // created until the view is dropped
    let held = tables_held_by_views(&live, &scylla);
    if !held.is_empty() {
        return Err(ApplyError::TableHeldByView { tables: held });
    }
    // issue each change in order, timing the run's wait for views from here
    let started = Instant::now();
    for change in &changes {
        // say why a view is dropped and created again, and what that costs, before doing it
        if let Change::Recreate { stale, base, .. } = change {
            progress(&format!(
                "view `{}` differs from this build's definition ({}); dropping it and creating it \
                 again, then waiting for Scylla to build it from `{base}`",
                stale.name,
                stale.differences.join("; ")
            ));
        }
        execute(session, &keyspace, change, started, run.view_wait).await?;
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
    let left = run
        .view_wait
        .map(|limit| limit.saturating_sub(started.elapsed()));
    report.views_built =
        wait_for_changed_views(session, &keyspace, &views, &changes, left, progress).await?;
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
    use super::{
        ApplyError, HostStatus, LiveSchema, ObjectDef, SchemaMigrationsTable, ScyllaObject,
        StuckView, ViewBuild, cluster_build, member_build, progress_line, single_node_build,
        tables_held_by_views, vanished_views, wait_expired,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    /// Name cluster members by host id, each as `<id>-addr (<id>)`
    ///
    /// # Arguments
    ///
    /// * `ids` - The members' host ids
    fn hosts(ids: &[&str]) -> BTreeMap<String, String> {
        ids.iter()
            .map(|id| ((*id).to_owned(), format!("{id}-addr ({id})")))
            .collect()
    }

    /// Collect status rows from host and status literals
    ///
    /// # Arguments
    ///
    /// * `rows` - Each row's host id and status
    fn statuses(rows: &[(&str, &str)]) -> Vec<HostStatus> {
        rows.iter()
            .map(|(host_id, status)| HostStatus {
                host_id: (*host_id).to_owned(),
                status: (*status).to_owned(),
            })
            .collect()
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
        assert_eq!(building.waiting, Vec::<String>::new());
    }

    /// A cluster's view is built only once every member reported success, and the count says how
    /// many did
    #[test]
    fn cluster_builds_count_the_members_that_finished() {
        let members = hosts(&["a", "b", "c"]);
        // every member reported success
        let rows = statuses(&[("a", "SUCCESS"), ("b", "SUCCESS"), ("c", "SUCCESS")]);
        let done = cluster_build("samples", &members, &rows);
        assert!(done.built);
        assert_eq!(done.nodes, Some((3, 3)));
        assert_eq!(done.waiting, Vec::<String>::new());
        // one member still building
        let rows = statuses(&[("a", "SUCCESS"), ("b", "STARTED"), ("c", "SUCCESS")]);
        let partial = cluster_build("samples", &members, &rows);
        assert!(!partial.built);
        assert_eq!(partial.nodes, Some((2, 3)));
        assert_eq!(partial.waiting, ["b-addr (b)"]);
        // a member that has not reported yet, such as one that is down, leaves the view unbuilt
        let rows = statuses(&[("a", "SUCCESS"), ("b", "SUCCESS")]);
        let silent = cluster_build("samples", &members, &rows);
        assert!(!silent.built);
        assert_eq!(silent.nodes, Some((2, 3)));
        assert_eq!(silent.waiting, ["c-addr (c)"]);
        // no member has reported
        let none = cluster_build("samples", &members, &[]);
        assert!(!none.built);
        assert_eq!(none.nodes, Some((0, 3)));
        assert_eq!(none.waiting.len(), 3);
    }

    /// A member that is down keeps the `SUCCESS` rows it wrote before it went down, so the views it
    /// built still count as built and an apply that creates no view isn't held back by it, while a
    /// view created since waits on it by name
    #[test]
    fn a_down_members_earlier_builds_still_count() {
        // members a and b are up and c is down, having built samples before it went down
        let members = hosts(&["a", "b", "c"]);
        let built_before = statuses(&[("a", "SUCCESS"), ("b", "SUCCESS"), ("c", "SUCCESS")]);
        let build = cluster_build("samples", &members, &built_before);
        assert!(build.built);
        assert_eq!(build.nodes, Some((3, 3)));
        // a view created while c is down has no row from it, so it waits on c
        let created_since = statuses(&[("a", "SUCCESS"), ("b", "SUCCESS")]);
        let build = cluster_build("tags", &members, &created_since);
        assert!(!build.built);
        assert_eq!(build.waiting, ["c-addr (c)"]);
    }

    /// A row left by a host that is no longer a member neither holds a view back nor counts
    /// toward it
    #[test]
    fn rows_of_former_members_are_ignored() {
        let members = hosts(&["a", "b", "c"]);
        // every member built it, and a removed node left a row that never finished
        let rows = statuses(&[
            ("a", "SUCCESS"),
            ("b", "SUCCESS"),
            ("c", "SUCCESS"),
            ("gone", "STARTED"),
        ]);
        let build = cluster_build("samples", &members, &rows);
        assert!(build.built);
        assert_eq!(build.nodes, Some((3, 3)));
        assert_eq!(member_build(&members, &rows).ignored, ["gone"]);
        // a removed node's success doesn't stand in for a member that hasn't finished
        let rows = statuses(&[("a", "SUCCESS"), ("b", "SUCCESS"), ("gone", "SUCCESS")]);
        let build = cluster_build("samples", &members, &rows);
        assert!(!build.built);
        assert_eq!(build.nodes, Some((2, 3)));
        assert_eq!(build.waiting, ["c-addr (c)"]);
    }

    /// The members are split by their own rows, and each ignored host is named once
    #[test]
    fn member_builds_split_members_and_name_ignored_hosts_once() {
        let members = hosts(&["a", "b"]);
        let rows = statuses(&[
            ("a", "SUCCESS"),
            ("b", "STARTED"),
            ("x", "STARTED"),
            ("x", "SUCCESS"),
            ("w", "STARTED"),
        ]);
        let build = member_build(&members, &rows);
        assert_eq!(build.built, ["a-addr (a)"]);
        assert_eq!(build.waiting, ["b-addr (b)"]);
        assert_eq!(build.ignored, ["w", "x"]);
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

    /// On a cluster each view still building shows how many members have finished it and which
    /// it waits on
    #[test]
    fn the_line_shows_node_counts_on_a_cluster() {
        let members = hosts(&["n1", "n2", "n3"]);
        let done = statuses(&[("n1", "SUCCESS"), ("n2", "SUCCESS"), ("n3", "SUCCESS")]);
        let partial = statuses(&[("n1", "SUCCESS"), ("n2", "STARTED"), ("n3", "STARTED")]);
        let builds = [
            cluster_build("a", &members, &done),
            cluster_build("b", &members, &partial),
            cluster_build("c", &members, &[]),
        ];
        assert_eq!(
            progress_line(0, &builds),
            "1 of 3 views built after 0s; building: b (1 of 3 nodes, waiting on n2-addr (n2), \
             n3-addr (n3)), c (0 of 3 nodes, waiting on n1-addr (n1), n2-addr (n2), n3-addr (n3))"
        );
    }

    /// A wait with no limit, or with time left, carries on; one past its limit fails naming each
    /// view still building and the members it waits on
    #[test]
    fn the_wait_fails_at_its_limit_naming_views_and_hosts() {
        let members = hosts(&["n1", "n2", "n3"]);
        let down = statuses(&[("n1", "SUCCESS"), ("n2", "SUCCESS")]);
        let done = statuses(&[("n1", "SUCCESS"), ("n2", "SUCCESS"), ("n3", "SUCCESS")]);
        let builds = [
            cluster_build("samples", &members, &down),
            cluster_build("tags", &members, &done),
            single_node_build("local", false),
        ];
        let limit = Some(Duration::from_secs(60));
        // no limit never ends the wait, and neither does time left
        assert!(wait_expired(&builds, Duration::from_secs(7_777), None).is_none());
        assert!(wait_expired(&builds, Duration::from_secs(59), limit).is_none());
        // at the limit the wait fails with the views still building
        let Some(ApplyError::ViewWaitExpired { waited, stuck }) =
            wait_expired(&builds, Duration::from_secs(60), limit)
        else {
            panic!("the wait should have expired");
        };
        assert_eq!(waited, 60);
        assert_eq!(
            stuck,
            [
                StuckView {
                    view: "samples".to_owned(),
                    hosts: vec!["n3-addr (n3)".to_owned()],
                },
                StuckView {
                    view: "local".to_owned(),
                    hosts: Vec::new(),
                },
            ]
        );
        // the message names each view and host, and says what to do
        let message = ApplyError::ViewWaitExpired { waited, stuck }.to_string();
        assert!(message.contains("within 60s"), "{message}");
        assert!(message.contains("nothing was recorded"), "{message}");
        assert!(
            message.contains("`samples` (not built on n3-addr (n3))"),
            "{message}"
        );
        assert!(
            message.contains("`local` (not yet in system.built_views)"),
            "{message}"
        );
        assert!(message.contains("--view-wait-secs"), "{message}");
        assert!(!message.contains("`tags`"), "{message}");
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

    /// A declared table is held when a live view has its name, and a declared table or view the
    /// keyspace holds as its own kind isn't
    #[test]
    fn a_declared_table_whose_name_a_view_holds_is_listed() {
        // the record table and the first declared view, as the schema declares them
        let table = <SchemaMigrationsTable as ScyllaObject>::DEF;
        let view = *super::super::views::declared_views()
            .first()
            .expect("a declared view");
        let objects = [table, ObjectDef::View(view)];
        // a keyspace where a view holds the table's name, and the view exists as a view
        let live = LiveSchema {
            keyspace: true,
            tables: BTreeMap::new(),
            views: BTreeSet::from([table.name().to_owned(), view.name.to_owned()]),
            ttls: BTreeMap::new(),
        };
        assert_eq!(tables_held_by_views(&live, &objects), [table.name()]);
        // a keyspace where the table is a table holds nothing
        let live = LiveSchema {
            keyspace: true,
            tables: BTreeMap::from([(table.name().to_owned(), BTreeSet::new())]),
            views: BTreeSet::from([view.name.to_owned()]),
            ttls: BTreeMap::new(),
        };
        assert_eq!(tables_held_by_views(&live, &objects), Vec::<String>::new());
    }
}
