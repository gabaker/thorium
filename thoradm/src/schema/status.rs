//! `thoradm schema status`: reads the deployment's record for the operator
//!
//! The operator reaches Scylla only through thoradm Jobs, and this is the read-only one. It prints
//! one line, `THORIUM-RESULT ` and the JSON of a [`StatusReport`], which the operator picks out of
//! the pod's log: the record's state, thoradm's catalog identity, and, with `--result <uid>`, the
//! full result a writing Job left under its UID. A Scylla that can't be reached, or that refuses
//! Thorium's login, is a state of the report, never a failure of the Job, so the operator retries
//! and never takes the namespace for a fresh install. The termination message stays a short
//! summary of the outcome.
//!
//! Unless `--no-probe` is given, a recorded deployment's report also carries the drift summary: the
//! probe's findings against the recorded revision's snapshot, with the registry's and
//! `--acknowledged`'s acknowledgements applied before the objects are capped. A probe that fails
//! fails the Job as a transient error, so the operator never mistakes a backend it couldn't read
//! for one without drift.

use serde_json::{Value, json};
use thorium::Conf;
use thorium::models::schema::{
    Acknowledgement, CheckReport, RESULT_PREFIX, RecordStatus, StatusReport, catalog_identity,
    converged_revision, diff_live, read_job_result, read_status,
};
use thorium::models::upgrades::Catalog;
use tracing::warn;

use crate::Error;
use crate::args::{Args, StatusSchema};
use crate::outcome::RunOutcome;
use crate::shared::scylla::{get_client, is_auth_error};
use crate::shared::termination::write_summary;

/// The most characters of an unreachable message the report carries
const MESSAGE_CHARS: usize = 300;

/// How long to wait between attempts to connect while waiting for Scylla, in seconds
const RETRY_SECS: u64 = 3;

/// Cut a message to the length the report carries
///
/// # Arguments
///
/// * `message` - The message to cut
fn shorten(message: &str) -> String {
    // keep the start, which names what failed
    message.chars().take(MESSAGE_CHARS).collect()
}

/// What a Job's result row held
#[derive(Debug, Default)]
struct JobResult {
    /// The record a writing Job left, when the row held one
    record: Option<RecordStatus>,
    /// The report a check Job left, when the row held one
    check: Option<CheckReport>,
}

/// Read a Job's result row as a record status or a check report, whichever it holds
///
/// # Arguments
///
/// * `uid` - The Job's UID, for the warning when the row is neither
/// * `value` - The row's result
fn parse_result(uid: &str, value: Value) -> JobResult {
    // a check report names its migration and counts; a record status is tagged by its state
    if value.get("migration").is_some() && value.get("counts").is_some() {
        return JobResult {
            record: None,
            check: serde_json::from_value::<CheckReport>(value)
                .map_err(|err| warn!("The result row {uid} isn't a check report: {err}"))
                .ok(),
        };
    }
    JobResult {
        record: serde_json::from_value::<RecordStatus>(value)
            .map_err(|err| warn!("The result row {uid} isn't a record status: {err}"))
            .ok(),
        check: None,
    }
}

/// Read the record, turning a Scylla that can't be reached or logged in to into a state
///
/// # Arguments
///
/// * `conf` - The cluster's config
/// * `wait` - How many seconds to keep retrying the connection, if any
/// * `result` - The UID of a Job whose result row to read too
/// * `probe` - The acknowledgements to probe the live schema with, or `None` to skip the probe
async fn read(
    conf: &Conf,
    wait: Option<u64>,
    result: Option<&str>,
    probe: Option<&[Acknowledgement]>,
) -> Result<(RecordStatus, JobResult), Error> {
    // connect, retrying while the wait lasts
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(wait.unwrap_or_default());
    let session = loop {
        match get_client(conf).await {
            Ok(session) => break session,
            // a refused login won't be fixed by waiting
            Err(Error::ScyllaNewSession(err)) if is_auth_error(&err) => {
                return Ok((RecordStatus::AuthFailed, JobResult::default()));
            }
            Err(err) if std::time::Instant::now() < deadline => {
                warn!("Scylla isn't accepting connections yet: {err}");
                tokio::time::sleep(std::time::Duration::from_secs(RETRY_SECS)).await;
            }
            Err(Error::ScyllaNewSession(err)) => {
                let message = shorten(&format!("Scylla can't be reached: {err}"));
                return Ok((RecordStatus::Unreachable { message }, JobResult::default()));
            }
            Err(err) => return Err(err),
        }
    };
    // read the record; a statement that fails reads as Scylla not being reachable
    let status = match read_status(&session, conf, &Catalog::PRODUCTION).await {
        Ok(status) => status,
        Err(thorium::models::schema::RecordError::Query { what, error }) => {
            let message = shorten(&format!("failed to {what}: {error}"));
            return Ok((RecordStatus::Unreachable { message }, JobResult::default()));
        }
        Err(err) => return Err(Error::Generic(err.to_string())),
    };
    // a recorded deployment gets the drift summary, unless the probe is skipped
    let status = match (status, probe) {
        (RecordStatus::Recorded { record, .. }, Some(acks)) => {
            // an apply that ran ahead of the record converged to the next revision's schema
            let revision = converged_revision(
                &Catalog::PRODUCTION,
                &record.revision,
                &record.schema.deployment,
            );
            let report = diff_live(&session, conf, &revision, acks)
                .await
                .map_err(|err| super::diff::probe_error(&err))?;
            let drift = Some(report.summary(acks));
            RecordStatus::Recorded { record, drift }
        }
        (status, _) => status,
    };
    // then the result row a Job left, when one was asked for
    let job_result = match result {
        Some(uid) => match read_job_result(&session, conf, uid).await {
            Ok(Some(value)) => parse_result(uid, value),
            Ok(None) => JobResult::default(),
            Err(err) => return Err(Error::Transient(err.to_string())),
        },
        None => JobResult::default(),
    };
    Ok((status, job_result))
}

/// Name a status for the termination summary
///
/// # Arguments
///
/// * `status` - The record's status
fn state_name(status: &RecordStatus) -> &'static str {
    // the tag the report serializes the state under
    match status {
        RecordStatus::Unreachable { .. } => "unreachable",
        RecordStatus::AuthFailed => "auth_failed",
        RecordStatus::NoKeyspace => "no_keyspace",
        RecordStatus::NoRecord { .. } => "no_record",
        RecordStatus::Recorded { .. } => "recorded",
    }
}

/// Print the deployment's record, thoradm's catalog identity, and the result a Job left, on one
/// `THORIUM-RESULT` line
///
/// # Arguments
///
/// * `cmd` - What to read and report
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &StatusSchema, args: &Args) -> Result<RunOutcome, Error> {
    // a config that can't be loaded is the caller's to fix
    let conf = super::load_conf(args)?;
    // the acknowledgements to probe with, unless the probe is skipped
    let acks = match (&cmd.acknowledged, cmd.no_probe) {
        (_, true) => None,
        (Some(json), false) => Some(super::diff::parse_acknowledgements(json, "--acknowledged")?),
        (None, false) => Some(Vec::new()),
    };
    // read the record, and the result row when asked
    let (status, JobResult { record, check }) = read(
        &conf,
        cmd.wait_for_scylla,
        cmd.result.as_deref(),
        acks.as_deref(),
    )
    .await?;
    // print the whole report on one line the operator picks out of the log
    let state = state_name(&status);
    let report = StatusReport {
        status,
        catalog: catalog_identity(&Catalog::PRODUCTION),
        job_result: record,
        check,
    };
    println!("{RESULT_PREFIX}{}", serde_json::to_string(&report)?);
    // the termination message is only the outcome, well under Kubernetes' limit
    let summary: Value = json!({"outcome": "done", "state": state});
    write_summary(&summary.to_string());
    Ok(RunOutcome::Complete)
}
