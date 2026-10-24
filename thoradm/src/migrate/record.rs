//! `thoradm migrate record`: copies the operator's entries into the record and moves its revision
//!
//! The operator runs it as the one Job that mounts the state `ConfigMap`, read-only and without a
//! service account token. `--revision` copies the entries the operator recorded since the last
//! copy, except a `Done` entry of a gated step, which only that step's own run writes, then moves
//! the revision once every step of the path is recorded. `--seed` is the one write that copies the
//! state's revision and whole history, for a deployment that predates the record, and is refused
//! once a revision is recorded. The Job's full result, the record as it stands, goes to its result
//! row, and the termination message stays a short summary.

use serde_json::{Value, json};
use std::path::Path;
use thorium::Conf;
use thorium::models::schema::{
    self, RecordError, RecordStatus, copy_operator_entries, record_revision, seed_from_state,
};
use thorium::models::upgrades::{Catalog, RevisionId, STATE_KEY, UpgradeState};
use tracing::info;

use crate::Error;
use crate::args::{Args, RecordRevision};
use crate::outcome::RunOutcome;
use crate::shared::termination::{job_uid, write_summary};

/// How long to wait for Scylla to accept connections, in seconds
const WAIT_FOR_SCYLLA_SECS: u64 = 60;

/// The most characters of an error message a failure summary carries
const ERROR_CHARS: usize = 512;

/// Convert a record error into thoradm's error, whose exit code an operator Job acts on
///
/// # Arguments
///
/// * `err` - The record error
fn record_error(err: RecordError) -> Error {
    match err {
        // a failed statement may succeed when the Job retries
        RecordError::Query { .. } => Error::Transient(err.to_string()),
        // everything else is a refusal a retry would repeat
        other => Error::Generic(other.to_string()),
    }
}

/// Read the state the operator mounted, from the file or from the directory holding it
///
/// # Arguments
///
/// * `path` - The mounted `ConfigMap`'s directory, or the state's file
fn read_state(path: &Path) -> Result<UpgradeState, Error> {
    // a mounted ConfigMap is a directory with one file per key
    let file = if path.is_dir() {
        path.join(STATE_KEY)
    } else {
        path.to_path_buf()
    };
    let raw = std::fs::read_to_string(&file).map_err(|err| {
        Error::Usage(format!(
            "the state at {} can't be read: {err}",
            file.display()
        ))
    })?;
    let state = serde_json::from_str::<UpgradeState>(&raw).map_err(|err| {
        Error::Usage(format!(
            "the state at {} isn't valid: {err}",
            file.display()
        ))
    })?;
    // a state written by a newer build may hold fields this one would drop
    state
        .check_schema()
        .map_err(|err| Error::Usage(err.to_string()))?;
    Ok(state)
}

/// Record a revision, or seed the record, and return what the record holds afterwards
///
/// # Arguments
///
/// * `cmd` - What to record
/// * `conf` - The cluster's config
async fn run(cmd: &RecordRevision, conf: &Conf) -> Result<(RecordStatus, Value), Error> {
    // read the arguments before touching Scylla
    let revision = cmd
        .revision
        .parse::<RevisionId>()
        .map_err(|err| Error::Usage(format!("--revision {}: {err}", cmd.revision)))?;
    let state = cmd.state.as_deref().map(read_state).transpose()?;
    if cmd.seed && state.is_none() {
        return Err(Error::Usage("--seed needs --state".to_owned()));
    }
    let session = crate::shared::scylla::connect_waiting(conf, Some(WAIT_FOR_SCYLLA_SECS)).await?;
    let version = env!("CARGO_PKG_VERSION");
    let mut detail = json!({"revision": revision});
    // a seed copies the state's revision and history; the revision named must be the state's own
    if let (true, Some(state)) = (cmd.seed, &state) {
        if state.revision != revision {
            return Err(Error::Usage(format!(
                "--revision {revision} isn't the state's revision {}",
                state.revision
            )));
        }
        let copied = seed_from_state(&session, conf, &revision, version, &state.applied)
            .await
            .map_err(record_error)?;
        info!("Seeded revision {revision} with {copied} history entries");
        detail["seeded"] = json!(copied);
    } else {
        // otherwise copy what the operator recorded since the last copy, then move the revision
        if let Some(state) = &state {
            let copied =
                copy_operator_entries(&session, conf, &Catalog::PRODUCTION, &state.applied)
                    .await
                    .map_err(record_error)?;
            info!("Copied {copied} history entries");
            detail["copied"] = json!(copied);
        }
        record_revision(&session, conf, &Catalog::PRODUCTION, &revision, version)
            .await
            .map_err(record_error)?;
        info!("Recorded revision {revision}");
    }
    // the record as it stands is the Job's result
    let status = schema::read_status(&session, conf, &Catalog::PRODUCTION)
        .await
        .map_err(record_error)?;
    if let Some(uid) = job_uid() {
        let value = serde_json::to_value(&status)?;
        schema::write_job_result(&session, conf, &uid, &value)
            .await
            .map_err(record_error)?;
    }
    Ok((status, detail))
}

/// Build the termination summary of a run
///
/// # Arguments
///
/// * `result` - How the run ended
fn summary(result: &Result<(RecordStatus, Value), Error>) -> String {
    match result {
        // a success names the revision and the row its full result is in
        Ok((_, detail)) => {
            let mut summary = json!({"outcome": "done", "result": job_uid()});
            if let Some(fields) = detail.as_object() {
                summary
                    .as_object_mut()
                    .into_iter()
                    .for_each(|map| map.extend(fields.clone()));
            }
            summary.to_string()
        }
        // a failure names its exit code and the start of its message
        Err(err) => json!({
            "outcome": "failed_systematic",
            "exit_code": err.exit_code(),
            "retryable": matches!(err, Error::Transient(_)),
            "error": err.to_string().chars().take(ERROR_CHARS).collect::<String>(),
        })
        .to_string(),
    }
}

/// Record the revision a plan reached, or seed the record from the state
///
/// # Arguments
///
/// * `cmd` - The revision to record
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &RecordRevision, args: &Args) -> Result<RunOutcome, Error> {
    // a config that can't be loaded is the caller's to fix
    let conf = crate::schema::load_conf(args)?;
    // run, then leave the Job's summary whatever the outcome was
    let result = run(cmd, &conf).await;
    write_summary(&summary(&result));
    let (_, detail) = result?;
    println!("{detail}");
    Ok(RunOutcome::Complete)
}
