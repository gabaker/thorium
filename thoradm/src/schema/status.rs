//! `thoradm schema status`: reads the deployment's record for the operator
//!
//! The operator reaches Scylla only through thoradm Jobs, and this is the read-only one. It prints
//! the JSON of a [`StatusReport`] on `THORIUM-RESULT ` lines, which the operator picks out of the
//! pod's log and joins: the record's state, thoradm's catalog identity, and, with `--result <uid>`,
//! the full result a writing Job left under its UID. A report of at most 64 KiB is one line; a
//! larger one is printed in chunks of at most 64 KiB, each line carrying its index and the chunk
//! count. A Scylla that can't be reached, or that refuses
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
    Acknowledgement, CheckReport, RecordStatus, StatusReport, catalog_identity, converged_revision,
    diff_live, read_job_result, read_status, result_lines,
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
    /// The applied report a migration run Job left, when the row held one
    applied: Option<Value>,
}

/// Read a Job's result row as a record status, a check report, or a run's applied report,
/// whichever it holds
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
            applied: None,
        };
    }
    // a run's applied report counts what it skipped, which neither of the others does
    if value.get("skipped_total").is_some() && value.get("groups").is_some() {
        return JobResult {
            record: None,
            check: None,
            applied: Some(value),
        };
    }
    JobResult {
        record: serde_json::from_value::<RecordStatus>(value)
            .map_err(|err| warn!("The result row {uid} isn't a record status: {err}"))
            .ok(),
        check: None,
        applied: None,
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

/// Build the result lines a status report is printed on: one line, or chunks of at most 64 KiB
/// each carrying its index and the chunk count
///
/// # Arguments
///
/// * `report` - The report to print
fn report_lines(report: &StatusReport) -> Result<Vec<String>, Error> {
    // the report's JSON, which may hold a long worker list
    let json = serde_json::to_string(report)?;
    Ok(result_lines(&json))
}

/// Print the deployment's record, thoradm's catalog identity, and the result a Job left, on
/// `THORIUM-RESULT` lines
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
    let (
        status,
        JobResult {
            record,
            check,
            applied,
        },
    ) = read(
        &conf,
        cmd.wait_for_scylla,
        cmd.result.as_deref(),
        acks.as_deref(),
    )
    .await?;
    // print the report on the result lines the operator picks out of the log and joins
    let state = state_name(&status);
    let report = StatusReport {
        status,
        catalog: catalog_identity(&Catalog::PRODUCTION),
        job_result: record,
        check,
        applied,
    };
    for line in report_lines(&report)? {
        println!("{line}");
    }
    // the termination message is only the outcome, well under Kubernetes' limit
    let summary: Value = json!({"outcome": "done", "state": state});
    write_summary(&summary.to_string());
    Ok(RunOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use thorium::models::schema::{
        AppliedReport, DrainCancelReport, DrainWorker, RESULT_CHUNK_BYTES, RESULT_PREFIX,
        RecordStatus, StatusReport, catalog_identity, join_result,
    };
    use thorium::models::upgrades::Catalog;

    use super::{parse_result, report_lines};

    /// Build a status report carrying an applied report with a long worker list
    ///
    /// # Arguments
    ///
    /// * `workers` - How many workers the list holds
    fn report_with(workers: usize) -> StatusReport {
        // each worker's name holds a character wider than a byte, so chunks must cut between
        // characters
        let listed = (0..workers)
            .map(|at| {
                json!({"group": "corn", "name": format!("worker-é-{at:05}"), "scaler": "K8s",
                       "cluster": "thorium", "node": "node-1"})
            })
            .collect::<Vec<_>>();
        StatusReport {
            status: RecordStatus::NoKeyspace,
            catalog: catalog_identity(&Catalog::PRODUCTION),
            job_result: None,
            check: None,
            applied: Some(json!({"groups": {}, "skipped_total": 0, "drain": {"workers": listed}})),
        }
    }

    /// A report larger than a chunk is printed on several result lines, each at most a chunk of
    /// the report after its prefix and its index and count, which join back into the report
    #[test]
    fn a_large_report_is_printed_in_chunks_that_join_back() {
        // a report several chunks long
        let report = report_with(5_000);
        let json = serde_json::to_string(&report).expect("the report serializes");
        assert!(json.len() > 2 * RESULT_CHUNK_BYTES, "{}", json.len());
        let lines = report_lines(&report).expect("lines");
        assert!(lines.len() > 2, "{}", lines.len());
        // each line is a numbered chunk of at most the chunk size
        let count = lines.len();
        let mut stripped = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let rest = line.strip_prefix(RESULT_PREFIX).expect("the prefix");
            let head = format!("{}/{count} ", index + 1);
            let chunk = rest.strip_prefix(&head).expect("the index and count");
            assert!(chunk.len() <= RESULT_CHUNK_BYTES, "{}", chunk.len());
            assert!(!line.contains('\n'));
            stripped.push(rest);
        }
        // the chunks join back into the same report
        let joined = join_result(&stripped).expect("the chunks join");
        let parsed: StatusReport = serde_json::from_str(&joined).expect("a report");
        assert_eq!(parsed, report);
    }

    /// A report that fits in a chunk is printed as one plain line, as before chunking
    #[test]
    fn a_small_report_is_one_plain_line() {
        // a report with a short worker list
        let report = report_with(2);
        let lines = report_lines(&report).expect("lines");
        assert_eq!(lines.len(), 1);
        let json = serde_json::to_string(&report).expect("the report serializes");
        assert_eq!(lines[0], format!("{RESULT_PREFIX}{json}"));
    }

    /// A result row holding a run's applied report is read as one, not as a record status
    #[test]
    fn an_applied_report_row_is_read_as_one() {
        // the row a run Job leaves
        let row: Value = json!({"groups": {}, "skipped_total": 0, "skipped": [], "warnings_total": 0,
                                "notes": [], "mappings_total": 0, "drain": {"workers": []}});
        let read = parse_result("uid-1", row.clone());
        assert_eq!(read.applied, Some(row));
        assert!(read.record.is_none() && read.check.is_none());
    }

    /// The bounded report a cancel run leaves is read as an applied report whose drain part reads
    /// back whole, even when the run changed no group and skipped nothing
    #[test]
    fn a_cancel_runs_report_is_read_as_an_applied_report() {
        // the report as the cancel leaves it: no groups, nothing skipped, the counts and workers
        let worker = DrainWorker {
            group: "corn".to_owned(),
            name: "corn-unpack-7f9c2".to_owned(),
            scaler: "K8s".to_owned(),
            cluster: "thorium".to_owned(),
            node: "node-1".to_owned(),
        };
        let drain = DrainCancelReport {
            cancelled: 4,
            recorded: 2,
            refused: 1,
            stale: 1,
            kept: 0,
            workers_removed: 1,
            workers_not_listed: 0,
            workers: vec![worker],
        };
        let mut report = AppliedReport {
            drain: Some(drain.clone()),
            ..AppliedReport::default()
        };
        report.bound();
        let row = serde_json::to_value(&report).expect("the report renders");
        // the status Job reads it as an applied report
        let read = parse_result("uid-1", row);
        let applied = read.applied.expect("an applied report");
        assert!(read.record.is_none() && read.check.is_none());
        // its drain part reads back as the run wrote it
        let back: DrainCancelReport =
            serde_json::from_value(applied["drain"].clone()).expect("a drain report");
        assert_eq!(back, drain);
    }
}
