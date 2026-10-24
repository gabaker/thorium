//! `thoradm migrate reset-state`: rewrites the recorded revision after a data restore
//!
//! The command is the admin's recovery when the record names a revision the data isn't at, for a
//! restore or wipe that didn't go through `thoradm restore`. It takes the namespace's writer lease,
//! so it refuses while a Job or another command is writing, voids the migration work and pause
//! counts of revisions above the target, appends one `reset-state` history entry, and writes the
//! revision row last, so a stopped run leaves the old revision and can run again. The entry lowers
//! the revision, so every step above the target counts as unfinished again although its old `Done`
//! entry stays in the history. The operator's `ConfigMap` mirror follows the record at the
//! operator's next drift read of it, due hourly at a reconcile (at least daily), which adopts the lowered revision and
//! history.

use chrono::{DateTime, SecondsFormat, Utc};
use futures::StreamExt;
use scylla::client::session::Session;
use std::collections::BTreeMap;
use thorium::models::schema::scylla::{SchemaMigrationRow, SchemaMigrationsTable};
use thorium::models::schema::{
    JsonText, MigrationRecordKind, MigrationRunStatus, Projection, REVISION_ROW, RecordError,
    ScyllaObject, TableStatements, history_id, with_keyspace,
};
use thorium::models::upgrades::{AppliedStep, Catalog, RESET_STATE_STEP, RevisionId, StepState};
use tracing::info;

use super::context::{self, Env, migrate_error};
use crate::Error;
use crate::args::{Args, ResetState};
use crate::outcome::RunOutcome;

/// What a reset changed, for the line the command prints
#[derive(Debug)]
struct Voided {
    /// The keys of the open passes whose rows were deleted
    passes: Vec<String>,
    /// How many history entries had their recorded pause counts cleared
    paused: usize,
}

/// Build a `schema_migrations` row with only its key and its write time set
///
/// # Arguments
///
/// * `kind` - The row's kind
/// * `id` - The row's id
fn blank(kind: MigrationRecordKind, id: &str) -> SchemaMigrationsTable {
    // every other cell stays unset
    SchemaMigrationsTable {
        kind,
        id: id.to_owned(),
        fingerprint: None,
        apply_key: None,
        layouts: BTreeMap::new(),
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

/// Convert a failed statement into the error of a command that is not retried
///
/// # Arguments
///
/// * `what` - What the statement did
/// * `err` - The failure
fn query_error(what: &str, err: &impl std::fmt::Display) -> Error {
    // an admin runs the command by hand and decides whether to run it again
    Error::Generic(format!("failed to {what}: {err}"))
}

/// Read every row of one kind of `schema_migrations`, oldest id first
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace holding the table
/// * `kind` - The kind to read
async fn read_kind(
    session: &Session,
    keyspace: &str,
    kind: MigrationRecordKind,
) -> Result<Vec<SchemaMigrationRow>, Error> {
    // read the kind's partition through the projection every reader of the record uses
    let columns = <SchemaMigrationRow as Projection>::COLUMNS.join(", ");
    let table = <SchemaMigrationsTable as ScyllaObject>::NAME;
    let mut stream = session
        .query_iter(
            format!("SELECT {columns} FROM {keyspace}.{table} WHERE kind = ?"),
            (kind.as_str(),),
        )
        .await
        .map_err(|err| query_error("read the record", &err))?
        .rows_stream::<SchemaMigrationRow>()
        .map_err(|err| query_error("read the record", &err))?;
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await {
        rows.push(row.map_err(|err| query_error("read a row of the record", &err))?);
    }
    Ok(rows)
}

/// Write one row of `schema_migrations`
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `keyspace` - The keyspace holding the table
/// * `row` - The row to write
async fn write_row(
    session: &Session,
    keyspace: &str,
    row: &SchemaMigrationsTable,
) -> Result<(), Error> {
    // the table's generated insert binds every column by name
    let insert = session
        .prepare(with_keyspace(SchemaMigrationsTable::INSERT, keyspace))
        .await
        .map_err(|err| query_error("prepare a record write", &err))?;
    session
        .execute_unpaged(&insert, row)
        .await
        .map_err(|err| query_error("write the record", &err))?;
    Ok(())
}

/// Read the recorded revision, refusing a namespace that records none
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace holding the table
async fn recorded(session: &Session, keyspace: &str) -> Result<RevisionId, Error> {
    // the revision row holds the revision in its own cell
    let row = read_kind(session, keyspace, MigrationRecordKind::Revision)
        .await?
        .into_iter()
        .find(|row| row.id == REVISION_ROW)
        .and_then(|row| row.revision);
    // without one there is nothing to rewrite
    let Some(revision) = row else {
        return Err(Error::Generic(RecordError::Unrecorded.to_string()));
    };
    revision
        .parse::<RevisionId>()
        .map_err(|err| Error::Generic(format!("the recorded revision {revision}: {err}")))
}

/// Delete the open passes above a target: their worker and issue rows, then their pass rows
///
/// A pass is open while it is running, or stopped and resumable after a failure. A pass that
/// names no revision is voided too, since it can't be shown to sit at or below the target.
///
/// # Arguments
///
/// * `env` - The backends and the pass rows' statements
/// * `target` - The revision the data is at
async fn void_passes(env: &Env, target: &RevisionId) -> Result<Vec<String>, Error> {
    let keyspace = env.store.keyspace();
    let session = env.store.session();
    let mut voided = Vec::new();
    // find the open passes above the target
    for row in read_kind(session, keyspace, MigrationRecordKind::Run).await? {
        let open = matches!(
            row.status,
            Some(MigrationRunStatus::Running | MigrationRunStatus::Failed)
        );
        let above = row
            .revision
            .as_deref()
            .and_then(|revision| revision.parse::<RevisionId>().ok())
            .is_none_or(|revision| revision > *target);
        if !(open && above) {
            continue;
        }
        // the worker and issue rows go first, so no row outlives the pass that owns it
        env.store
            .delete_rows(&row.id)
            .await
            .map_err(migrate_error)?;
        let table = <SchemaMigrationsTable as ScyllaObject>::NAME;
        session
            .query_unpaged(
                format!("DELETE FROM {keyspace}.{table} WHERE kind = ? AND id = ?"),
                (MigrationRecordKind::Run.as_str(), &row.id),
            )
            .await
            .map_err(|err| query_error("delete a pass row", &err))?;
        voided.push(row.id);
    }
    Ok(voided)
}

/// Clear the pause counts recorded on history entries above a target
///
/// The entries stay where they are, under the ids they were written with, so the audit trail keeps
/// its order; only the counts, which a later Job would otherwise scale Deployments back to, go.
///
/// # Arguments
///
/// * `session` - The Scylla session to write with
/// * `keyspace` - The keyspace holding the table
/// * `target` - The revision the data is at
async fn void_pauses(
    session: &Session,
    keyspace: &str,
    target: &RevisionId,
) -> Result<usize, Error> {
    let mut cleared = 0;
    // rewrite each entry that holds counts for a revision above the target
    for row in read_kind(session, keyspace, MigrationRecordKind::History).await? {
        let Some(JsonText(mut entry)) = row.entry.clone() else {
            continue;
        };
        let above = entry
            .revision
            .parse::<RevisionId>()
            .is_ok_and(|revision| revision > *target);
        if entry.paused.is_empty() || !above {
            continue;
        }
        entry.paused.clear();
        let mut rewritten = blank(MigrationRecordKind::History, &row.id);
        rewritten.entry = Some(JsonText(entry));
        write_row(session, keyspace, &rewritten).await?;
        cleared += 1;
    }
    Ok(cleared)
}

/// Build the audit entry of a reset
///
/// # Arguments
///
/// * `from` - The revision that was recorded
/// * `to` - The revision the data is at
/// * `at` - When the reset ran
fn audit_entry(from: &RevisionId, to: &RevisionId, at: DateTime<Utc>) -> AppliedStep {
    // the OS user is the one the lease holder names for a manual run
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown-user".to_owned());
    // the entry belongs to the target's revision, which is what lowers the revision
    AppliedStep {
        revision: to.to_string(),
        step: RESET_STATE_STEP.to_owned(),
        outcome: StepState::Done,
        message: Some(format!("from {from} to {to}")),
        at: Some(at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        by: Some(format!(
            "thoradm {} run by {user}",
            env!("CARGO_PKG_VERSION")
        )),
        ..AppliedStep::default()
    }
}

/// Reset the record while the writer lease is held
///
/// # Arguments
///
/// * `env` - The backends and the pass rows' statements
/// * `target` - The revision the data is at
async fn reset(env: &Env, target: &RevisionId) -> Result<(RevisionId, Voided), Error> {
    let keyspace = env.store.keyspace();
    let session = env.store.session();
    // the record has to name a revision to rewrite
    let from = recorded(session, keyspace).await?;
    // void what was taken over the old data, before the record says the data changed
    let passes = void_passes(env, target).await?;
    let paused = void_pauses(session, keyspace, target).await?;
    // the audit entry first, so a stopped run leaves the old revision and the entry says why
    let at = Utc::now();
    let entry = audit_entry(&from, target, at);
    let id = history_id(&entry).map_err(|err| Error::Generic(err.to_string()))?;
    let mut row = blank(MigrationRecordKind::History, &id);
    row.entry = Some(JsonText(entry));
    write_row(session, keyspace, &row).await?;
    // the revision row goes last
    let mut revision = blank(MigrationRecordKind::Revision, REVISION_ROW);
    revision.revision = Some(target.to_string());
    revision.version = Some(env!("CARGO_PKG_VERSION").to_owned());
    write_row(session, keyspace, &revision).await?;
    Ok((from, Voided { passes, paused }))
}

/// Reset the recorded revision after a restore
///
/// # Arguments
///
/// * `cmd` - The revision the restored data is at
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &ResetState, args: &Args) -> Result<RunOutcome, Error> {
    // the command rewrites what Jobs trust, so it asks for the admin's word first
    if !cmd.i_restored_data {
        return Err(Error::Usage(
            "reset-state rewrites the recorded revision; pass --i-restored-data to confirm the \
             data was restored or wiped and the record is wrong"
                .to_owned(),
        ));
    }
    // the target has to be a revision this build's catalog knows
    let target = super::revision(&cmd.to).map_err(|err| Error::Usage(err.to_string()))?;
    if !Catalog::PRODUCTION.knows(&target) {
        return Err(Error::Usage(format!(
            "--to {target} is not a revision of this thoradm's catalog; see `thoradm migrate list`"
        )));
    }
    // connect, then take the lease without breaking another writer's
    let env = context::connect(args).await?;
    let lease = context::take_lease(&env, false, false).await?;
    let result = reset(&env, &target).await;
    lease.release().await;
    let (from, voided) = result?;
    // say what was written and what was voided
    info!("Reset the recorded revision from {from} to {target}");
    println!(
        "reset the recorded revision from {from} to {target}; voided {} open passes{} and the \
         pause counts of {} history entries",
        voided.passes.len(),
        if voided.passes.is_empty() {
            String::new()
        } else {
            format!(" ({})", voided.passes.join(", "))
        },
        voided.paused
    );
    Ok(RunOutcome::Complete)
}
