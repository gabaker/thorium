//! What a restore does with the schema: converging the keyspace to the backup's revision before
//! and after it loads, checking the files password against the backup's recorded identity, and
//! writing the backup's schema record back while keeping the history it replaces

use chrono::{DateTime, SecondsFormat, Utc};
use futures::StreamExt;
use scylla::client::session::Session;
use scylla::statement::batch::{Batch, BatchType};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use strum::IntoEnumIterator;
use thorium::Conf;
use thorium::models::schema::scylla::{
    SchemaMigrationIssuesTable, SchemaMigrationRow, SchemaMigrationsTable,
};
use thorium::models::schema::{
    ApplyError, ApplyOptions, ApplyReport, ApplyTarget, CLASSES, MigrationRecordKind, Projection,
    REVISION_ROW, RevisionSnapshot, ScyllaObject, SnapshotRun, Store, TableStatements, apply,
    apply_snapshot, verify_files_password, with_keyspace,
};
use thorium::models::upgrades::{
    AppliedStep, Catalog, FIRST_HELM_REVISION, RESTORED_STEP, RevisionId, StepState, UpgradeError,
    UpgradeState,
};

use super::schema::{BackupSchemaRecord, RECORD_FORMAT, RawRow, migrations_row};
use crate::Error;
use crate::args::BackupComponents;

/// What a restore converges the keyspace to
#[derive(Debug, Clone)]
pub enum ConvergeTo {
    /// The snapshot of the revision the backup's record names
    Snapshot(Box<RevisionSnapshot>),
    /// The schema this build declares, for a backup that records no revision this build knows
    Declared,
}

/// What a target holds in the record tables that a restore replaces
#[derive(Debug, Clone, Default)]
pub struct Existing {
    /// The target's history rows, which the restore archives
    pub history: Vec<SchemaMigrationsTable>,
    /// The revision the target recorded, if it recorded one
    pub revision: Option<String>,
    /// The kind and id of every backend and run row the target holds, which the backup's rows
    /// replace; history rows are known from `history`
    pub rows: Vec<(MigrationRecordKind, String)>,
    /// The key of every worker and issue row the target holds, as migration, transform, reason,
    /// and worker
    pub issue_keys: BTreeSet<(String, String, String, String)>,
}

/// Where a restore's record comes from and who wrote it
#[derive(Debug, Clone)]
pub struct Origin<'a> {
    /// The backup the record was read from, as the admin named it
    pub reference: &'a str,
    /// What restored it: thoradm's version and the OS user
    pub by: &'a str,
    /// When the restore wrote the record
    pub at: DateTime<Utc>,
    /// What the keys of the archived history rows start with: the backup's finish time, so a
    /// restore run again over an interrupted one writes the same keys instead of new ones
    pub stamp: &'a str,
    /// Whether the record is the baseline a restore records for a backup that holds none, rather
    /// than the backup's own
    pub baseline: bool,
}

/// The writes a restore makes to the record tables, in the order they are made: every row is
/// written over the old one first, and only the rows the backup lacks are deleted at the end, so a
/// restore that stops partway leaves the target's old record with some new rows beside it, never
/// no record
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordPlan {
    /// The target's replaced history, as archived history rows keyed from the backup's stamp
    pub archive: Vec<SchemaMigrationsTable>,
    /// The rows of the replaced kinds the target holds and the backup lacks, deleted last
    pub delete_rows: Vec<(MigrationRecordKind, String)>,
    /// The worker and issue rows the target holds and the backup lacks, deleted last
    pub delete_issues: Vec<(String, String, String, String)>,
    /// The rows written over the target's: the backup's, and the Restored entry, with the revision
    /// row last
    pub migrations: Vec<SchemaMigrationsTable>,
    /// The worker and issue rows written over the target's
    pub issues: Vec<SchemaMigrationIssuesTable>,
    /// The rows of kinds this thoradm has no variant for, written back beside the target's own
    /// rows of those kinds, since they are audit rows and a restore never discards one
    pub other: Vec<RawRow>,
    /// The revision the target had recorded, which the restore replaced
    pub replaced_revision: Option<String>,
}

/// What a restore did with the schema record, for its summary
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordRestored {
    /// The revision the record was restored at
    pub revision: Option<String>,
    /// The revision the target recorded before the restore
    pub replaced_revision: Option<String>,
    /// How many history entries the restore kept as archived history
    pub archived: usize,
    /// How many `schema_migrations` rows it wrote, the Restored entry included
    pub rows: usize,
    /// How many `schema_migration_issues` rows it wrote
    pub issues: usize,
    /// Whether the backup held no record, so the restore recorded the baseline revision its data
    /// is at instead of the backup's record
    #[serde(default)]
    pub baseline: bool,
    /// The steps the operator plans from the restored revision up to the one the target recorded,
    /// as `<revision> <step>`, when the restore recorded the baseline
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planned: Vec<String>,
}

/// The kinds of row a restore writes back from the record: the data-shaping settings stay the
/// restoring cluster's, and a Job's result is never in a record; a legacy drop's audit row is
/// written back beside the target's own, never replacing them
const WRITTEN_BACK: [MigrationRecordKind; 7] = [
    MigrationRecordKind::Backend,
    MigrationRecordKind::Run,
    MigrationRecordKind::Revision,
    MigrationRecordKind::History,
    MigrationRecordKind::ConsumedCode,
    MigrationRecordKind::ArchivedHistory,
    MigrationRecordKind::LegacyDrop,
];

/// The kinds of row a restore replaces outright
const REPLACED: [MigrationRecordKind; 3] = [
    MigrationRecordKind::Backend,
    MigrationRecordKind::Run,
    MigrationRecordKind::History,
];

/// Returns the objects a restore leaves uncreated where they don't exist: the tables and views only
/// keyspaces older releases created hold
#[must_use]
pub fn skipped_objects() -> BTreeSet<String> {
    // the class table marks them legacy
    CLASSES
        .iter()
        .filter(|entry| {
            entry.legacy && matches!(entry.store, Store::ScyllaTable | Store::ScyllaView)
        })
        .map(|entry| entry.name.to_owned())
        .collect()
}

/// Decides what a restore converges the keyspace to
///
/// # Arguments
///
/// * `record` - The backup's schema record, if it has one
pub fn converge_target(record: Option<&BackupSchemaRecord>) -> Result<ConvergeTo, String> {
    // a backup with no record needs only the tables this build declares
    let Some(record) = record else {
        return Ok(ConvergeTo::Declared);
    };
    // the revision the record names, or the one whose snapshot has the recorded fingerprint
    let revision = record.revision().map(ToOwned::to_owned).or_else(|| {
        let fingerprint = record.deployment_fingerprint()?;
        RevisionSnapshot::committed_ids()
            .filter_map(RevisionSnapshot::committed)
            .filter_map(Result::ok)
            .filter(|snapshot| snapshot.deployment.as_str() == fingerprint)
            .map(|snapshot| snapshot.revision.as_str().to_owned())
            .last()
    });
    let Some(revision) = revision else {
        return Ok(ConvergeTo::Declared);
    };
    // this build can only converge to a revision whose snapshot it carries
    match RevisionSnapshot::committed(&revision) {
        Some(Ok(snapshot)) => Ok(ConvergeTo::Snapshot(Box::new(snapshot))),
        Some(Err(error)) => Err(format!(
            "this thoradm's snapshot of revision {revision} is unreadable ({error:?}), so it can't \
             converge the keyspace to the backup's revision"
        )),
        None => Err(format!(
            "the backup's schema record is at revision {revision}, which this thoradm has no \
             snapshot of; restore it with the thoradm version that took the backup{}",
            record
                .thoradm_version
                .as_ref()
                .map_or_else(String::new, |version| format!(" ({version})"))
        )),
    }
}

/// Says whether a backup's record can replace the target's: the backup holds every non-S3
/// component (every Scylla table and Redis) and the record names a revision
///
/// The record's revision, history, and fingerprints describe the whole deployment, so a backup of
/// some tables says nothing the target's own record doesn't say better, and restoring it would
/// rewind the target's revision under data it never touched. Backed-up S3 components don't count:
/// a backup without them still holds the data the record's layouts describe.
///
/// # Arguments
///
/// * `record` - The backup's schema record
/// * `components` - The components the backup holds
pub fn record_trusted(
    record: &BackupSchemaRecord,
    components: &[BackupComponents],
) -> Result<(), String> {
    // a record that names no revision can't say where the deployment was
    if record.revision().is_none() {
        return Err(
            "the schema record was not restored: the backup's record names no revision, \
                    so the target's revision and history stay as they are"
                .to_owned(),
        );
    }
    // every Scylla table and Redis must be in the backup
    let missing = missing_state(components);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the schema record was not restored: the backup lacks {}, so it says nothing about \
             them, and the target's revision, history, and fingerprints stay as they are",
            missing.join(", ")
        ))
    }
}

/// Lists the Scylla tables and Redis a backup lacks: the components that make up the deployment's
/// state, without which a backup says nothing about the revision the rest of its data is at
///
/// # Arguments
///
/// * `components` - The components the backup holds
#[must_use]
pub fn missing_state(components: &[BackupComponents]) -> Vec<String> {
    // every component that isn't a filter or in S3
    BackupComponents::iter()
        .filter(|component| !component.is_filter() && !component.is_in_s3())
        .filter(|component| !components.contains(component))
        .map(|component| component.to_string())
        .collect()
}

/// Decides the revision a restore records for a backup that holds no schema record
///
/// A backup without a record was taken before revisions were recorded, or of a keyspace that
/// never recorded one, so its Scylla and Redis rows are in the layouts of [`FIRST_HELM_REVISION`],
/// the revision the operator also suggests for a Helm deployment with data and no record. No row
/// tells a pre-Helm backup from a first Helm one, and none needs to: the edge from the pre-Helm
/// baseline changes only Elastic, which a backup doesn't hold, and checks credentials. Recording
/// that revision under a target at a newer one makes the operator plan every step after it, and
/// each of them checks every object first and leaves the ones already converted as they are, so
/// rows already in a newer layout, such as a merge target's, are safe.
///
/// Returns `None` when the target records no revision, or none newer than the baseline, since its
/// record already says as much, and refuses when this release no longer upgrades from the
/// baseline, naming the release to restore with first.
///
/// # Arguments
///
/// * `recorded` - The revision the target records, if it records one
/// * `catalog` - The catalog whose support floors say which revisions this release upgrades from
pub fn recordless_baseline(
    recorded: Option<&str>,
    catalog: &Catalog,
) -> Result<Option<RevisionId>, String> {
    // the baseline is a catalog id, which the catalog tests check is well formed
    let baseline = FIRST_HELM_REVISION
        .parse::<RevisionId>()
        .map_err(|error| format!("the baseline revision {FIRST_HELM_REVISION}: {error}"))?;
    // a target recording nothing newer than the baseline keeps its record
    let Some(recorded) = recorded.and_then(|recorded| recorded.parse::<RevisionId>().ok()) else {
        return Ok(None);
    };
    if recorded <= baseline {
        return Ok(None);
    }
    // the operator plans from the baseline only while this release still upgrades from it
    match catalog.check_floor(&baseline) {
        Ok(()) => Ok(Some(baseline)),
        Err(UpgradeError::BelowMinimum {
            minimum,
            intermediate,
            ..
        }) => Err(format!(
            "the backup holds no schema record, so its data is in the layouts of revision \
             {baseline}, which deployments from before revisions were recorded are at, but this \
             release upgrades only from revision {minimum} on and the target records {recorded}: \
             restore the backup with the thoradm of Thorium {intermediate} into a cluster of that \
             release, let its operator finish upgrading the data, then take a new backup and \
             restore that one with this release"
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// Builds the record a restore writes for a backup that holds none: the baseline revision its data
/// is at, and the target's own backend rows, since the restore applied the target's schema
///
/// Written like a backup's record, it replaces the target's revision, history, passes, and issue
/// rows, archiving the history, and ends the history with a Restored entry at the baseline.
///
/// # Arguments
///
/// * `baseline` - The revision the backup's data is at
/// * `backends` - The target's backend rows, read before the restore's applies
/// * `at` - When the restore writes the record
#[must_use]
pub fn baseline_record(
    baseline: &RevisionId,
    backends: &[SchemaMigrationsTable],
    at: DateTime<Utc>,
) -> BackupSchemaRecord {
    // the revision row names the baseline and the thoradm that recorded it
    let revision = SchemaMigrationsTable {
        kind: MigrationRecordKind::Revision,
        id: REVISION_ROW.to_owned(),
        revision: Some(baseline.to_string()),
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        updated: at,
        ..blank_row()
    };
    // the target's fingerprints and layouts stay, written back over the restore's applies
    let mut migrations: Vec<SchemaMigrationsTable> = backends
        .iter()
        .filter(|row| row.kind == MigrationRecordKind::Backend)
        .cloned()
        .collect();
    migrations.push(revision);
    BackupSchemaRecord {
        format: RECORD_FORMAT,
        thoradm_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        migrations,
        issues: Vec::new(),
        other: Vec::new(),
    }
}

/// Lists the steps the operator plans once a restore lowered the recorded revision: every step
/// from the restored revision up to the one the target had recorded, as `<revision> <step>`
///
/// A restore that didn't lower the revision plans nothing, and so does one the catalog can't plan
/// from, whose reason the operator reports in the cluster's status.
///
/// # Arguments
///
/// * `restored` - The revision the restore recorded
/// * `replaced` - The revision the target had recorded
/// * `catalog` - The catalog the operator plans over
#[must_use]
pub fn planned_after(
    restored: &RevisionId,
    replaced: &RevisionId,
    catalog: &Catalog,
) -> Vec<String> {
    // nothing is left when the revision didn't go down
    if replaced <= restored {
        return Vec::new();
    }
    // the restored history finishes no step after the restored revision
    let state = UpgradeState::new(restored.clone(), env!("CARGO_PKG_VERSION"));
    catalog.plan(&state, replaced).map_or_else(
        |_| Vec::new(),
        |steps| {
            steps
                .iter()
                .map(|planned| format!("{} {}", planned.revision, planned.step))
                .collect()
        },
    )
}

/// Reads the revision the target records, if its keyspace holds the record table and a revision
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `namespace` - The keyspace
pub async fn read_revision(scylla: &Session, namespace: &str) -> Result<Option<String>, Error> {
    // a keyspace without the record table records nothing
    let table = <SchemaMigrationsTable as ScyllaObject>::NAME;
    if !super::schema::has_table(scylla, namespace, table).await? {
        return Ok(None);
    }
    // the revision row holds the revision in its own cell
    let mut rows = scylla
        .query_iter(
            format!("SELECT revision FROM {namespace}.{table} WHERE kind = ? AND id = ?"),
            (MigrationRecordKind::Revision.as_str(), REVISION_ROW),
        )
        .await?
        .rows_stream::<(Option<String>,)>()
        .map_err(|error| Error::new(format!("failed to read {table}: {error}")))?;
    let mut revision = None;
    while let Some(row) = rows.next().await {
        let (recorded,) =
            row.map_err(|error| Error::new(format!("failed to read a {table} row: {error}")))?;
        revision = recorded;
    }
    Ok(revision)
}

/// Says whether a restore of these components restores stored files, so the files password
/// matters
///
/// # Arguments
///
/// * `components` - The components the backup holds
#[must_use]
pub fn restores_files(components: &[BackupComponents]) -> bool {
    // the files table and the files bucket are the s3-ids components
    components.iter().any(|component| {
        matches!(
            component,
            BackupComponents::S3Ids | BackupComponents::S3IdsObjects
        )
    })
}

/// Checks the configured files password against the identity the backup recorded
///
/// Stored files are restored as they were encrypted, so a restore into a cluster with another
/// password would leave files nothing opens. The reason names the setting and never a password or
/// a hash.
///
/// # Arguments
///
/// * `record` - The backup's schema record
/// * `conf` - The config of the cluster being restored
pub fn check_files_password(record: &BackupSchemaRecord, conf: &Conf) -> Result<(), String> {
    // a backup that recorded no identity can't be checked
    let Some(identity) = record.files_password() else {
        return Ok(());
    };
    match verify_files_password(identity, &conf.thorium.files.password) {
        Ok(true) => Ok(()),
        Ok(false) => Err(
            "`files.password` differs from the password the backup was taken with; stored files \
             are restored as they were encrypted, so they would not open: restore into a cluster \
             configured with the original `files.password`"
                .to_owned(),
        ),
        Err(_) => Err(
            "the backup's recorded `files.password` identity isn't argon2id with the default \
             parameters, so it isn't verified; the backup is refused"
                .to_owned(),
        ),
    }
}

/// Builds the Restored entry a restore appends to the restored history
///
/// # Arguments
///
/// * `record` - The backup's schema record
/// * `replaced` - The revision the target had recorded
/// * `origin` - Where the record comes from and who restored it
fn restored_entry(
    record: &BackupSchemaRecord,
    replaced: Option<&str>,
    origin: &Origin<'_>,
) -> Option<AppliedStep> {
    // the entry belongs to the backup's revision, so every later step counts as unfinished
    let revision = record.revision()?;
    // a baseline says why the restore recorded it
    let message = if origin.baseline {
        format!(
            "restored from backup {}, which holds no schema record, at revision {revision}, \
             whose layouts its data is in, replacing revision {}",
            origin.reference,
            replaced.unwrap_or("none")
        )
    } else {
        format!(
            "restored from backup {} at revision {revision}, replacing revision {}",
            origin.reference,
            replaced.unwrap_or("none")
        )
    };
    Some(AppliedStep {
        revision: revision.to_owned(),
        step: RESTORED_STEP.to_owned(),
        outcome: StepState::Done,
        message: Some(message),
        at: Some(origin.at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        by: Some(origin.by.to_owned()),
        backup: Some(origin.reference.to_owned()),
        ..AppliedStep::default()
    })
}

/// Plans the writes that put a backup's record in a target, keeping the history it replaces
///
/// # Arguments
///
/// * `existing` - What the target holds now
/// * `record` - The backup's schema record
/// * `origin` - Where the record comes from and who restored it
#[must_use]
pub fn plan_record(
    existing: &Existing,
    record: &BackupSchemaRecord,
    origin: &Origin<'_>,
) -> RecordPlan {
    let when = origin.at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    // every replaced history entry is kept as archived history under the backup's stamp, so the
    // keys are the same however often the restore runs
    let archive = existing
        .history
        .iter()
        .map(|row| SchemaMigrationsTable {
            kind: MigrationRecordKind::ArchivedHistory,
            id: format!("{}/{}", origin.stamp, row.id),
            updated: origin.at,
            ..row.clone()
        })
        .collect();
    // the backup's rows come back as they were, except the settings and Job results
    let mut migrations: Vec<SchemaMigrationsTable> = record
        .migrations
        .iter()
        .filter(|row| WRITTEN_BACK.contains(&row.kind))
        .cloned()
        .collect();
    // and the history ends with the Restored entry
    if let Some(entry) = restored_entry(record, existing.revision.as_deref(), origin) {
        let revision = entry.revision.clone();
        migrations.push(SchemaMigrationsTable {
            kind: MigrationRecordKind::History,
            id: format!("{when}/{revision}/{RESTORED_STEP}/done"),
            entry: Some(thorium::models::schema::JsonText(entry)),
            updated: origin.at,
            ..blank_row()
        });
    }
    // the revision row goes last, after every other row, so an interrupted restore never leaves
    // tables with no revision recorded
    migrations.sort_by_key(|row| row.kind == MigrationRecordKind::Revision);
    // a row of a replaced kind the target holds and the backup lacks is stale
    let written: BTreeSet<(&str, &str)> = migrations
        .iter()
        .map(|row| (row.kind.as_str(), row.id.as_str()))
        .collect();
    let held = existing
        .rows
        .iter()
        .map(|(kind, id)| (*kind, id.clone()))
        .chain(
            existing
                .history
                .iter()
                .map(|row| (row.kind, row.id.clone())),
        );
    let delete_rows = held
        .filter(|(kind, id)| {
            REPLACED.contains(kind) && !written.contains(&(kind.as_str(), id.as_str()))
        })
        .collect();
    // and so is a worker or issue row the backup lacks
    let kept: BTreeSet<(&str, &str, &str, &str)> = record
        .issues
        .iter()
        .map(|row| {
            (
                row.migration.as_str(),
                row.transform.as_str(),
                row.reason.as_str(),
                row.worker.as_str(),
            )
        })
        .collect();
    let delete_issues = existing
        .issue_keys
        .iter()
        .filter(|(migration, transform, reason, worker)| {
            !kept.contains(&(
                migration.as_str(),
                transform.as_str(),
                reason.as_str(),
                worker.as_str(),
            ))
        })
        .cloned()
        .collect();
    RecordPlan {
        archive,
        delete_rows,
        delete_issues,
        migrations,
        issues: record.issues.clone(),
        other: record.other.clone(),
        replaced_revision: existing.revision.clone(),
    }
}

/// A `schema_migrations` row with nothing set, to build rows from
fn blank_row() -> SchemaMigrationsTable {
    // every optional cell unset
    SchemaMigrationsTable {
        kind: MigrationRecordKind::History,
        id: String::new(),
        fingerprint: None,
        apply_key: None,
        layouts: std::collections::BTreeMap::new(),
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
        updated: DateTime::<Utc>::UNIX_EPOCH,
    }
}

/// Converts an applier error into thoradm's error
///
/// # Arguments
///
/// * `error` - The applier's error
fn apply_error(error: ApplyError) -> Error {
    match error {
        // a failed statement may succeed when the restore runs again
        ApplyError::Query { .. } => Error::Transient(error.to_string()),
        other => Error::new(format!("schema apply failed: {other}")),
    }
}

/// Converges the target's Scylla schema to what a restore needs
///
/// # Arguments
///
/// * `scylla` - The session to apply with
/// * `conf` - The config of the cluster being restored
/// * `to` - What to converge to
/// * `progress` - Receives a line for each progress report while views build
pub async fn converge(
    scylla: &Session,
    conf: &Conf,
    to: &ConvergeTo,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, Error> {
    match to {
        // the backup's revision, leaving out the objects no release reads
        ConvergeTo::Snapshot(snapshot) => {
            let run = SnapshotRun {
                skip: skipped_objects(),
                ..SnapshotRun::default()
            };
            apply_snapshot(scylla, conf, snapshot, &run, progress)
                .await
                .map_err(apply_error)
        }
        // this build's declarations
        ConvergeTo::Declared => {
            let options = ApplyOptions {
                target: ApplyTarget::Declared,
                dry_run: false,
            };
            apply(scylla, conf, &options, "", progress)
                .await
                .map_err(apply_error)
        }
    }
}

/// Reads what the target holds in the record tables that a restore replaces
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `namespace` - The keyspace
pub async fn read_existing(scylla: &Session, namespace: &str) -> Result<Existing, Error> {
    let table = <SchemaMigrationsTable as ScyllaObject>::NAME;
    let columns = <SchemaMigrationRow as Projection>::COLUMNS.join(", ");
    // the history rows, and the revision row
    let mut existing = Existing::default();
    let cql = format!("SELECT {columns} FROM {namespace}.{table} WHERE kind = ?");
    for kind in [MigrationRecordKind::History, MigrationRecordKind::Revision] {
        let mut rows = scylla
            .query_iter(cql.as_str(), (kind.as_str(),))
            .await?
            .rows_stream::<SchemaMigrationRow>()
            .map_err(|error| Error::new(format!("failed to read {table}: {error}")))?;
        while let Some(row) = rows.next().await {
            let row =
                migrations_row(row.map_err(|error| {
                    Error::new(format!("failed to read a {table} row: {error}"))
                })?);
            match kind {
                MigrationRecordKind::History => existing.history.push(row),
                _ => existing.revision = row.revision,
            }
        }
    }
    // the ids of the backend and run rows
    for kind in [MigrationRecordKind::Backend, MigrationRecordKind::Run] {
        let mut ids = scylla
            .query_iter(
                format!("SELECT id FROM {namespace}.{table} WHERE kind = ?"),
                (kind.as_str(),),
            )
            .await?
            .rows_stream::<(String,)>()
            .map_err(|error| Error::new(format!("failed to read {table}: {error}")))?;
        while let Some(row) = ids.next().await {
            let (id,) =
                row.map_err(|error| Error::new(format!("failed to read a {table} row: {error}")))?;
            existing.rows.push((kind, id));
        }
    }
    // the keys of the worker and issue rows
    let issues = <SchemaMigrationIssuesTable as ScyllaObject>::NAME;
    let mut keys = scylla
        .query_iter(
            format!("SELECT migration, transform, reason, worker FROM {namespace}.{issues}"),
            (),
        )
        .await?
        .rows_stream::<(String, String, String, String)>()
        .map_err(|error| Error::new(format!("failed to read {issues}: {error}")))?;
    while let Some(row) = keys.next().await {
        existing.issue_keys.insert(
            row.map_err(|error| Error::new(format!("failed to read an {issues} row: {error}")))?,
        );
    }
    Ok(existing)
}

/// The most rows one batch writes
const BATCH_ROWS: usize = 50;

/// Writes rows that share a partition with single-partition batches, which Scylla applies whole
///
/// # Arguments
///
/// * `scylla` - The session to write with
/// * `insert` - The prepared insert every row binds
/// * `rows` - The rows, all in one partition
async fn write_partition<R: scylla::serialize::row::SerializeRow>(
    scylla: &Session,
    insert: &scylla::statement::prepared::PreparedStatement,
    rows: &[&R],
) -> Result<(), Error> {
    // a batch per chunk, so no batch outgrows the limit
    for chunk in rows.chunks(BATCH_ROWS) {
        if let [only] = chunk {
            scylla.execute_unpaged(insert, only).await?;
            continue;
        }
        let mut batch = Batch::new(BatchType::Unlogged);
        for _ in chunk {
            batch.append_statement(insert.clone());
        }
        scylla.batch(&batch, chunk.to_vec()).await?;
    }
    Ok(())
}

/// Writes a backup's record into the target's record tables, keeping the history it replaces
///
/// Raw rows go first, since a row no write can take should fail before anything changes. The
/// replaced history is then written as archived history, every row of the record is written over
/// the target's one partition at a time, with the revision row last, and only then are the rows
/// the backup lacks deleted. A restore that stops partway therefore leaves the old record, with
/// some of the new rows beside it, and one run again finishes the job under the same archive keys.
///
/// # Arguments
///
/// * `scylla` - The session to write with
/// * `namespace` - The keyspace
/// * `record` - The backup's schema record
/// * `origin` - Where the record comes from and who restored it
pub async fn restore_record(
    scylla: &Session,
    namespace: &str,
    record: &BackupSchemaRecord,
    origin: &Origin<'_>,
) -> Result<RecordRestored, Error> {
    // plan against what the target holds now
    let existing = read_existing(scylla, namespace).await?;
    let plan = plan_record(&existing, record, origin);
    let table = <SchemaMigrationsTable as ScyllaObject>::NAME;
    let issues = <SchemaMigrationIssuesTable as ScyllaObject>::NAME;
    let insert_migrations = scylla
        .prepare(with_keyspace(SchemaMigrationsTable::INSERT, namespace))
        .await?;
    let insert_issues = scylla
        .prepare(with_keyspace(SchemaMigrationIssuesTable::INSERT, namespace))
        .await?;
    // the rows of kinds this thoradm doesn't know, one partition at a time
    let mut others: BTreeMap<&str, Vec<&RawRow>> = BTreeMap::new();
    for row in &plan.other {
        others.entry(row.kind.as_str()).or_default().push(row);
    }
    for rows in others.values() {
        write_partition(scylla, &insert_migrations, rows).await?;
    }
    // the replaced history, kept before it is replaced
    let archive: Vec<&SchemaMigrationsTable> = plan.archive.iter().collect();
    write_partition(scylla, &insert_migrations, &archive).await?;
    // the worker and issue rows, one pass at a time
    let mut passes: BTreeMap<&str, Vec<&SchemaMigrationIssuesTable>> = BTreeMap::new();
    for row in &plan.issues {
        passes.entry(row.migration.as_str()).or_default().push(row);
    }
    for rows in passes.values() {
        write_partition(scylla, &insert_issues, rows).await?;
    }
    // the record's rows, one kind at a time, in the order the plan lists them, which ends with
    // the revision row
    let mut kinds: Vec<(MigrationRecordKind, Vec<&SchemaMigrationsTable>)> = Vec::new();
    for row in &plan.migrations {
        match kinds.iter_mut().find(|(kind, _)| *kind == row.kind) {
            Some((_, rows)) => rows.push(row),
            None => kinds.push((row.kind, vec![row])),
        }
    }
    for (_, rows) in &kinds {
        write_partition(scylla, &insert_migrations, rows).await?;
    }
    // the rows the backup lacks go last
    for (kind, id) in &plan.delete_rows {
        scylla
            .query_unpaged(
                format!("DELETE FROM {namespace}.{table} WHERE kind = ? AND id = ?"),
                (kind.as_str(), id),
            )
            .await?;
    }
    for key in &plan.delete_issues {
        scylla
            .query_unpaged(
                format!(
                    "DELETE FROM {namespace}.{issues} WHERE migration = ? AND transform = ? \
                     AND reason = ? AND worker = ?"
                ),
                key,
            )
            .await?;
    }
    Ok(RecordRestored {
        revision: record.revision().map(ToOwned::to_owned),
        replaced_revision: plan.replaced_revision,
        archived: plan.archive.len(),
        rows: plan.migrations.len() + plan.other.len(),
        issues: plan.issues.len(),
        baseline: origin.baseline,
        planned: Vec::new(),
    })
}

/// Reads the backend rows of the target, which a restore that leaves the record alone puts back
/// after its `schema apply` records fingerprints over them
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `namespace` - The keyspace
pub async fn read_backends(
    scylla: &Session,
    namespace: &str,
) -> Result<Vec<SchemaMigrationsTable>, Error> {
    // a keyspace without the record table has none
    let table = <SchemaMigrationsTable as ScyllaObject>::NAME;
    if !super::schema::has_table(scylla, namespace, table).await? {
        return Ok(Vec::new());
    }
    let columns = <SchemaMigrationRow as Projection>::COLUMNS.join(", ");
    let mut rows = scylla
        .query_iter(
            format!("SELECT {columns} FROM {namespace}.{table} WHERE kind = ?"),
            (MigrationRecordKind::Backend.as_str(),),
        )
        .await?
        .rows_stream::<SchemaMigrationRow>()
        .map_err(|error| Error::new(format!("failed to read {table}: {error}")))?;
    let mut backends = Vec::new();
    while let Some(row) = rows.next().await {
        backends.push(migrations_row(row.map_err(|error| {
            Error::new(format!("failed to read a {table} row: {error}"))
        })?));
    }
    Ok(backends)
}

/// Writes backend rows back over what `schema apply` recorded
///
/// # Arguments
///
/// * `scylla` - The session to write with
/// * `namespace` - The keyspace
/// * `backends` - The rows [`read_backends`] returned
pub async fn put_back_backends(
    scylla: &Session,
    namespace: &str,
    backends: &[SchemaMigrationsTable],
) -> Result<(), Error> {
    // the target's own fingerprints stand, since the record wasn't restored
    let insert = scylla
        .prepare(with_keyspace(SchemaMigrationsTable::INSERT, namespace))
        .await?;
    let rows: Vec<&SchemaMigrationsTable> = backends.iter().collect();
    write_partition(scylla, &insert, &rows).await
}

/// Names who is restoring: thoradm's version and the OS user
#[must_use]
pub fn restored_by() -> String {
    // the login name the shell gives, or unknown where none is set
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    format!("thoradm {} ({user})", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::collections::BTreeSet;
    use thorium::Conf;
    use thorium::models::schema::{MigrationRecordKind, hash_files_password};

    use super::{
        ConvergeTo, Existing, Origin, RecordPlan, baseline_record, blank_row, check_files_password,
        converge_target, plan_record, planned_after, record_trusted, recordless_baseline,
        restores_files, skipped_objects,
    };
    use crate::backup::schema::BackupSchemaRecord;
    use thorium::models::upgrades::{CATALOG, Catalog, RevisionId, SupportFloor};

    /// The name serde gives a record kind, which is the variant's name
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind as the table stores it
    fn serde_kind(kind: &str) -> String {
        // each stored word starts a capital
        kind.split('_')
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
            })
            .collect()
    }

    /// One `schema_migrations` row as JSON
    ///
    /// # Arguments
    ///
    /// * `kind` - The row's kind
    /// * `id` - The row's id
    /// * `cells` - The cells to set, as a JSON object
    fn row(kind: &str, id: &str, cells: &serde_json::Value) -> serde_json::Value {
        // every column of the table, unset unless a cell sets it
        let mut row = json!({
            "kind": serde_kind(kind), "id": id, "fingerprint": null, "apply_key": null, "layouts": {},
            "revision": null, "version": null, "entry": null, "result": null, "value": null,
            "status": null, "attempt": null, "lease": null, "started": null, "finished": null,
            "job": null, "backup": null, "updated": "2026-10-01T00:01:00Z",
        });
        for (name, value) in cells.as_object().unwrap() {
            row[name] = value.clone();
        }
        row
    }

    /// Builds a record from rows
    ///
    /// # Arguments
    ///
    /// * `rows` - The `schema_migrations` rows
    fn record(rows: &[serde_json::Value]) -> BackupSchemaRecord {
        // a record written by this thoradm
        serde_json::from_value(json!({
            "format": 1, "thoradm_version": "1.8.1", "migrations": rows,
            "issues": [{
                "migration": "pass", "transform": "~worker", "reason": "~", "worker": "w00",
                "attempt": "a", "checkpoint": null, "lease": null, "class": null, "count": 0,
                "first_seen": null, "last_seen": null, "samples": [], "resolved_count": 0,
                "accepted": false, "updated": "2026-10-01T00:01:00Z",
            }],
        }))
        .unwrap()
    }

    /// A history entry as a row's `entry` cell
    ///
    /// # Arguments
    ///
    /// * `revision` - The entry's revision
    /// * `step` - The step
    fn entry(revision: &str, step: &str) -> serde_json::Value {
        // a finished step
        json!({"entry": {"revision": revision, "step": step, "outcome": "Done"}})
    }

    /// Where the tests' record comes from
    fn origin() -> Origin<'static> {
        // a restore at a fixed time
        Origin {
            reference: "/backups/one",
            by: "thoradm 1.8.1 (tester)",
            at: Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap(),
            stamp: "2026-10-19T08:00:00.000000000Z",
            baseline: false,
        }
    }

    /// The target's rows are what the restore replaces: a record naming a revision converges to
    /// that revision's snapshot, one that names none converges to the snapshot whose fingerprint it
    /// recorded, and one this build can't place is refused or left to the declarations
    #[test]
    fn the_target_of_a_restore_follows_the_record() {
        // no record: this build's declarations
        assert!(matches!(converge_target(None), Ok(ConvergeTo::Declared)));
        // a revision with a committed snapshot
        let named = record(&[row(
            "revision",
            "deployment",
            &json!({"revision": "2026-10-v02"}),
        )]);
        match converge_target(Some(&named)) {
            Ok(ConvergeTo::Snapshot(snapshot)) => {
                assert_eq!(snapshot.revision.as_str(), "2026-10-v02");
            }
            other => panic!("{other:?}"),
        }
        // no revision row, but the fingerprint of a committed snapshot
        let committed = thorium::models::schema::RevisionSnapshot::committed("2026-10-v01")
            .unwrap()
            .unwrap();
        let by_fingerprint = record(&[row(
            "backend",
            "deployment",
            &json!({"fingerprint": committed.deployment.as_str()}),
        )]);
        assert!(matches!(
            converge_target(Some(&by_fingerprint)),
            Ok(ConvergeTo::Snapshot(_))
        ));
        // no revision and a fingerprint no snapshot has: the declarations
        let unknown = record(&[row(
            "backend",
            "deployment",
            &json!({"fingerprint": "nothing"}),
        )]);
        assert!(matches!(
            converge_target(Some(&unknown)),
            Ok(ConvergeTo::Declared)
        ));
        // a revision this build has no snapshot of is refused, naming it and the thoradm to use
        let newer = record(&[row(
            "revision",
            "deployment",
            &json!({"revision": "2026-10-v99"}),
        )]);
        let reason = converge_target(Some(&newer)).unwrap_err();
        assert!(
            reason.contains("2026-10-v99") && reason.contains("1.8.1"),
            "{reason}"
        );
    }

    /// The configured files password must verify against the backup's recorded identity, and a
    /// refusal prints neither password
    #[test]
    fn the_files_password_is_checked_against_the_recorded_identity() {
        // a config with a known files password
        let mut conf =
            Conf::new("../api/tests/thorium-testing.yml").expect("the test config loads");
        conf.thorium.files.password = "the-configured-password".to_owned();
        let with = |identity: &str| {
            record(&[row("config", "files.password", &json!({"value": identity}))])
        };
        // a backup that recorded no identity can't be checked
        assert!(check_files_password(&record(&[]), &conf).is_ok());
        // the same password verifies
        let same = hash_files_password("the-configured-password").unwrap();
        assert!(check_files_password(&with(&same), &conf).is_ok());
        // another password is refused, naming the setting and printing no secret
        let other = hash_files_password("a-different-password").unwrap();
        let reason = check_files_password(&with(&other), &conf).unwrap_err();
        assert!(reason.contains("files.password"), "{reason}");
        for secret in [
            "the-configured-password",
            "a-different-password",
            other.as_str(),
        ] {
            assert!(!reason.contains(secret), "{reason}");
        }
        // an identity with other parameters is refused before any hash is computed
        let weak = "$argon2id$v=19$m=4294967295,t=4294967295,p=255$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2g";
        let reason = check_files_password(&with(weak), &conf).unwrap_err();
        assert!(
            reason.contains("files.password") && !reason.contains(weak),
            "{reason}"
        );
    }

    /// Plans a restore of a backup at v01 into a target at v02 that holds two history entries and a
    /// pass
    fn planned() -> (Existing, BackupSchemaRecord, RecordPlan) {
        // a backup at v01 with one history entry, a setting, a consumed code, and archived history
        let backup = record(&[
            row(
                "revision",
                "deployment",
                &json!({"revision": "2026-10-v01"}),
            ),
            row(
                "history",
                "2026-10-01T00:00:00.000000000Z/2026-10-v01/first/done",
                &entry("2026-10-v01", "first"),
            ),
            row("config", "partition_size.files", &json!({"value": "7777"})),
            row("consumed_code", "CODE-OLD", &entry("2026-10-v01", "lossy")),
            row(
                "archived_history",
                "earlier/orig",
                &entry("2026-10-v01", "older"),
            ),
            row("backend", "deployment", &json!({"fingerprint": "fp"})),
            row("job_result", "uid", &json!({"result": {"outcome": "done"}})),
        ]);
        // a target at v02 with two history entries, and a pass
        let target = record(&[
            row(
                "history",
                "2026-10-02T00:00:00.000000000Z/2026-10-v02/a/done",
                &entry("2026-10-v02", "a"),
            ),
            row(
                "history",
                "2026-10-03T00:00:00.000000000Z/2026-10-v02/b/done",
                &entry("2026-10-v02", "b"),
            ),
        ]);
        let existing = Existing {
            history: target.migrations.clone(),
            revision: Some("2026-10-v02".to_owned()),
            rows: vec![
                (MigrationRecordKind::Backend, "stale-backend".to_owned()),
                (MigrationRecordKind::Backend, "deployment".to_owned()),
            ],
            issue_keys: BTreeSet::from([(
                "old-pass".to_owned(),
                "t".to_owned(),
                "r".to_owned(),
                "w".to_owned(),
            )]),
        };
        let plan = plan_record(&existing, &backup, &origin());
        (existing, backup, plan)
    }

    /// The plan keeps every replaced history entry as an archived row under the restore's time,
    /// replaces the kinds that make up the history and the passes the target holds, and writes
    /// back the backup's rows but its settings
    #[test]
    fn the_plan_archives_the_replaced_history_and_writes_back_the_rows() {
        let (existing, backup, plan) = planned();
        // each replaced entry is archived as `<restore time>/<original id>`, keeping its entry
        assert_eq!(plan.archive.len(), 2);
        for (archived, original) in plan.archive.iter().zip(&existing.history) {
            assert_eq!(archived.kind, MigrationRecordKind::ArchivedHistory);
            assert_eq!(
                archived.id,
                format!("2026-10-19T08:00:00.000000000Z/{}", original.id)
            );
            assert_eq!(archived.entry, original.entry);
        }
        // only the rows the backup lacks are deleted, and never the revision row
        let ids: Vec<String> = existing.history.iter().map(|row| row.id.clone()).collect();
        for id in &ids {
            assert!(
                plan.delete_rows
                    .contains(&(MigrationRecordKind::History, id.clone()))
            );
        }
        assert!(
            plan.delete_rows
                .contains(&(MigrationRecordKind::Backend, "stale-backend".to_owned()))
        );
        assert!(
            !plan
                .delete_rows
                .contains(&(MigrationRecordKind::Backend, "deployment".to_owned()))
        );
        assert!(
            plan.delete_rows
                .iter()
                .all(|(kind, _)| *kind != MigrationRecordKind::Revision)
        );
        assert_eq!(
            plan.delete_issues,
            [(
                "old-pass".to_owned(),
                "t".to_owned(),
                "r".to_owned(),
                "w".to_owned()
            )]
        );
        assert_eq!(plan.replaced_revision.as_deref(), Some("2026-10-v02"));
        // the backup's rows come back, but no setting and no Job result
        let written: BTreeSet<_> = plan
            .migrations
            .iter()
            .map(|row| format!("{}/{}", row.kind.as_str(), row.id))
            .collect();
        for expected in [
            "revision/deployment",
            "backend/deployment",
            "consumed_code/CODE-OLD",
            "archived_history/earlier/orig",
        ] {
            assert!(
                written.contains(expected),
                "{expected} missing from {written:?}"
            );
        }
        assert!(
            !written
                .iter()
                .any(|id| id.starts_with("config/") || id.starts_with("job_result/")),
            "{written:?}"
        );
        assert_eq!(plan.issues, backup.issues);
    }

    /// The history the plan writes ends with a Restored entry naming the backup, both revisions,
    /// and who restored
    #[test]
    fn the_plan_ends_the_history_with_a_restored_entry() {
        let (_, _, plan) = planned();
        let when = "2026-10-20T12:00:00.000000000Z";
        // the Restored entry names the backup, both revisions, and who restored
        let restored = plan
            .migrations
            .iter()
            .find(|row| row.id.ends_with("/restored/done"))
            .expect("a Restored entry");
        assert_eq!(restored.kind, MigrationRecordKind::History);
        assert_eq!(restored.id, format!("{when}/2026-10-v01/restored/done"));
        let applied = restored.entry.clone().unwrap().0;
        assert_eq!(
            (applied.step.as_str(), applied.revision.as_str()),
            ("restored", "2026-10-v01")
        );
        assert_eq!(applied.backup.as_deref(), Some("/backups/one"));
        assert_eq!(applied.by.as_deref(), Some("thoradm 1.8.1 (tester)"));
        let message = applied.message.unwrap();
        assert!(
            message.contains("/backups/one")
                && message.contains("2026-10-v01")
                && message.contains("2026-10-v02"),
            "{message}"
        );
        // it sorts after every entry the backup's history holds
        let newest = plan
            .migrations
            .iter()
            .filter(|row| row.kind == MigrationRecordKind::History)
            .map(|row| row.id.clone())
            .max();
        assert_eq!(newest, Some(restored.id.clone()));
    }

    /// A backup whose record names no revision gets no Restored entry, and a target that held no
    /// history archives nothing
    #[test]
    fn a_record_without_a_revision_gets_no_restored_entry() {
        // a record of rows that aren't a revision, into an empty target
        let backup = record(&[row("backend", "scylla", &json!({"fingerprint": "fp"}))]);
        let plan = plan_record(&Existing::default(), &backup, &origin());
        assert_eq!(plan.archive, Vec::new());
        assert!(plan.replaced_revision.is_none());
        assert!(
            !plan
                .migrations
                .iter()
                .any(|row| row.kind == MigrationRecordKind::History)
        );
        assert_eq!(plan.migrations.len(), 1);
    }

    /// The objects a restore leaves uncreated are the two only older releases created
    #[test]
    fn the_skipped_objects_are_the_legacy_ones() {
        // the class table's legacy table and view
        assert_eq!(
            skipped_objects(),
            BTreeSet::from(["events".to_owned(), "entities_by_name".to_owned()])
        );
    }
    /// The plan writes back the rows of kinds this thoradm doesn't know and deletes none of the
    /// target's, so audit rows of both survive
    #[test]
    fn the_plan_writes_back_unknown_rows_and_deletes_no_kind_of_them() {
        // a backup holding one raw audit row
        let mut backup = record(&[row(
            "revision",
            "deployment",
            &json!({"revision": "2026-10-v01"}),
        )]);
        backup.other = serde_json::from_value(json!([{
            "kind": "legacy_drop", "id": "x", "fingerprint": null, "apply_key": null,
            "layouts": null, "revision": null, "version": null, "entry": null, "result": null,
            "value": "v", "status": null, "attempt": null, "lease": null, "started": null,
            "finished": null, "job": null, "backup": null, "updated": null,
        }]))
        .unwrap();
        let plan = plan_record(&Existing::default(), &backup, &origin());
        assert_eq!(plan.other, backup.other);
        // no replaced kind is a name the raw rows could have
        assert_eq!(plan.delete_rows, Vec::new());
    }

    /// The revision row is written after every other row and is never deleted, so an interrupted
    /// restore doesn't leave tables with no revision
    #[test]
    fn the_revision_row_is_written_last() {
        // raw rows of the kinds an operator writes: history, backend, config, run, and a revision
        let backup = record(&[
            row(
                "revision",
                "deployment",
                &json!({"revision": "2026-10-v02"}),
            ),
            row("run", "pass", &json!({"revision": "2026-10-v02"})),
            row(
                "history",
                "2026-10-01T00:00:00.000000000Z/2026-10-v02/step/done",
                &entry("2026-10-v02", "step"),
            ),
            row("backend", "deployment", &json!({"fingerprint": "fp"})),
            row("config", "files.password", &json!({"value": "hash"})),
        ]);
        let plan = plan_record(&Existing::default(), &backup, &origin());
        // the revision row is last, the Restored entry is before it, and nothing deletes it first
        assert_eq!(
            plan.migrations.last().unwrap().kind,
            MigrationRecordKind::Revision
        );
        assert!(
            plan.migrations
                .iter()
                .any(|row| row.id.ends_with("/restored/done"))
        );
        assert_eq!(plan.delete_rows, Vec::new());
        // every history row is written back under its own id
        assert!(
            plan.migrations
                .iter()
                .any(|row| row.id == "2026-10-01T00:00:00.000000000Z/2026-10-v02/step/done")
        );
    }

    /// A record replaces the target's only when the backup holds every non-S3 component and the
    /// record names a revision
    #[test]
    fn a_record_is_trusted_only_from_a_whole_backup() {
        use crate::args::BackupComponents;
        use strum::IntoEnumIterator;
        let named = record(&[row(
            "revision",
            "deployment",
            &json!({"revision": "2026-10-v02"}),
        )]);
        // every component that isn't a filter or in S3
        let whole: Vec<BackupComponents> = BackupComponents::iter()
            .filter(|component| !component.is_filter() && !component.is_in_s3())
            .collect();
        assert!(record_trusted(&named, &whole).is_ok());
        // the S3 components don't matter
        let mut with_s3 = whole.clone();
        with_s3.push(BackupComponents::S3IdsObjects);
        assert!(record_trusted(&named, &with_s3).is_ok());
        // one table short, and the reason names it
        let short: Vec<BackupComponents> = whole
            .iter()
            .copied()
            .filter(|component| *component != BackupComponents::Entities)
            .collect();
        let reason = record_trusted(&named, &short).unwrap_err();
        assert!(reason.contains("entities"), "{reason}");
        assert!(record_trusted(&named, &[BackupComponents::Tags]).is_err());
        // a record with no revision row is left out too
        let unnamed = record(&[row("backend", "scylla", &json!({"fingerprint": "fp"}))]);
        let reason = record_trusted(&unnamed, &whole).unwrap_err();
        assert!(reason.contains("revision"), "{reason}");
    }

    /// The files password matters only to a restore that restores stored files
    #[test]
    fn the_files_password_matters_when_files_are_restored() {
        use crate::args::BackupComponents;
        assert!(!restores_files(&[
            BackupComponents::Tags,
            BackupComponents::Entities
        ]));
        assert!(restores_files(&[BackupComponents::S3Ids]));
        assert!(restores_files(&[BackupComponents::S3IdsObjects]));
    }

    /// A revision as the catalog parses it
    ///
    /// # Arguments
    ///
    /// * `id` - The revision's id
    fn rev(id: &str) -> RevisionId {
        // every id the tests name is well formed
        id.parse().unwrap()
    }

    /// A backup without a record is at the first Helm revision, which the restore records only
    /// over a target that records a newer one
    #[test]
    fn a_backup_without_a_record_is_at_the_first_helm_revision() {
        let production = Catalog::PRODUCTION;
        // a target at a newer revision, such as a fresh install at the third revision or the latest
        assert_eq!(
            recordless_baseline(Some("2026-10-v03"), &production),
            Ok(Some(rev("2026-10-v02")))
        );
        let latest = production.latest();
        assert_eq!(
            recordless_baseline(Some(latest.as_str()), &production),
            Ok(Some(rev("2026-10-v02")))
        );
        // a target at the baseline or below it, recording nothing, or naming no revision this
        // build parses already says as much, so it keeps its record
        for recorded in [
            Some("2026-10-v02"),
            Some("2026-10-v01"),
            None,
            Some("not-a-revision"),
        ] {
            assert_eq!(
                recordless_baseline(recorded, &production),
                Ok(None),
                "{recorded:?}"
            );
        }
    }

    /// A release that no longer upgrades from the baseline refuses a backup without a record,
    /// naming the baseline and the release to restore it with first
    #[test]
    fn a_baseline_outside_the_support_window_is_refused() {
        // a catalog whose floor was raised above the baseline
        let floored = Catalog {
            edges: CATALOG,
            floors: &[SupportFloor {
                minimum: "2026-10-v03",
                intermediate: "9.9.9",
                since_version: "10.0.0",
            }],
        };
        let reason = recordless_baseline(Some("2026-10-v03"), &floored).unwrap_err();
        for expected in ["schema record", "2026-10-v02", "2026-10-v03", "9.9.9"] {
            assert!(reason.contains(expected), "{expected} missing: {reason}");
        }
        // a target the baseline doesn't replace keeps its record whatever the floor
        assert_eq!(recordless_baseline(Some("2026-10-v02"), &floored), Ok(None));
    }

    /// The baseline's record replaces the target's revision and history like a backup's record,
    /// archiving the history, but keeps the target's backend rows, since the restore applied the
    /// target's schema
    #[test]
    fn the_baseline_record_replaces_the_revision_and_keeps_the_fingerprints() {
        // a target at the third revision with two history entries, a pass, and two backends
        let target = record(&[
            row(
                "history",
                "2026-10-02T00:00:00.000000000Z/2026-10-v03/schema-apply/done",
                &entry("2026-10-v03", "schema-apply"),
            ),
            row(
                "history",
                "2026-10-03T00:00:00.000000000Z/2026-10-v03/finalize/done",
                &entry("2026-10-v03", "finalize"),
            ),
        ]);
        let mut backends = Vec::new();
        for (id, fingerprint) in [("deployment", "fp-target"), ("redis", "fp-redis")] {
            let mut backend = blank_row();
            backend.kind = MigrationRecordKind::Backend;
            backend.id = id.to_owned();
            backend.fingerprint = Some(fingerprint.to_owned());
            backends.push(backend);
        }
        let existing = Existing {
            history: target.migrations.clone(),
            revision: Some("2026-10-v03".to_owned()),
            rows: vec![
                (MigrationRecordKind::Backend, "deployment".to_owned()),
                (MigrationRecordKind::Backend, "redis".to_owned()),
                (MigrationRecordKind::Run, "pass".to_owned()),
            ],
            issue_keys: BTreeSet::new(),
        };
        let origin = Origin {
            baseline: true,
            ..origin()
        };
        let baseline = baseline_record(&rev("2026-10-v02"), &backends, origin.at);
        assert_eq!(baseline.revision(), Some("2026-10-v02"));
        let plan = plan_record(&existing, &baseline, &origin);
        // the revision row is last and names the baseline
        let last = plan.migrations.last().unwrap();
        assert_eq!(last.kind, MigrationRecordKind::Revision);
        assert_eq!(last.revision.as_deref(), Some("2026-10-v02"));
        // the backend rows are written back as the target had them and none is deleted
        for backend in &backends {
            assert!(plan.migrations.contains(backend), "{:?}", backend.id);
        }
        assert!(
            plan.delete_rows
                .iter()
                .all(|(kind, _)| *kind != MigrationRecordKind::Backend),
            "{:?}",
            plan.delete_rows
        );
        // the target's history is archived and replaced, and its pass is void
        assert_eq!(plan.archive.len(), 2);
        assert!(
            plan.delete_rows
                .contains(&(MigrationRecordKind::Run, "pass".to_owned()))
        );
        // the history is one Restored entry at the baseline that says why
        let history: Vec<_> = plan
            .migrations
            .iter()
            .filter(|row| row.kind == MigrationRecordKind::History)
            .collect();
        assert_eq!(history.len(), 1, "{history:?}");
        let restored = history[0].entry.clone().unwrap().0;
        assert_eq!(restored.revision, "2026-10-v02");
        let message = restored.message.unwrap();
        for expected in [
            "/backups/one",
            "no schema record",
            "2026-10-v02",
            "2026-10-v03",
        ] {
            assert!(message.contains(expected), "{expected} missing: {message}");
        }
        assert_eq!(plan.replaced_revision.as_deref(), Some("2026-10-v03"));
    }

    /// After the baseline is recorded under a target at the third revision, the operator plans
    /// that revision's schema apply and its users conversion
    #[test]
    fn the_operator_plans_every_step_after_the_baseline() {
        let production = Catalog::PRODUCTION;
        assert_eq!(
            planned_after(&rev("2026-10-v02"), &rev("2026-10-v03"), &production),
            [
                "2026-10-v03 schema-apply",
                "2026-10-v03 users-settings-theme"
            ]
        );
        // under the latest revision every step of every edge after the baseline is planned,
        // whatever order the planner runs them in
        let latest = production.latest();
        let state = production.reached(&rev("2026-10-v02"), "test");
        let mut expected: Vec<String> = production
            .path(&rev("2026-10-v02"), &latest)
            .unwrap()
            .into_iter()
            .flat_map(|edge| {
                production
                    .edge_steps(&state, edge)
                    .into_iter()
                    .map(move |step| format!("{} {}", edge.id, step.id))
            })
            .collect();
        let mut planned = planned_after(&rev("2026-10-v02"), &latest, &production);
        planned.sort();
        expected.sort();
        assert_eq!(planned, expected);
        // nothing is planned when the restore didn't lower the revision
        assert_eq!(
            planned_after(&latest, &latest, &production),
            Vec::<String>::new()
        );
    }
}
