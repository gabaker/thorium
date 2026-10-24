//! Listing and dropping legacy Scylla objects: the tables and views older releases created that
//! this build no longer declares
//!
//! Listing reads the live keyspace and sorts what it holds beyond the declared schema into legacy
//! objects (registered in [`LEGACY`], so an admin can drop them), not-registered objects (live and
//! named in a past snapshot, but nothing says whether a revision needs them, so no command drops
//! them), and unmanaged extras (in no snapshot, never touched). Dropping is the one exception to
//! never dropping an undeclared object: it drops only registered legacy objects, only when asked
//! by name, and records each drop in `schema_migrations`.

use chrono::{DateTime, SecondsFormat, Utc};
use scylla::client::session::Session;
use scylla::statement::Statement;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::queries::migrations::SchemaMigrationsQueries;
use super::scylla::SchemaMigrationsTableRef;
use super::{MigrationRecordKind, ProbeError, RevisionSnapshot, SchemaObject, probe_scylla};
use crate::Conf;
use crate::models::upgrades::{
    Catalog, LEGACY, LegacyDef, LegacyKind, LegacyNeed, RevisionId, SchemaBackend, backend_name,
};

/// What a downgrade to one revision does to a legacy object
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DowngradeOutcome {
    /// The revision doesn't use the object, so a downgrade creates it again
    RecreatedEmpty,
    /// The revision uses the object and its data can be recreated, so a downgrade creates it
    /// again and rebuilds it
    Rebuilt,
    /// The revision uses the object and its rows are the only copy, so a downgrade loses them
    DataLoss {
        /// How many rows the object holds now
        rows: u64,
    },
}

impl DowngradeOutcome {
    /// Describe the outcome in one line, as `thoradm schema legacy` prints it
    ///
    /// # Arguments
    ///
    /// * `kind` - Whether the object is a table or a view
    #[must_use]
    pub fn describe(&self, kind: LegacyKind) -> String {
        // each outcome has its own wording, and a view is rebuilt from its base table
        match (self, kind) {
            (DowngradeOutcome::RecreatedEmpty, _) => {
                "recreated (it does not use this object)".to_owned()
            }
            (DowngradeOutcome::Rebuilt, LegacyKind::View) => {
                "recreated and rebuilt from the base table".to_owned()
            }
            (DowngradeOutcome::Rebuilt, LegacyKind::Table) => {
                "recreated and rebuilt by that revision".to_owned()
            }
            (DowngradeOutcome::DataLoss { rows }, _) => format!("DATA LOSS ({rows} rows)"),
        }
    }
}

/// What a downgrade to one revision of the support window does to a legacy object
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DowngradeTarget {
    /// The revision a deployment could downgrade to
    pub revision: String,
    /// What the downgrade does to the object
    pub outcome: DowngradeOutcome,
}

/// Whether `thoradm schema drop-legacy` may drop a legacy object
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "droppable", rename_all = "snake_case")]
pub enum Droppable {
    /// A drop loses nothing a revision in the support window needs
    Yes,
    /// A drop is refused
    No {
        /// Why
        reason: String,
    },
}

/// One registered legacy object as the keyspace holds it
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyStatus {
    /// The object's id, `<backend>:<name>`
    pub id: String,
    /// The backend the object lives in
    pub backend: SchemaBackend,
    /// The object's name
    pub name: String,
    /// Whether the object is a table or a view
    pub kind: LegacyKind,
    /// The object's data class
    pub class: super::DataClass,
    /// Whether the keyspace holds the object now
    pub present: bool,
    /// What the keyspace holds under the object's name (`table`, `view`, or `type`), when it
    /// holds something
    pub live_kind: Option<String>,
    /// How many rows it holds (zero when it is absent)
    pub rows: u64,
    /// The revisions whose setup created it
    pub created_by: Vec<String>,
    /// The revisions that read it, each with whether its data can be recreated
    pub used_by: Vec<(String, LegacyNeed)>,
    /// What a downgrade to each revision of the support window does to it, newest revision first
    pub downgrades: Vec<DowngradeTarget>,
    /// Whether `drop-legacy` may drop it
    pub droppable: Droppable,
    /// Why it was retired
    pub retirement: String,
}

/// A live object that a past snapshot names, that isn't declared, and that no legacy entry
/// registers, so no command drops it
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotRegistered {
    /// The backend the object lives in
    pub backend: SchemaBackend,
    /// The object's name
    pub name: String,
    /// Whether it is a table, a view, or a type
    pub kind: String,
    /// The revisions whose snapshots name it
    pub snapshots: Vec<String>,
}

/// What `thoradm schema legacy` reports
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyListing {
    /// One entry per registered legacy object, in registry order
    pub objects: Vec<LegacyStatus>,
    /// Objects that are live and named in a past snapshot but neither declared nor registered
    pub not_registered: Vec<NotRegistered>,
    /// How many live objects no snapshot names and nothing declares or registers
    pub unmanaged_extras: usize,
}

/// What the legacy commands judge objects against: the registry, the past snapshots, and the
/// support window
#[derive(Debug, Clone)]
pub struct LegacySources {
    /// The registered legacy objects
    pub registry: Vec<LegacyDef>,
    /// The past revisions' snapshots
    pub snapshots: Vec<RevisionSnapshot>,
    /// The revisions a deployment can still downgrade to, newest first
    pub window: Vec<RevisionId>,
    /// The declared Scylla objects' names
    pub declared: BTreeSet<String>,
}

impl LegacySources {
    /// Gets the sources this build judges against: [`LEGACY`], every committed snapshot, the
    /// production catalog's support window, and the declared schema
    ///
    /// # Panics
    ///
    /// Panics when a committed snapshot is invalid, which the snapshot tests rule out.
    #[must_use]
    pub fn production() -> LegacySources {
        // the committed snapshots, each checked when loaded
        let snapshots = RevisionSnapshot::committed_ids()
            .filter_map(RevisionSnapshot::committed)
            .map(|snapshot| snapshot.expect("committed snapshots are valid"))
            .collect();
        LegacySources {
            registry: LEGACY.to_vec(),
            snapshots,
            window: support_window(&Catalog::PRODUCTION),
            declared: declared_scylla(),
        }
    }
}

/// Gets the names of the Scylla objects this build declares
#[must_use]
pub fn declared_scylla() -> BTreeSet<String> {
    // keep the Scylla objects among the declared ones
    SchemaObject::declared()
        .into_iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .map(|object| object.name)
        .collect()
}

/// Gets the revisions a deployment can still move to by downgrade or restore: every released
/// revision the catalog knows that isn't below its last support floor, newest first
///
/// # Arguments
///
/// * `catalog` - The catalog to read the window from
#[must_use]
pub fn support_window(catalog: &Catalog) -> Vec<RevisionId> {
    // every released revision any edge starts from or reaches
    let mut revisions = BTreeSet::new();
    for edge in catalog.edges {
        revisions.insert(edge.revision());
        if let Some(from) = edge.from.and_then(|from| from.parse::<RevisionId>().ok()) {
            revisions.insert(from);
        }
    }
    // a revision below the last floor can't be reached any more, and a pre-release is a
    // development branch's
    let floor = catalog
        .floors
        .last()
        .and_then(|floor| floor.minimum.parse::<RevisionId>().ok());
    revisions
        .into_iter()
        .rev()
        .filter(|revision| !revision.is_prerelease())
        .filter(|revision| floor.as_ref().is_none_or(|floor| revision >= floor))
        .collect()
}

/// Why a legacy command refuses to go on
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyRefusal {
    /// The id names an object this build declares
    Declared {
        /// The id given
        id: String,
    },
    /// The id names no registered legacy object
    Unregistered {
        /// The id given
        id: String,
        /// The registered ids
        registered: Vec<String>,
        /// The revisions whose snapshots name an object of that id, when some do: it is then a
        /// not-registered object
        snapshots: Vec<String>,
    },
    /// A revision in the support window uses the object with data that is the only copy
    RequiredData {
        /// The object's id
        id: String,
        /// The revisions that need it
        revisions: Vec<String>,
        /// How many rows a downgrade would lose
        rows: u64,
    },
    /// The live object isn't the kind the registry says it is, so dropping it as that kind would
    /// fail
    WrongKind {
        /// The object's id
        id: String,
        /// The kind the registry says
        registered: LegacyKind,
        /// The kind the keyspace holds
        live: String,
    },
    /// `schema_migrations` doesn't exist, so a drop can't be recorded and nothing is dropped
    NoRecord,
}

impl std::fmt::Display for LegacyRefusal {
    /// Describe why the command refuses
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LegacyRefusal::Declared { id } => write!(f, "{id} is a declared object; refusing."),
            LegacyRefusal::Unregistered {
                id,
                registered,
                snapshots,
            } => {
                write!(
                    f,
                    "{id} is not a registered legacy object. Registered: {}.",
                    registered.join(", ")
                )?;
                if snapshots.is_empty() {
                    Ok(())
                } else {
                    write!(
                        f,
                        " It is named in the snapshots of {} but has no legacy entry; ask a \
                         developer to register it as legacy or declare it again.",
                        snapshots.join(", ")
                    )
                }
            }
            LegacyRefusal::RequiredData {
                id,
                revisions,
                rows,
            } => write!(
                f,
                "refused: {id} is used by {} (required data, {rows} rows); a downgrade would \
                 lose it.",
                revisions.join(", ")
            ),
            LegacyRefusal::WrongKind {
                id,
                registered,
                live,
            } => write!(
                f,
                "refused: {id} is registered as a {} but the keyspace holds a {live} of that \
                 name; nothing is dropped.",
                registered.label()
            ),
            LegacyRefusal::NoRecord => write!(
                f,
                "refused: schema_migrations doesn't exist, so a drop can't be recorded; run \
                 `thoradm schema apply` first."
            ),
        }
    }
}

/// Why listing or dropping legacy objects stopped
#[derive(Debug)]
pub enum LegacyError {
    /// The command refuses; nothing was changed
    Refused(LegacyRefusal),
    /// Reading the live schema failed
    Probe(ProbeError),
    /// A statement failed
    Query {
        /// The statement, or what it was for
        statement: String,
        /// The error Scylla returned
        error: String,
    },
    /// An object was dropped but its audit row couldn't be written
    NotRecorded {
        /// The object's id
        id: String,
        /// Why the write failed
        error: String,
    },
}

impl LegacyError {
    /// Build the error for a failed statement
    ///
    /// # Arguments
    ///
    /// * `statement` - The statement, or what it was for
    /// * `error` - The error Scylla returned
    fn query(statement: &str, error: impl std::fmt::Display) -> Self {
        // keep the statement and the message
        LegacyError::Query {
            statement: statement.to_owned(),
            error: error.to_string(),
        }
    }
}

impl std::fmt::Display for LegacyError {
    /// Describe why the command stopped
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LegacyError::Refused(refusal) => write!(f, "{refusal}"),
            LegacyError::Probe(err) => write!(f, "{err}"),
            LegacyError::Query { statement, error } => write!(f, "{statement}: {error}"),
            LegacyError::NotRecorded { id, error } => write!(
                f,
                "{id} was dropped, but recording the drop in schema_migrations failed: {error}"
            ),
        }
    }
}

impl std::error::Error for LegacyError {}

impl From<LegacyRefusal> for LegacyError {
    /// Wrap a refusal
    ///
    /// # Arguments
    ///
    /// * `refusal` - The refusal
    fn from(refusal: LegacyRefusal) -> Self {
        LegacyError::Refused(refusal)
    }
}

/// How long the driver waits for one row count; a little over the server-side timeout the count
/// statement asks for, so the server's answer or error arrives first
pub const COUNT_TIMEOUT: Duration = Duration::from_mins(11);

/// Build the statement that counts the rows of a table or view, with a request timeout that
/// matches its server-side one
///
/// The driver's own default request timeout is 30 seconds, which would cut a count of a large
/// table short whatever `USING TIMEOUT` says.
///
/// # Arguments
///
/// * `keyspace` - The keyspace holding the object
/// * `name` - The table or view
#[must_use]
pub fn count_statement(keyspace: &str, name: &str) -> Statement {
    // a full count walks every partition, so it gets a timeout of its own on both sides
    let mut statement = Statement::new(format!(
        "SELECT COUNT(*) FROM {keyspace}.{name} BYPASS CACHE USING TIMEOUT 10m"
    ));
    statement.set_request_timeout(Some(COUNT_TIMEOUT));
    statement
}

/// Count the rows of a table or view
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace holding the object
/// * `name` - The table or view
async fn count_rows(session: &Session, keyspace: &str, name: &str) -> Result<u64, LegacyError> {
    // the statement carries its own request timeout
    let statement = count_statement(keyspace, name);
    let cql = statement.contents.clone();
    // run the count and read its one row
    let rows = session
        .query_unpaged(statement, ())
        .await
        .map_err(|err| LegacyError::query(&cql, err))?
        .into_rows_result()
        .map_err(|err| LegacyError::query(&cql, err))?;
    let (count,) = rows
        .single_row::<(i64,)>()
        .map_err(|err| LegacyError::query(&cql, err))?;
    // a count is never negative
    Ok(u64::try_from(count).unwrap_or_default())
}

/// Work out what a downgrade to each revision of the window does to a legacy object
///
/// # Arguments
///
/// * `entry` - The legacy object's registry entry
/// * `rows` - How many rows the object holds now
/// * `window` - The revisions a deployment can still downgrade to
#[must_use]
pub fn downgrades(entry: &LegacyDef, rows: u64, window: &[RevisionId]) -> Vec<DowngradeTarget> {
    // each revision either doesn't use the object, or uses it with data that can or can't be
    // recreated
    window
        .iter()
        .map(|revision| {
            let used = entry
                .used_by
                .iter()
                .find(|used| used.revision == revision.as_str());
            let outcome = match used.map(|used| used.need) {
                None => DowngradeOutcome::RecreatedEmpty,
                Some(LegacyNeed::Rebuildable) => DowngradeOutcome::Rebuilt,
                Some(LegacyNeed::Required) => DowngradeOutcome::DataLoss { rows },
            };
            DowngradeTarget {
                revision: revision.as_str().to_owned(),
                outcome,
            }
        })
        .collect()
}

/// Work out whether a legacy object may be dropped: not while a revision of the window uses it
/// with data that is the only copy
///
/// # Arguments
///
/// * `entry` - The legacy object's registry entry
/// * `rows` - How many rows the object holds now
/// * `window` - The revisions a deployment can still downgrade to
#[must_use]
pub fn droppable(entry: &LegacyDef, rows: u64, window: &[RevisionId]) -> Droppable {
    // the revisions of the window that need the object's rows
    let needing = entry
        .used_by
        .iter()
        .filter(|used| used.need == LegacyNeed::Required)
        .filter(|used| {
            window
                .iter()
                .any(|revision| revision.as_str() == used.revision)
        })
        .map(|used| used.revision.to_owned())
        .collect::<Vec<_>>();
    if needing.is_empty() {
        Droppable::Yes
    } else {
        Droppable::No {
            reason: format!(
                "{} uses it with required data ({rows} rows)",
                needing.join(", ")
            ),
        }
    }
}

/// Sort the live Scylla objects into the registered ones' statuses, the not-registered objects,
/// and the count of unmanaged extras
///
/// # Arguments
///
/// * `sources` - The registry, snapshots, window, and declared objects to judge against
/// * `live` - The live Scylla objects, by name, with their kind (`table`, `view`, or `type`)
/// * `rows` - The row count of each registered object that is live
#[must_use]
pub fn classify(
    sources: &LegacySources,
    live: &BTreeMap<String, String>,
    rows: &BTreeMap<String, u64>,
) -> LegacyListing {
    // one status per registered object, present or not
    let objects = sources
        .registry
        .iter()
        .map(|entry| {
            let held = rows.get(entry.name).copied().unwrap_or_default();
            LegacyStatus {
                id: entry.id(),
                backend: entry.backend,
                name: entry.name.to_owned(),
                kind: entry.kind,
                class: entry.class,
                present: live.contains_key(entry.name),
                live_kind: live.get(entry.name).cloned(),
                rows: held,
                created_by: entry
                    .created_by
                    .iter()
                    .map(|rev| (*rev).to_owned())
                    .collect(),
                used_by: entry
                    .used_by
                    .iter()
                    .map(|used| (used.revision.to_owned(), used.need))
                    .collect(),
                downgrades: downgrades(entry, held, &sources.window),
                droppable: droppable(entry, held, &sources.window),
                retirement: entry.retirement.to_owned(),
            }
        })
        .collect();
    // the live objects nothing declares or registers are either named in a snapshot or not
    let mut not_registered = Vec::new();
    let mut unmanaged_extras = 0;
    for (name, kind) in live {
        let registered = sources
            .registry
            .iter()
            .any(|entry| entry.backend == SchemaBackend::Scylla && entry.name == name);
        if registered || sources.declared.contains(name) {
            continue;
        }
        let snapshots = snapshots_naming(sources, name);
        if snapshots.is_empty() {
            unmanaged_extras += 1;
        } else {
            not_registered.push(NotRegistered {
                backend: SchemaBackend::Scylla,
                name: name.clone(),
                kind: kind.clone(),
                snapshots,
            });
        }
    }
    LegacyListing {
        objects,
        not_registered,
        unmanaged_extras,
    }
}

/// Lists the revisions whose snapshots name a Scylla object
///
/// # Arguments
///
/// * `sources` - The snapshots to look through
/// * `name` - The object's name
fn snapshots_naming(sources: &LegacySources, name: &str) -> Vec<String> {
    // each snapshot that lists a Scylla object of that name
    sources
        .snapshots
        .iter()
        .filter(|snapshot| {
            snapshot
                .objects
                .iter()
                .any(|object| object.backend == SchemaBackend::Scylla && object.name == name)
        })
        .map(|snapshot| snapshot.revision.as_str().to_owned())
        .collect()
}

/// Read the keyspace and report its legacy objects; only reads
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
/// * `sources` - The registry, snapshots, window, and declared objects to judge against
pub async fn inspect(
    session: &Session,
    conf: &Conf,
    sources: &LegacySources,
) -> Result<LegacyListing, LegacyError> {
    // probe the keyspace, keeping the Scylla tables, views, and types by name
    let keyspace = conf.thorium.namespace.to_lowercase();
    let (objects, _) = probe_scylla(session, &keyspace)
        .await
        .map_err(LegacyError::Probe)?;
    // name each live Scylla object with its kind
    let live = objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .map(|object| {
            let kind = object.canonical["kind"].as_str().unwrap_or("type");
            (object.name.clone(), kind.to_owned())
        })
        .collect::<BTreeMap<_, _>>();
    // count the rows of each registered object that is live
    let mut rows = BTreeMap::new();
    for entry in &sources.registry {
        // an object the keyspace lacks has no rows to count
        if live.contains_key(entry.name) {
            rows.insert(
                entry.name.to_owned(),
                count_rows(session, &keyspace, entry.name).await?,
            );
        }
    }
    // sort what the keyspace holds against the registry, snapshots, and declared schema
    Ok(classify(sources, &live, &rows))
}

/// Parse an object id, `<backend>:<name>`
///
/// # Arguments
///
/// * `id` - The id to parse
#[must_use]
pub fn parse_id(id: &str) -> Option<(SchemaBackend, &str)> {
    // the backend is everything before the first colon
    let (backend, name) = id.split_once(':')?;
    let backend = [
        SchemaBackend::Scylla,
        SchemaBackend::Elastic,
        SchemaBackend::Redis,
        SchemaBackend::S3,
    ]
    .into_iter()
    .find(|candidate| backend_name(*candidate) == backend)?;
    (!name.is_empty()).then_some((backend, name))
}

/// Resolve the ids an admin named to registry entries, refusing the whole list when any id names
/// a declared object, or no registered legacy object; an id given twice is dropped once
///
/// This needs no connection: it judges the ids against the registry, the declared schema, and the
/// snapshots alone.
///
/// # Arguments
///
/// * `ids` - The object ids the admin named
/// * `sources` - The registry, snapshots, and declared objects to judge against
pub fn resolve(ids: &[String], sources: &LegacySources) -> Result<Vec<LegacyDef>, LegacyRefusal> {
    // judge each id in the order given
    let registered = sources
        .registry
        .iter()
        .map(LegacyDef::id)
        .collect::<Vec<_>>();
    let mut found: Vec<LegacyDef> = Vec::with_capacity(ids.len());
    for id in ids {
        let parsed = parse_id(id);
        // a declared object is refused before the registry is consulted
        if let Some((SchemaBackend::Scylla, name)) = parsed
            && sources.declared.contains(name)
        {
            return Err(LegacyRefusal::Declared { id: id.clone() });
        }
        // then only a registered object may be named
        let entry = parsed.and_then(|(backend, name)| {
            sources
                .registry
                .iter()
                .find(|entry| entry.backend == backend && entry.name == name)
        });
        let Some(entry) = entry else {
            let snapshots = match parsed {
                Some((SchemaBackend::Scylla, name)) => snapshots_naming(sources, name),
                _ => Vec::new(),
            };
            return Err(LegacyRefusal::Unregistered {
                id: id.clone(),
                registered,
                snapshots,
            });
        };
        if !found.contains(entry) {
            found.push(*entry);
        }
    }
    Ok(found)
}

/// One drop a plan holds
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedDrop {
    /// The object's id
    pub id: String,
    /// Whether the object is a table or a view
    pub kind: LegacyKind,
    /// Whether the keyspace holds the object; an absent object has nothing to drop
    pub present: bool,
    /// How many rows the drop would remove
    pub rows: u64,
    /// The statement the drop issues
    pub statement: String,
    /// What a downgrade to each revision of the window does to the object afterwards
    pub downgrades: Vec<DowngradeTarget>,
}

/// What `drop-legacy` would do, or did
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DropPlan {
    /// The drops, in the order the admin named the objects
    pub drops: Vec<PlannedDrop>,
}

/// Render the statement that drops a legacy object
///
/// # Arguments
///
/// * `keyspace` - The keyspace holding the object
/// * `entry` - The registered object
#[must_use]
pub fn drop_statement(keyspace: &str, entry: &LegacyDef) -> String {
    // a table and a view are dropped with different statements, and one already gone is fine
    match entry.kind {
        LegacyKind::Table => format!("DROP TABLE IF EXISTS {keyspace}.{}", entry.name),
        LegacyKind::View => format!("DROP MATERIALIZED VIEW IF EXISTS {keyspace}.{}", entry.name),
    }
}

/// Plan the drops of resolved objects against what the keyspace holds, refusing the whole plan
/// when a revision of the window needs one of them
///
/// # Arguments
///
/// * `keyspace` - The keyspace holding the objects
/// * `entries` - The registered objects to drop, from [`resolve`]
/// * `listing` - What the keyspace holds, from [`inspect`]
pub fn plan(
    keyspace: &str,
    entries: &[LegacyDef],
    listing: &LegacyListing,
) -> Result<DropPlan, LegacyRefusal> {
    // judge every object before planning any
    let mut drops = Vec::with_capacity(entries.len());
    for entry in entries {
        let status = listing
            .objects
            .iter()
            .find(|status| status.id == entry.id())
            .ok_or_else(|| LegacyRefusal::Unregistered {
                id: entry.id(),
                registered: listing
                    .objects
                    .iter()
                    .map(|status| status.id.clone())
                    .collect(),
                snapshots: Vec::new(),
            })?;
        // an object a window revision needs is never dropped
        if status.droppable != Droppable::Yes {
            let revisions = status
                .used_by
                .iter()
                .filter(|(_, need)| *need == LegacyNeed::Required)
                .map(|(revision, _)| revision.clone())
                .collect();
            return Err(LegacyRefusal::RequiredData {
                id: status.id.clone(),
                revisions,
                rows: status.rows,
            });
        }
        // a live object of another kind can't be dropped as the registered one
        if let Some(live) = status
            .live_kind
            .as_ref()
            .filter(|live| *live != entry.kind.label())
        {
            return Err(LegacyRefusal::WrongKind {
                id: status.id.clone(),
                registered: entry.kind,
                live: live.clone(),
            });
        }
        drops.push(PlannedDrop {
            id: status.id.clone(),
            kind: entry.kind,
            present: status.present,
            rows: status.rows,
            statement: drop_statement(keyspace, entry),
            downgrades: status.downgrades.clone(),
        });
    }
    Ok(DropPlan { drops })
}

/// One drop the audit trail holds
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DropRecord {
    /// The object's id
    pub object: String,
    /// When it was dropped
    pub at: DateTime<Utc>,
    /// Who dropped it
    pub who: String,
    /// How many rows it held
    pub rows: u64,
}

/// Build the id of an audit row: the drop's time with nanoseconds, so rows sort oldest first,
/// then the object's id
///
/// # Arguments
///
/// * `at` - When the object was dropped
/// * `object` - The object's id
#[must_use]
pub fn record_id(at: DateTime<Utc>, object: &str) -> String {
    // RFC 3339 with nanoseconds, a slash, and the id
    format!(
        "{}/{object}",
        at.to_rfc3339_opts(SecondsFormat::Nanos, true)
    )
}

/// Drop the planned objects and record each drop in `schema_migrations`
///
/// Every id in the plan must be a registered legacy object, checked for all of them before
/// anything is dropped, and each drop runs the statement built here from the registry entry
/// ([`drop_statement`]); the statement a [`PlannedDrop`] carries is for display only and is never
/// executed. Nothing is dropped unless every drop can be recorded: the record's statements are
/// prepared first, and without `schema_migrations` the drop is refused. Each object is dropped and
/// then recorded in turn; an object that is already absent is skipped and recorded nowhere.
///
/// # Arguments
///
/// * `session` - The Scylla session to drop with
/// * `conf` - The Thorium config, for the keyspace
/// * `plan` - The drops, from [`plan`]
/// * `who` - Who runs the drop, recorded with each one
/// * `progress` - Receives a line for each drop
pub async fn drop_planned(
    session: &Session,
    conf: &Conf,
    plan: &DropPlan,
    who: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<DropRecord>, LegacyError> {
    // refuse before dropping anything when an id isn't registered, whatever the plan says
    let mut entries = Vec::with_capacity(plan.drops.len());
    for drop in &plan.drops {
        let Some(entry) = LEGACY.iter().find(|entry| entry.id() == drop.id) else {
            return Err(LegacyRefusal::Unregistered {
                id: drop.id.clone(),
                registered: LEGACY.iter().map(LegacyDef::id).collect(),
                snapshots: Vec::new(),
            }
            .into());
        };
        entries.push(*entry);
    }
    // refuse before dropping anything when the drops can't be recorded
    let keyspace = &conf.thorium.namespace;
    let queries = SchemaMigrationsQueries::prepare(session, keyspace)
        .await
        .map_err(|_| LegacyRefusal::NoRecord)?;
    let mut records = Vec::new();
    for (drop, entry) in plan.drops.iter().zip(&entries) {
        // an absent object has nothing to drop and nothing to record
        if !drop.present {
            progress(&format!("{} is not present; nothing to drop", drop.id));
            continue;
        }
        // drop it with the statement the registry entry gives, not the one the plan carries
        let statement = drop_statement(keyspace, entry);
        session
            .query_unpaged(statement.as_str(), ())
            .await
            .map_err(|err| LegacyError::query(&statement, err))?;
        progress(&format!("dropped {} ({} rows)", drop.id, drop.rows));
        // record the drop: who, when, the object, and its rows
        let at = Utc::now();
        let id = record_id(at, &drop.id);
        let rows = drop.rows.to_string();
        let none = BTreeMap::new();
        let row = SchemaMigrationsTableRef {
            kind: &MigrationRecordKind::LegacyDrop,
            id: &id,
            fingerprint: None,
            apply_key: None,
            layouts: &none,
            revision: None,
            version: None,
            entry: None,
            result: None,
            value: Some(&rows),
            status: None,
            attempt: None,
            lease: Some(who),
            started: None,
            finished: None,
            job: None,
            backup: None,
            counts: None,
            report: None,
            updated: at,
        };
        queries
            .insert(session, &row)
            .await
            .map_err(|err| LegacyError::NotRecorded {
                id: drop.id.clone(),
                error: err.to_string(),
            })?;
        records.push(DropRecord {
            object: drop.id.clone(),
            at,
            who: who.to_owned(),
            rows: drop.rows,
        });
    }
    Ok(records)
}

/// Read the audit rows of every legacy drop, oldest first
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
pub async fn drop_records(session: &Session, conf: &Conf) -> Result<Vec<DropRecord>, LegacyError> {
    // read the legacy-drop partition
    let queries = SchemaMigrationsQueries::prepare(session, &conf.thorium.namespace)
        .await
        .map_err(|err| LegacyError::query("prepare the schema_migrations statements", err))?;
    // list every row of the kind
    let rows = queries
        .list(session, &MigrationRecordKind::LegacyDrop)
        .await
        .map_err(|err| LegacyError::query("read the legacy drops", err))?;
    let mut records = Vec::new();
    for row in rows {
        // a row that doesn't deserialize can't be read
        let row = row.map_err(|err| LegacyError::query("read the legacy drops", err))?;
        // the id is `<at>/<object id>`
        let (at, object) = row.id.split_once('/').unwrap_or((&row.id, ""));
        // the time comes from the row, or failing that from its id; the rows count from its value
        records.push(DropRecord {
            object: object.to_owned(),
            at: row.updated.or_else(|| at.parse().ok()).unwrap_or_default(),
            who: row.lease.unwrap_or_default(),
            rows: row
                .value
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
        });
    }
    Ok(records)
}
