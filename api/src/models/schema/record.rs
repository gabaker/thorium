//! The deployment's revision and history in `schema_migrations`: reading the record for `thoradm
//! schema status`, seeding it, copying the operator's entries into it, moving its revision, and the
//! per-Job result rows
//!
//! The revision is one row and the history one row per entry, keyed `<at>/<revision>/<step>/
//! <outcome>` with `at` in RFC 3339 with nanoseconds, so rows sort oldest first and need neither a
//! counter nor a lightweight transaction, and two writers can't pick the same id.

use chrono::{DateTime, SecondsFormat, Utc};
use scylla::client::session::Session;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::apply::read_texts;
use super::baseline;
use super::queries::migrations::SchemaMigrationsQueries;
use super::scylla::{SchemaMigrationRow, SchemaMigrationsTable, SchemaMigrationsTableRef};
use super::{
    AppliedSchema, CatalogIdentity, ConfigRecord, DeploymentRecord, DriftReport, Fingerprint,
    FinishedStep, JsonText, MigrationRecordKind, ObjectDef, RecordStatus, RowResult,
    SchemaDefinition, SchemaObject, ScyllaObject,
};
use crate::Conf;
use crate::models::upgrades::{
    AppliedStep, Catalog, DriftKind, Family, RESET_STATE_STEP, RESTORED_STEP, RevisionId,
    SchemaBackend, StepState, UpgradeState,
};

/// The id of the revision row, the only row of its kind
pub const REVISION_ROW: &str = "deployment";

/// The prefix of the one line `thoradm schema status` prints, which the operator picks out of the
/// Job's log
pub const RESULT_PREFIX: &str = "THORIUM-RESULT ";

/// The step the `migrate record --revision` Job records for itself when it moves the revision
/// across an edge that lists it: it is the Job that finalizes the revision, so it never waits for
/// its own entry
pub const FINALIZE_STEP: &str = "finalize";

/// The step of the history entry a fresh install's first apply records
pub const RECORD_REVISION_STEP: &str = "record-revision";

/// The step of the history entry a seeded revision is recorded with
pub const SEED_REVISION_STEP: &str = "seed-revision";

/// How long a Job's result row lives before Scylla expires it, in seconds (seven days)
pub const RESULT_TTL_SECS: i32 = 7 * 86_400;

/// How many history entries a status returns: the state `ConfigMap`'s window
const WINDOW: usize = crate::models::upgrades::MAX_HISTORY;

/// What the status says in place of the files password's hash: that one is recorded
pub const REDACTED_PASSWORD: &str = "[recorded]";

/// The id of the deployment's backend row
const DEPLOYMENT_ID: &str = "deployment";

/// Why reading or writing the deployment's record failed
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// A statement failed
    Query {
        /// What was being done
        what: String,
        /// The error Scylla returned
        error: String,
    },
    /// A revision is already recorded, so no other is seeded
    Recorded {
        /// The recorded revision
        revision: String,
    },
    /// No revision is recorded, so there is nothing to move
    Unrecorded,
    /// Steps on the path to a revision have no `Done` or `Skipped` entry yet
    Unfinished {
        /// The revision asked for
        target: String,
        /// The steps still to finish, as `revision/step`
        steps: Vec<String>,
    },
    /// An input that can't be recorded
    Invalid {
        /// What is wrong with it
        what: String,
    },
}

impl fmt::Display for RecordError {
    /// Describe why the record couldn't be read or written
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Query { what, error } => write!(f, "failed to {what}: {error}"),
            RecordError::Recorded { revision } => write!(
                f,
                "the deployment already records revision {revision}, so no revision is seeded"
            ),
            RecordError::Unrecorded => write!(
                f,
                "the deployment records no revision; seed it with `thoradm migrate record --seed` \
                 or `thoradm schema apply --seed-revision`"
            ),
            RecordError::Unfinished { target, steps } => write!(
                f,
                "revision {target} isn't recorded because these steps have no done or skipped \
                 entry: {}",
                steps.join(", ")
            ),
            RecordError::Invalid { what } => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl RecordError {
    /// Build the error for a statement that failed
    ///
    /// # Arguments
    ///
    /// * `what` - What was being done
    /// * `error` - The error Scylla returned
    fn query(what: &str, error: impl fmt::Display) -> Self {
        // keep what was being done and the driver's message
        RecordError::Query {
            what: what.to_owned(),
            error: error.to_string(),
        }
    }

    /// Build the error for an input that can't be recorded
    ///
    /// # Arguments
    ///
    /// * `what` - What is wrong with it
    fn invalid(what: impl Into<String>) -> Self {
        // keep the description
        RecordError::Invalid { what: what.into() }
    }
}

/// Render an instant as history ids spell it: RFC 3339 in UTC with nanoseconds
///
/// # Arguments
///
/// * `at` - The instant
fn spell(at: DateTime<Utc>) -> String {
    // nine fractional digits keep ids of any precision in time order
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Parse an entry's time, which `AppliedStep` keeps as RFC 3339 text
///
/// # Arguments
///
/// * `at` - The entry's time, if it has one
fn parse_at(at: Option<&str>) -> Option<DateTime<Utc>> {
    // accept any RFC 3339 offset and read it as the same instant in UTC
    at.and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// Build the id of a history row: `<at>/<revision>/<step>/<outcome>`, with `at` in RFC 3339 with
/// nanoseconds so ids sort oldest first
///
/// # Arguments
///
/// * `entry` - The history entry
pub fn history_id(entry: &AppliedStep) -> Result<String, RecordError> {
    // the time is part of the key, so an entry without one has none
    let at = parse_at(entry.at.as_deref()).ok_or_else(|| {
        RecordError::invalid(format!(
            "the history entry for {}/{} has no valid time",
            entry.revision, entry.step
        ))
    })?;
    // spell the outcome the way it is serialized
    let outcome = serde_json::to_value(entry.outcome)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{:?}", entry.outcome));
    Ok(format!(
        "{}/{}/{}/{outcome}",
        spell(at),
        entry.revision,
        entry.step
    ))
}

/// Compute the `(revision, step)` pairs a full history counts as finished
///
/// Only steps the catalog holds are listed, so the set is as small as the catalog however long the
/// history is; the rule is the planner's ([`Catalog::finished`]).
///
/// # Arguments
///
/// * `history` - Every history entry, oldest first
/// * `catalog` - The catalog whose rule decides when a step counts as finished
#[must_use]
pub fn finished_steps(history: &[AppliedStep], catalog: &Catalog) -> Vec<FinishedStep> {
    // the rule reads only the history, so any revision stands in for the deployment's
    let mut state = UpgradeState::new(catalog.latest(), "");
    state.applied = history.to_vec();
    // keep each catalog step whose entries finish it
    catalog
        .steps()
        .filter(|(edge, step)| catalog.finished(&state, edge.id, step.id))
        .map(|(edge, step)| FinishedStep {
            revision: edge.revision(),
            step: step.id.to_owned(),
        })
        .collect()
}

/// Identify a catalog by its newest revision and a hash over every edge, step, and floor
///
/// # Arguments
///
/// * `catalog` - The catalog to identify
#[must_use]
pub fn catalog_identity(catalog: &Catalog) -> CatalogIdentity {
    // describe each edge with the parts that decide what runs on it
    let edges = catalog
        .edges
        .iter()
        .map(|edge| {
            let steps = edge
                .steps
                .iter()
                .map(|step| {
                    json!({
                        "id": step.id,
                        "kind": format!("{:?}", step.kind),
                        "migration": step.thoradm_migration,
                        "reverses": step.reverses,
                        "phase": step.phase.map(|phase| format!("{phase:?}")),
                        "pause": step.pause.iter().map(|component| component.deployment_name()).collect::<Vec<_>>(),
                        "supersedes": step.supersedes,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "from": edge.from,
                "id": edge.id,
                "prerelease": edge.prerelease,
                "sibling": edge.sibling,
                "steps": steps,
            })
        })
        .collect::<Vec<_>>();
    let floors = catalog
        .floors
        .iter()
        .map(|floor| json!([floor.minimum, floor.intermediate]))
        .collect::<Vec<_>>();
    // hash the whole description
    CatalogIdentity {
        latest: catalog.latest(),
        hash: Fingerprint::of(&json!({"edges": edges, "floors": floors})),
    }
}

/// Take the first row of a select, if it has one
///
/// # Arguments
///
/// * `rows` - What the select returned
fn first(
    rows: Vec<RowResult<SchemaMigrationRow>>,
) -> Result<Option<SchemaMigrationRow>, RecordError> {
    // a key select has at most one row
    rows.into_iter()
        .next()
        .transpose()
        .map_err(|err| RecordError::query("read a row", err))
}

/// Prepare the statements on `schema_migrations`
///
/// # Arguments
///
/// * `session` - The Scylla session to prepare on
/// * `conf` - The Thorium config, for the keyspace
async fn prepare(session: &Session, conf: &Conf) -> Result<SchemaMigrationsQueries, RecordError> {
    // the statements target the configured keyspace
    SchemaMigrationsQueries::prepare(session, &conf.thorium.namespace)
        .await
        .map_err(|err| RecordError::query("prepare the schema_migrations statements", err))
}

/// Get the tables a keyspace holds, which is none when the keyspace doesn't exist
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
async fn tables(session: &Session, conf: &Conf) -> Result<BTreeSet<String>, RecordError> {
    // Scylla stores unquoted names in lowercase
    let keyspace = conf.thorium.namespace.to_lowercase();
    let rows = read_texts::<1>(
        session,
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        &[&keyspace],
    )
    .await
    .map_err(|err| RecordError::query("read the keyspace's tables", err))?;
    Ok(rows.into_iter().map(|[table]| table).collect())
}

/// Get the name a backend's row has in `schema_migrations`
///
/// # Arguments
///
/// * `id` - The row's id
fn backend_of(id: &str) -> Option<SchemaBackend> {
    // the rows `schema apply` writes, besides the deployment's
    match id {
        "scylla" => Some(SchemaBackend::Scylla),
        "elastic" => Some(SchemaBackend::Elastic),
        "redis" => Some(SchemaBackend::Redis),
        "s3" => Some(SchemaBackend::S3),
        _ => None,
    }
}

/// Read every history entry, oldest first
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `queries` - The prepared statements
async fn read_history(
    session: &Session,
    queries: &SchemaMigrationsQueries,
) -> Result<Vec<AppliedStep>, RecordError> {
    // rows come back in key order, which is time order
    let rows = queries
        .list(session, &MigrationRecordKind::History)
        .await
        .map_err(|err| RecordError::query("read the history", err))?;
    let mut history = Vec::with_capacity(rows.len());
    for row in rows {
        // a row without an entry holds nothing to count
        let row = row.map_err(|err| RecordError::query("read a history row", err))?;
        if let Some(JsonText(entry)) = row.entry {
            history.push(entry);
        }
    }
    Ok(history)
}

/// Read the revision row, if a revision is recorded
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `queries` - The prepared statements
async fn read_revision(
    session: &Session,
    queries: &SchemaMigrationsQueries,
) -> Result<Option<SchemaMigrationRow>, RecordError> {
    // the revision row, when it holds a revision
    let rows = queries
        .get(session, &MigrationRecordKind::Revision, REVISION_ROW)
        .await
        .map_err(|err| RecordError::query("read the recorded revision", err))?;
    Ok(first(rows)?.filter(|row| row.revision.is_some()))
}

/// Read the applied schema from the backend rows
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `queries` - The prepared statements
async fn read_schema(
    session: &Session,
    queries: &SchemaMigrationsQueries,
) -> Result<AppliedSchema, RecordError> {
    // every backend row, with the deployment's among them
    let rows = queries
        .list(session, &MigrationRecordKind::Backend)
        .await
        .map_err(|err| RecordError::query("read the applied schema", err))?;
    let mut deployment = None;
    let mut backends = BTreeMap::new();
    let mut layouts = BTreeMap::new();
    for row in rows {
        let row = row.map_err(|err| RecordError::query("read a backend row", err))?;
        // a row's fingerprint is the one its apply wrote
        let print = row.fingerprint.map(|text| {
            serde_json::from_value::<Fingerprint>(json!(text)).expect("a fingerprint is a string")
        });
        if row.id == DEPLOYMENT_ID {
            deployment = print;
        } else if let Some(backend) = backend_of(&row.id) {
            if let Some(print) = print {
                backends.insert(backend, print);
            }
            // a Redis or S3 row also holds each family's layout version
            if let Some(versions) = row.layouts.filter(|versions| !versions.is_empty()) {
                layouts.insert(backend, versions);
            }
        }
    }
    // before the first apply no fingerprint is recorded, which reads as an empty one
    let deployment = deployment
        .unwrap_or_else(|| serde_json::from_value(json!("")).expect("a fingerprint is a string"));
    Ok(AppliedSchema {
        deployment,
        backends,
        layouts,
    })
}

/// Read the data-shaping settings
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `queries` - The prepared statements
async fn read_config(
    session: &Session,
    queries: &SchemaMigrationsQueries,
) -> Result<ConfigRecord, RecordError> {
    // every config row, as the setting and its value
    let rows = queries
        .list(session, &MigrationRecordKind::Config)
        .await
        .map_err(|err| RecordError::query("read the data-shaping record", err))?;
    let mut settings = Vec::with_capacity(rows.len());
    for row in rows {
        let row = row.map_err(|err| RecordError::query("read a config row", err))?;
        settings.push((row.id, row.value.unwrap_or_default()));
    }
    let mut config = ConfigRecord::from_rows(settings)
        .map_err(|err| RecordError::query("read the record", err))?;
    // the files password's hash would reach the pod log and the Job's result row and nothing
    // that reads the status uses it, so the status only says that a hash is recorded
    if !config.files_password.is_empty() {
        REDACTED_PASSWORD.clone_into(&mut config.files_password);
    }
    Ok(config)
}

/// Read the deployment's record
///
/// A keyspace that doesn't exist, or holds no tables, has no record to preserve and reads as no
/// keyspace: a fresh install. A keyspace with tables and no revision reads as no record.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
/// * `catalog` - The catalog whose rule decides which steps are finished
pub async fn read_status(
    session: &Session,
    conf: &Conf,
    catalog: &Catalog,
) -> Result<RecordStatus, RecordError> {
    // a keyspace without tables is a fresh install
    let tables = tables(session, conf).await?;
    if tables.is_empty() {
        return Ok(RecordStatus::NoKeyspace);
    }
    // a keyspace without the record table has no record
    let none = RecordStatus::NoRecord { suggested: None };
    if !tables.contains(<SchemaMigrationsTable as ScyllaObject>::NAME) {
        return Ok(none);
    }
    // and so does one whose table holds no revision
    let queries = prepare(session, conf).await?;
    let Some(row) = read_revision(session, &queries).await? else {
        return Ok(none);
    };
    let revision = row
        .revision
        .as_deref()
        .unwrap_or_default()
        .parse::<RevisionId>()
        .map_err(|err| RecordError::invalid(format!("the recorded revision: {err}")))?;
    // read the whole history in one query, count it, and compute what it finished
    let history = read_history(session, &queries).await?;
    let history_len = history.len() as u64;
    let finished = finished_steps(&history, catalog);
    let window = history.len().saturating_sub(WINDOW);
    let record = DeploymentRecord {
        revision,
        applied_by_version: row.version,
        history: history[window..].to_vec(),
        finished,
        history_len,
        schema: read_schema(session, &queries).await?,
        config: read_config(session, &queries).await?,
    };
    Ok(RecordStatus::Recorded {
        record,
        drift: None,
    })
}

/// Build a row of the record with only its key and write time set
///
/// # Arguments
///
/// * `kind` - The row's kind
/// * `id` - The row's id
/// * `none` - An empty layouts map the row borrows
fn blank<'a>(
    kind: &'a MigrationRecordKind,
    id: &'a str,
    none: &'a BTreeMap<String, u32>,
) -> SchemaMigrationsTableRef<'a> {
    // every column a kind leaves unset stays unset
    SchemaMigrationsTableRef {
        kind,
        id,
        fingerprint: None,
        apply_key: None,
        layouts: none,
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
    }
}

/// Write one history row, keyed by its entry, with the entry's time spelled the way ids spell it
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `queries` - The prepared statements
/// * `entry` - The entry to write
async fn write_entry(
    session: &Session,
    queries: &SchemaMigrationsQueries,
    entry: &AppliedStep,
) -> Result<(), RecordError> {
    // keep the entry with its time normalized, so it reads back with the id it was written under
    let id = history_id(entry)?;
    let mut stored = entry.clone();
    stored.at = parse_at(entry.at.as_deref()).map(spell);
    let none = BTreeMap::new();
    let mut row = blank(&MigrationRecordKind::History, &id, &none);
    let wrapped = JsonText(stored);
    row.entry = Some(&wrapped);
    queries
        .insert(session, &row)
        .await
        .map_err(|err| RecordError::query("write a history entry", err))?;
    Ok(())
}

/// Write the revision row
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `queries` - The prepared statements
/// * `revision` - The revision to record
/// * `version` - The Thorium version recording it
async fn write_revision(
    session: &Session,
    queries: &SchemaMigrationsQueries,
    revision: &RevisionId,
    version: &str,
) -> Result<(), RecordError> {
    // the row holds only the revision and who recorded it
    let none = BTreeMap::new();
    let mut row = blank(&MigrationRecordKind::Revision, REVISION_ROW, &none);
    row.revision = Some(revision.as_str());
    row.version = Some(version);
    queries
        .insert(session, &row)
        .await
        .map_err(|err| RecordError::query("record the revision", err))?;
    Ok(())
}

/// Fill in the times of entries that have none, so the history keeps its order: an entry without a
/// valid time gets a nanosecond after the entry before it
///
/// # Arguments
///
/// * `entries` - The entries, oldest first
fn timed(entries: &[AppliedStep]) -> Vec<AppliedStep> {
    // walk the entries in order, remembering the last time seen
    let mut last = DateTime::<Utc>::UNIX_EPOCH;
    entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            let at =
                parse_at(entry.at.as_deref()).unwrap_or(last + chrono::Duration::nanoseconds(1));
            entry.at = Some(spell(at));
            last = at;
            entry
        })
        .collect()
}

/// Seed the revision and the full history from the operator's state, creating `schema_migrations`
/// when it is missing; refused once a revision is recorded
///
/// Returns how many history rows were written.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `revision` - The revision the state records
/// * `version` - The Thorium version recording it
/// * `entries` - The state's history, oldest first
pub async fn seed_from_state(
    session: &Session,
    conf: &Conf,
    revision: &RevisionId,
    version: &str,
    entries: &[AppliedStep],
) -> Result<usize, RecordError> {
    // create the table when the deployment predates it, in the keyspace it already has
    let keyspace = conf.thorium.namespace.to_lowercase();
    if tables(session, conf).await?.is_empty() {
        return Err(RecordError::invalid(format!(
            "the keyspace {keyspace} holds no tables to seed a record for"
        )));
    }
    if let ObjectDef::Table(table) = <SchemaMigrationsTable as ScyllaObject>::DEF {
        let cql = table.create_cql(&conf.thorium.namespace, None);
        session
            .query_unpaged(cql.as_str(), ())
            .await
            .map_err(|err| RecordError::query("create schema_migrations", err))?;
    }
    // a recorded revision is never replaced
    let queries = prepare(session, conf).await?;
    if let Some(row) = read_revision(session, &queries).await? {
        return Err(RecordError::Recorded {
            revision: row.revision.unwrap_or_default(),
        });
    }
    // write every entry, then the revision, so a stopped seed leaves no revision and runs again
    let entries = timed(entries);
    for entry in &entries {
        write_entry(session, &queries, entry).await?;
    }
    write_revision(session, &queries, revision, version).await?;
    Ok(entries.len())
}

/// Append history entries, each under its own id, so an entry written twice stays one row
///
/// Returns how many rows were new.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `entries` - The entries to append
pub async fn append_history(
    session: &Session,
    conf: &Conf,
    entries: &[AppliedStep],
) -> Result<usize, RecordError> {
    // write each entry, counting those whose id wasn't there
    let queries = prepare(session, conf).await?;
    append(session, &queries, entries).await
}

/// Append history entries with prepared statements
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `queries` - The prepared statements
/// * `entries` - The entries to append
async fn append(
    session: &Session,
    queries: &SchemaMigrationsQueries,
    entries: &[AppliedStep],
) -> Result<usize, RecordError> {
    // an entry whose id exists is written again, which changes nothing
    let mut added = 0;
    for entry in entries {
        let id = history_id(entry)?;
        let existing = queries
            .get(session, &MigrationRecordKind::History, &id)
            .await
            .map_err(|err| RecordError::query("read a history row", err))?;
        added += usize::from(first(existing)?.is_none());
        write_entry(session, queries, entry).await?;
    }
    Ok(added)
}

/// Claim a lossy step's confirmation code, so no run of any step ever uses it again
///
/// The code is written as a `consumed_code` row keyed by the code, holding the entry of the run that
/// claimed it. The rows are never deleted and aren't part of the history the state mirrors, so a
/// code stays consumed after the 200-entry mirror drops its entry or an older operator rewrites the
/// mirror without it. The caller holds the namespace's writer lease, so a read followed by a write
/// can't race another claim.
///
/// Returns whether the code was claimed now, `false` when a row for it already existed.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `code` - The confirmation code
/// * `entry` - The entry of the run that consumes it
pub async fn claim_consumed_code(
    session: &Session,
    conf: &Conf,
    code: &str,
    entry: &AppliedStep,
) -> Result<bool, RecordError> {
    // a code with a row is consumed already
    let queries = prepare(session, conf).await?;
    let existing = queries
        .get(session, &MigrationRecordKind::ConsumedCode, code)
        .await
        .map_err(|err| RecordError::query("read a consumed code", err))?;
    if first(existing)?.is_some() {
        return Ok(false);
    }
    // otherwise claim it with the entry of the run
    let none = BTreeMap::new();
    let mut row = blank(&MigrationRecordKind::ConsumedCode, code, &none);
    let wrapped = JsonText(entry.clone());
    row.entry = Some(&wrapped);
    queries
        .insert(session, &row)
        .await
        .map_err(|err| RecordError::query("write a consumed code", err))?;
    Ok(true)
}

/// Whether the operator's entry may be copied into the record: not a `Done` entry of a gated step,
/// which only that step's own thoradm run writes, and not an entry that moves the revision, which
/// only thoradm's own reset and restore write
///
/// # Arguments
///
/// * `catalog` - The catalog saying which steps are gated
/// * `entry` - The entry from the state
pub(super) fn copyable(catalog: &Catalog, entry: &AppliedStep) -> bool {
    // reset and restore entries lower the revision, so only their own commands write them
    if entry.step == RESET_STATE_STEP || entry.step == RESTORED_STEP {
        return false;
    }
    // a gated step is finished by its own run, never by an entry in the state
    let gated = catalog
        .step(&entry.step)
        .is_some_and(|(_, step)| step.kind.needs_approval());
    !(gated && entry.outcome == StepState::Done)
}

/// Whether an entry in the state's history is the `Done` that ends a derived rebuild's reingest
///
/// The rebuild's own run only starts the reingest and ends its step `Reingesting`, so the operator's
/// `Done` entry, recorded once the search-streamer finished, is the record of the step finishing even
/// though the step is gated. A `Done` of the step with no `Reingesting` entry before it is not.
///
/// # Arguments
///
/// * `entries` - The state's history, oldest first
/// * `index` - The position of the entry to judge
pub(super) fn ends_reingest(entries: &[AppliedStep], index: usize) -> bool {
    // only a `Done` can end a reingest
    let entry = &entries[index];
    if entry.outcome != StepState::Done {
        return false;
    }
    // it follows the same step's `Reingesting` entry
    entries[..index].iter().any(|earlier| {
        earlier.outcome == StepState::Reingesting
            && earlier.revision == entry.revision
            && earlier.step == entry.step
    })
}

/// Whether an entry in the state's history is the `Done` that ends a run the operator started and
/// no `Done` of the record holds yet for that run
///
/// A `Done` the record holds from before the run started belongs to an earlier run, such as one a
/// reverse has since undone, so it doesn't stop this run's `Done` from being copied.
///
/// # Arguments
///
/// * `entries` - The state's history, oldest first
/// * `index` - The position of the entry to judge
/// * `recorded` - The history the record already holds
fn ends_run(entries: &[AppliedStep], index: usize, recorded: &[AppliedStep]) -> bool {
    // only a `Done` can end a run, and the lowering entries are only written by their own commands
    let entry = &entries[index];
    if entry.outcome != StepState::Done
        || entry.step == RESET_STATE_STEP
        || entry.step == RESTORED_STEP
    {
        return false;
    }
    // the history holds the `Running` entry that started the step's latest run before this `Done`
    let same = |other: &AppliedStep| other.revision == entry.revision && other.step == entry.step;
    let Some(started) = entries[..index]
        .iter()
        .rev()
        .find(|earlier| earlier.outcome == StepState::Running && same(earlier))
    else {
        return false;
    };
    // the record holds no `Done` of the step written since that run started, so an earlier run's
    // `Done` doesn't count, and an entry with no readable time is taken to belong to the run
    let started_at = parse_at(started.at.as_deref());
    !recorded.iter().any(|held| {
        held.outcome == StepState::Done
            && same(held)
            && match (parse_at(held.at.as_deref()), started_at) {
                (Some(held_at), Some(started_at)) => held_at >= started_at,
                _ => true,
            }
    })
}

/// Pick the entries of the operator's history to copy into the record
///
/// An entry is picked when [`copyable`] says so, when it ends a derived rebuild's reingest, or
/// when it is the operator's `Done` of a gated step whose run the history holds a `Running` entry
/// of and the record holds no `Done` of yet. The operator records a data step's outcome from its
/// Job's termination summary, so that `Done` is the record of the step finishing; a step the
/// record already holds a `Done` of, such as one run by hand, keeps a single `Done`.
///
/// # Arguments
///
/// * `catalog` - The catalog saying which steps are gated
/// * `entries` - The state's history, oldest first
/// * `recorded` - The history the record already holds
pub(super) fn picked(
    catalog: &Catalog,
    entries: &[AppliedStep],
    recorded: &[AppliedStep],
) -> Vec<AppliedStep> {
    // keep each entry the record may hold
    entries
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            copyable(catalog, entry)
                || ends_reingest(entries, *index)
                || ends_run(entries, *index, recorded)
        })
        .map(|(_, entry)| entry.clone())
        .collect()
}

/// Copy the entries the operator recorded since the last copy into the history, never a `Done`
/// entry of a gated step, except the one that ends a rebuild's reingest and the one that ends a
/// run the operator started that the record holds no `Done` of
///
/// Returns how many rows were copied. An entry already in the record, matched by its revision,
/// step, outcome, and time, isn't copied again.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `catalog` - The catalog saying which steps are gated
/// * `entries` - The state's history
pub async fn copy_operator_entries(
    session: &Session,
    conf: &Conf,
    catalog: &Catalog,
    entries: &[AppliedStep],
) -> Result<usize, RecordError> {
    // read what the record holds, then keep only what may be copied and can be keyed
    let queries = prepare(session, conf).await?;
    let recorded = read_history(session, &queries).await?;
    let copy = picked(catalog, entries, &recorded)
        .into_iter()
        .filter(|entry| history_id(entry).is_ok())
        .collect::<Vec<_>>();
    append(session, &queries, &copy).await
}

/// Write a step's target layout into a backend row when the step was recorded `Skipped` because
/// every object was already current
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `queries` - The prepared statements
/// * `family` - The family the step's transform rewrites
/// * `layout` - The layout version the transform writes
async fn record_layout(
    session: &Session,
    queries: &SchemaMigrationsQueries,
    family: Family,
    layout: u32,
) -> Result<(), RecordError> {
    // only Redis families and S3 buckets record a layout version
    let (id, name) = match family {
        Family::Redis(name) => ("redis", name),
        Family::S3Prefix { bucket, .. } => ("s3", bucket),
        Family::ScyllaTable(_) => return Ok(()),
    };
    // a backend nothing applied yet records no layouts that could lag
    let rows = queries
        .get(session, &MigrationRecordKind::Backend, id)
        .await
        .map_err(|err| RecordError::query("read a backend row", err))?;
    let Some(existing) = first(rows)? else {
        return Ok(());
    };
    // write the row again with the one family's version changed
    let mut layouts = existing.layouts.unwrap_or_default();
    layouts.insert(name.to_owned(), layout);
    let mut row = blank(&MigrationRecordKind::Backend, id, &layouts);
    row.fingerprint = existing.fingerprint.as_deref();
    row.apply_key = existing.apply_key.as_deref();
    row.job = existing.job.as_deref();
    queries
        .insert(session, &row)
        .await
        .map_err(|err| RecordError::query("record a layout", err))?;
    Ok(())
}

/// Write one Redis family's or S3 bucket's layout version into its backend row, as a data
/// migration does when its pass finishes, so the record never lags the data
///
/// A backend nothing applied yet has no row to advance, and the call changes nothing.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `family` - The family or bucket the migration converted
/// * `layout` - The layout version its data is in now
pub async fn record_family_layout(
    session: &Session,
    conf: &Conf,
    family: Family,
    layout: u32,
) -> Result<(), RecordError> {
    // write the one version into the backend row
    let queries = prepare(session, conf).await?;
    record_layout(session, &queries, family, layout).await
}

/// Move the recorded revision to a target once every step of the path is finished
///
/// A step counts as finished by the rule in [`Catalog::finished`], except the finalize step, which
/// this call records itself. For each step of the path recorded `Skipped` because its check found
/// every object current, the step's target layout is written into its backend row, so the record
/// never lags data that needed no conversion.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `catalog` - The catalog holding the path
/// * `target` - The revision to record
/// * `version` - The Thorium version recording it
pub async fn record_revision(
    session: &Session,
    conf: &Conf,
    catalog: &Catalog,
    target: &RevisionId,
    version: &str,
) -> Result<(), RecordError> {
    // a revision can only move when one is recorded
    let queries = prepare(session, conf).await?;
    let Some(row) = read_revision(session, &queries).await? else {
        return Err(RecordError::Unrecorded);
    };
    let current = row
        .revision
        .as_deref()
        .unwrap_or_default()
        .parse::<RevisionId>()
        .map_err(|err| RecordError::invalid(format!("the recorded revision: {err}")))?;
    // find the edges to cross, and the history that says which of their steps finished
    let path = catalog
        .path(&current, target)
        .map_err(|err| RecordError::invalid(err.to_string()))?;
    let history = read_history(session, &queries).await?;
    let mut state = UpgradeState::new(current, version);
    state.applied = history;
    // cross each edge only when every step but the finalize step is finished
    let mut pending = Vec::new();
    for edge in &path {
        let unfinished = catalog
            .edge_steps(&state, edge)
            .into_iter()
            .filter(|step| step.id != FINALIZE_STEP)
            .map(|step| format!("{}/{}", edge.id, step.id))
            .collect::<Vec<_>>();
        if !unfinished.is_empty() {
            return Err(RecordError::Unfinished {
                target: target.to_string(),
                steps: unfinished,
            });
        }
        pending.push(*edge);
        state.revision = edge.revision();
    }
    // record the finalize step of each edge that lists it and hasn't finished it
    let by = format!("thoradm {version}");
    for edge in &pending {
        for step in edge.steps.iter().filter(|step| step.id == FINALIZE_STEP) {
            if !catalog.finished(&state, edge.id, step.id) {
                let entry = AppliedStep {
                    revision: edge.id.to_owned(),
                    step: step.id.to_owned(),
                    outcome: StepState::Done,
                    message: Some(format!("recorded revision {}", edge.id)),
                    at: Some(spell(Utc::now())),
                    by: Some(by.clone()),
                    ..AppliedStep::default()
                };
                write_entry(session, &queries, &entry).await?;
            }
        }
    }
    // write the layouts of the steps that needed no conversion
    for edge in &pending {
        for step in edge.steps {
            let latest = state.applied.iter().rev().find(|applied| {
                applied.revision == edge.id
                    && applied.step == step.id
                    && matches!(applied.outcome, StepState::Done | StepState::Skipped)
            });
            if let (Some(applied), Some(transform)) = (latest, step.transform)
                && applied.outcome == StepState::Skipped
            {
                record_layout(session, &queries, transform.family, transform.to).await?;
            }
        }
    }
    // the revision moves last, so a stopped run leaves the old one and runs again
    write_revision(session, &queries, target, version).await?;
    Ok(())
}

/// Write the revision row and the history entry of a seed the apply made, after the schema and its
/// record converged
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `revision` - The revision to record
/// * `step` - The step of the history entry
/// * `message` - What the entry says about where the revision came from
pub(super) async fn record_seed(
    session: &Session,
    conf: &Conf,
    revision: &RevisionId,
    step: &str,
    message: &str,
) -> Result<(), RecordError> {
    // the entry first, then the revision, so a stopped run leaves no revision and seeds again
    let queries = prepare(session, conf).await?;
    let version = env!("CARGO_PKG_VERSION");
    let entry = AppliedStep {
        revision: revision.to_string(),
        step: step.to_owned(),
        outcome: StepState::Done,
        message: Some(message.to_owned()),
        at: Some(spell(Utc::now())),
        by: Some(format!("thoradm {version}")),
        ..AppliedStep::default()
    };
    write_entry(session, &queries, &entry).await?;
    write_revision(session, &queries, revision, version).await
}

/// Read the revision a deployment records, if any
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
pub(super) async fn recorded_revision(
    session: &Session,
    conf: &Conf,
) -> Result<Option<String>, RecordError> {
    // a keyspace without the table records nothing
    if !tables(session, conf)
        .await?
        .contains(<SchemaMigrationsTable as ScyllaObject>::NAME)
    {
        return Ok(None);
    }
    let queries = prepare(session, conf).await?;
    Ok(read_revision(session, &queries)
        .await?
        .and_then(|row| row.revision))
}

/// Write a Job's full result under its UID, expiring after seven days
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `conf` - The Thorium config, for the keyspace
/// * `uid` - The Job's UID
/// * `result` - The result as JSON
pub async fn write_job_result(
    session: &Session,
    conf: &Conf,
    uid: &str,
    result: &serde_json::Value,
) -> Result<(), RecordError> {
    // the row holds only the result, and Scylla expires it
    let queries = prepare(session, conf).await?;
    let none = BTreeMap::new();
    let mut row = blank(&MigrationRecordKind::JobResult, uid, &none);
    let wrapped = JsonText(result.clone());
    row.result = Some(&wrapped);
    queries
        .insert_expiring(session, &row, RESULT_TTL_SECS)
        .await
        .map_err(|err| RecordError::query("write the Job's result", err))?;
    Ok(())
}

/// Read the full result a Job left under its UID
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
/// * `uid` - The Job's UID
pub async fn read_job_result(
    session: &Session,
    conf: &Conf,
    uid: &str,
) -> Result<Option<serde_json::Value>, RecordError> {
    // a keyspace without the table has no results
    if !tables(session, conf)
        .await?
        .contains(<SchemaMigrationsTable as ScyllaObject>::NAME)
    {
        return Ok(None);
    }
    let queries = prepare(session, conf).await?;
    let rows = queries
        .get(session, &MigrationRecordKind::JobResult, uid)
        .await
        .map_err(|err| RecordError::query("read the Job's result", err))?;
    Ok(first(rows)?
        .and_then(|row| row.result)
        .map(|JsonText(result)| result))
}

/// Whether an object is one of the two registered legacy objects older releases created and the
/// registry no longer declares
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `name` - The object's name
pub(super) fn is_registered_legacy(backend: SchemaBackend, name: &str) -> bool {
    // the `events` table and the `entities_by_name` view
    backend == SchemaBackend::Scylla
        && (name == baseline::EVENTS.name() || name == baseline::ENTITIES_BY_NAME.name())
}

/// List the differences between a live schema and a revision's snapshot that refuse a seed
///
/// An object or column the live schema lacks and `schema apply` would add doesn't refuse it, and
/// neither does an unexpected extra. An object that exists but differs, or a missing object the
/// apply can't add (one the registry doesn't declare, a key column, a view's column), does.
///
/// # Arguments
///
/// * `report` - The probe's comparison with the revision's snapshot
pub(super) fn seed_differences(report: &DriftReport) -> Vec<String> {
    // the Scylla objects this build's apply creates, by name
    let declared = SchemaObject::declared()
        .into_iter()
        .filter_map(|object| match object.definition {
            SchemaDefinition::Scylla(def) => Some((object.name, def)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    // whether the apply would add a missing object or column
    let addable = |backend: SchemaBackend, object: &str| {
        if backend != SchemaBackend::Scylla {
            return false;
        }
        match object.split_once('.') {
            // a table's non-key column is added with ALTER; a view is never altered
            Some((name, column)) => match declared.get(name) {
                Some(ObjectDef::Table(table)) => table
                    .columns
                    .iter()
                    .any(|def| def.name == column && !def.role.is_key()),
                _ => false,
            },
            // a declared table or view is created
            None => declared.contains_key(object),
        }
    };
    // judge each difference
    let mut differences = Vec::new();
    for (backend, drift) in &report.backends {
        for entry in &drift.objects {
            let refuses = match entry.kind {
                DriftKind::Changed => true,
                DriftKind::Missing => !addable(*backend, &entry.object),
                DriftKind::Unexpected | DriftKind::Unknown => false,
            };
            if refuses {
                let kind = match entry.kind {
                    DriftKind::Changed => "changed",
                    _ => "missing",
                };
                differences.push(format!("{} ({kind})", entry.object));
            }
        }
    }
    differences
}
