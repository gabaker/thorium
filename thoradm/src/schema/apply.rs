//! `thoradm schema apply`: converges the live schema to the declared one
//!
//! The operator runs it as its schema-apply Job, `thoradm --cluster-conf <file> schema apply
//! --wait-for-scylla <secs>`, passing the trigger key in [`TRIGGER_KEY_ENV`]. Whatever the outcome,
//! the command writes a short JSON summary where Kubernetes reads the Job's termination message;
//! on success it names the deployment fingerprint the operator records, and on a refusal it names
//! the refused settings (`refused`) and the data types whose sampled rows didn't match their
//! partition size (`sampled`). The exit code tells the Job what to do: 0 on success, 1 when the
//! apply refuses, 2 when the config or a flag can't be used, 3 when a flag's behavior isn't
//! available in this build, all of which fail the Job at once, and 4 when Scylla couldn't be
//! reached or a statement failed, which the Job retries.

use serde_json::{Value, json};
use thorium::Conf;
use thorium::models::schema::{
    self, ApplyError, ApplyOptions, ApplyReport, ApplyTarget, ConfigAccept, RecordStatus,
    SeedRequest,
};
use thorium::models::upgrades::{Catalog, RevisionId};
use tracing::{info, warn};

use crate::Error;
use crate::args::{ApplySchema, Args};
use crate::outcome::RunOutcome;
use crate::shared::termination::{TERMINATION_LIMIT, job_uid, write_summary};

/// The environment variable the operator passes the trigger key in, which the apply records with
/// the fingerprint
pub const TRIGGER_KEY_ENV: &str = "THORIUM_SCHEMA_APPLY_KEY";

/// The environment variable the operator passes where a seeded revision came from in, which the
/// seed's history entry names; a run by hand names `--seed-revision`
pub const SEED_SOURCE_ENV: &str = "THORIUM_SEED_SOURCE";

/// The most characters of an error message a failure summary carries
const ERROR_CHARS: usize = 512;

/// What ends a trigger key cut to fit the termination message
const ELLIPSIS: &str = "…";

/// Why a run failed, with what the operator plans from a refusal
#[derive(Debug)]
pub(super) struct Failure {
    /// The error, whose exit code the Job acts on
    error: Error,
    /// The settings refused because they changed while their data exists, as config paths
    refused: Vec<String>,
    /// The data types whose sampled rows sit in buckets their configured size doesn't give
    sampled: Vec<String>,
}

impl From<Error> for Failure {
    /// A failure that refused no setting
    ///
    /// # Arguments
    ///
    /// * `error` - The error
    fn from(error: Error) -> Self {
        Failure {
            error,
            refused: Vec::new(),
            sampled: Vec::new(),
        }
    }
}

impl From<ApplyError> for Failure {
    /// A failure from the applier, keeping the settings it refused
    ///
    /// # Arguments
    ///
    /// * `err` - The applier's error
    fn from(err: ApplyError) -> Self {
        // read the refused settings before the error becomes text
        let refused = err.refused_settings();
        let sampled = err.sampled_data_types();
        Failure {
            error: apply_error(err),
            refused,
            sampled,
        }
    }
}

/// Convert an applier error into thoradm's error, whose exit code an operator Job acts on
///
/// # Arguments
///
/// * `err` - The applier's error
fn apply_error(err: ApplyError) -> Error {
    match err {
        // a failed statement may succeed when the Job retries
        ApplyError::Query { .. } => Error::Transient(err.to_string()),
        // a target this build can't converge to exits like any unavailable subcommand
        ApplyError::Unavailable { .. } => Error::NotImplemented(err.to_string()),
        // every other error is a refusal a retry would repeat
        other => Error::Generic(other.to_string()),
    }
}

/// Cut the trigger key in a summary to the longest start that keeps the summary within the size
/// Kubernetes keeps, ending it with an ellipsis
///
/// # Arguments
///
/// * `summary` - The summary to cut the key in
/// * `key` - The whole trigger key
fn shorten_key(summary: &mut Value, key: &str) {
    // whether the summary fits with the key cut at a byte offset
    let mut fits = |end: usize| {
        summary["trigger_key"] = json!(format!("{}{ELLIPSIS}", &key[..end]));
        summary.to_string().len() <= TERMINATION_LIMIT
    };
    // cut only at char boundaries; a longer start never fits where a shorter one doesn't
    let ends = key.char_indices().map(|(at, _)| at).collect::<Vec<_>>();
    let keep = ends.partition_point(|end| fits(*end)).saturating_sub(1);
    // leave the summary holding the longest start that fits
    fits(ends.get(keep).copied().unwrap_or_default());
}

/// Build the short summary written as the Job's termination message for a run that left no
/// result row
///
/// # Arguments
///
/// * `result` - How the run ended
#[cfg(test)]
fn summary(result: &Result<ApplyReport, Failure>) -> String {
    // a run that left no result row names none
    summary_with(result, None)
}

/// Build the short summary written as the Job's termination message, naming the row the Job's
/// full result was written under
///
/// # Arguments
///
/// * `result` - How the run ended
/// * `row` - The id of the Job's result row, when one was written
fn summary_with(result: &Result<ApplyReport, Failure>, row: Option<&str>) -> String {
    match result {
        // a success names the fingerprint the operator records, with counts instead of lists
        Ok(report) => {
            // the key never carries the files password's hash
            let key = schema::redact_trigger_key(&report.trigger_key);
            let mut summary = json!({
                "outcome": "done",
                "fingerprint": report.fingerprint,
                "trigger_key": key,
                "created": report.created.len(),
                "added": report.added.len(),
                "views_built": report.views_built,
                "warnings": report.warnings,
                "dry_run": report.dry_run,
                "result": row,
            });
            // the lists give way to their lengths when they'd outgrow the message
            if summary.to_string().len() > TERMINATION_LIMIT {
                summary["views_built"] = json!(report.views_built.len());
                summary["warnings"] = json!(report.warnings.len());
            }
            // a key still too long is cut to what's left, so the message stays whole JSON
            if summary.to_string().len() > TERMINATION_LIMIT {
                shorten_key(&mut summary, &key);
            }
            summary.to_string()
        }
        // a failure names its exit code, what it refused, and the start of its message
        Err(failure) => {
            let err = &failure.error;
            let mut message = err
                .to_string()
                .chars()
                .take(ERROR_CHARS)
                .collect::<String>();
            loop {
                let summary = json!({
                    "outcome": "failed_systematic",
                    "exit_code": err.exit_code(),
                    "retryable": matches!(err, Error::Transient(_)),
                    "refused": failure.refused,
                    "sampled": failure.sampled,
                    "result": row,
                    "error": message,
                })
                .to_string();
                // the message gives way first, so the lists the operator plans from stay whole
                if summary.len() <= TERMINATION_LIMIT || message.is_empty() {
                    return summary;
                }
                let keep = message.chars().count() / 2;
                message = message.chars().take(keep).collect();
            }
        }
    }
}

/// Apply the declared schema and return what the run did
///
/// # Arguments
///
/// * `cmd` - How to apply it
/// * `args` - The arguments thoradm was started with
pub(super) async fn run(cmd: &ApplySchema, args: &Args) -> Result<ApplyReport, Failure> {
    // read the revision to seed before touching anything, so a malformed one is a usage error
    let seed = seed_request(cmd)?;
    let accept = ConfigAccept {
        partition_sizes: cmd.accept_partition_sizes,
    };
    let conf = super::load_conf(args)?;
    // connect, waiting for Scylla when asked to
    let session = crate::shared::scylla::connect_waiting(&conf, cmd.wait_for_scylla).await?;
    // a run that changes anything is the namespace's only writer while it runs
    let lease = if cmd.dry_run {
        None
    } else {
        let redis = crate::shared::redis::get_client(&conf)
            .await
            .map_err(|err| Error::Transient(format!("Redis can't be reached: {err}")))?;
        Some(
            crate::migrate::context::lease_in(
                &redis,
                &conf.thorium.namespace,
                cmd.force_break_lease,
                true,
            )
            .await?,
        )
    };
    // converge to the declared schema, recording the key the operator passed, if any
    let key = std::env::var(TRIGGER_KEY_ENV).unwrap_or_default();
    let options = ApplyOptions {
        target: snapshot_target(cmd)?,
        dry_run: cmd.dry_run,
    };
    let progress = |line: &str| info!("{line}");
    let result = schema::apply_seeding(
        &session,
        &conf,
        &options,
        &accept,
        seed.as_ref(),
        &key,
        &progress,
    )
    .await;
    // give the lease up, so the next writer needn't wait for it to expire
    if let Some(lease) = lease {
        lease.release().await;
    }
    let report = result?;
    // log each warning as well as reporting it
    for warning in &report.warnings {
        warn!("{warning}");
    }
    // leave the Job's result, the record as it stands now, where the operator reads it
    if !report.dry_run {
        write_result(&session, &conf).await?;
    }
    Ok(report)
}

/// Get what the apply converges to: the declared schema, or an older revision's snapshot
///
/// # Arguments
///
/// * `cmd` - How to apply the schema
fn snapshot_target(cmd: &ApplySchema) -> Result<ApplyTarget, Error> {
    // no flag, the declared schema
    let Some(raw) = &cmd.to_revision else {
        return Ok(ApplyTarget::Declared);
    };
    // a revision that doesn't parse is the caller's to fix
    let revision = raw
        .parse::<RevisionId>()
        .map_err(|err| Error::Usage(format!("--to-revision {raw}: {err}")))?;
    Ok(ApplyTarget::Snapshot(revision))
}

/// Build the seed `--seed-revision` asks for, with where it came from
///
/// # Arguments
///
/// * `cmd` - How to apply the schema
fn seed_request(cmd: &ApplySchema) -> Result<Option<SeedRequest>, Error> {
    // no flag, no seed
    let Some(raw) = &cmd.seed_revision else {
        return Ok(None);
    };
    // a revision that doesn't parse is the caller's to fix
    let revision = raw
        .parse::<RevisionId>()
        .map_err(|err| Error::Usage(format!("--seed-revision {raw}: {err}")))?;
    // the operator says where the revision came from; a run by hand names the flag
    let source = std::env::var(SEED_SOURCE_ENV)
        .ok()
        .filter(|source| !source.trim().is_empty())
        .unwrap_or_else(|| "--seed-revision".to_owned());
    Ok(Some(SeedRequest {
        revision,
        source,
        catalog: Catalog::PRODUCTION,
    }))
}

/// Write the record as it stands to this Job's result row, when the process runs in a Job
///
/// # Arguments
///
/// * `session` - The Scylla session to read and write with
/// * `conf` - The Thorium config, for the keyspace
async fn write_result(
    session: &scylla::client::session::Session,
    conf: &Conf,
) -> Result<(), Error> {
    // a run by hand has no Job to leave a result for
    let Some(uid) = job_uid() else {
        return Ok(());
    };
    // the record after this run's changes is the result
    let status = schema::read_status(session, conf, &Catalog::PRODUCTION)
        .await
        .map_err(|err| Error::Transient(err.to_string()))?;
    schema::write_job_result(session, conf, &uid, &status_value(&status)?)
        .await
        .map_err(|err| Error::Transient(err.to_string()))
}

/// Serialize a record status for a result row
///
/// # Arguments
///
/// * `status` - The status to serialize
fn status_value(status: &RecordStatus) -> Result<Value, Error> {
    // a status is plain data, so it serializes
    Ok(serde_json::to_value(status)?)
}

/// Apply the declared schema
///
/// # Arguments
///
/// * `cmd` - How to apply it
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &ApplySchema, args: &Args) -> Result<RunOutcome, Error> {
    // apply, recreating any views named, then summarize the outcome for the Job whatever it was
    let result = if cmd.recreate_view.is_empty() {
        run(cmd, args).await
    } else {
        super::recreate_view::run(cmd, args).await
    };
    // name the Job's result row in the summary when this run wrote one
    let row = result
        .as_ref()
        .ok()
        .filter(|report| !report.dry_run)
        .and_then(|_| job_uid());
    write_summary(&summary_with(&result, row.as_deref()));
    let report = result.map_err(|failure| failure.error)?;
    if report.dry_run {
        // a dry run prints the statements a real run would issue
        for statement in &report.statements {
            println!("{statement};");
        }
    } else {
        // a real run logs what it changed and prints its full report
        info!(
            "Applied the schema: {} objects created, {} columns added, {} views built; recorded \
             deployment fingerprint {}",
            report.created.len(),
            report.added.len(),
            report.views_built.len(),
            report.fingerprint.as_str()
        );
        println!("{}", serde_json::to_string(&report)?);
    }
    Ok(RunOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use thorium::models::schema::{ApplyError, ApplyReport, ApplyTarget, Fingerprint};

    use super::{
        ELLIPSIS, Failure, TERMINATION_LIMIT, apply_error, seed_request, snapshot_target, summary,
    };
    use crate::Error;
    use crate::args::ApplySchema;

    /// A report as a run that changed a lot would return
    fn report() -> ApplyReport {
        ApplyReport {
            statements: vec!["CREATE TABLE IF NOT EXISTS t (x int PRIMARY KEY)".to_owned(); 500],
            created: vec!["t".to_owned(); 500],
            added: vec!["t.x".to_owned(); 500],
            config_changes: Vec::new(),
            fingerprint: Fingerprint::of(&json!("fixture")),
            trigger_key: "image=thorium:1;revision=2026-10-v02".to_owned(),
            views_built: vec!["v".to_owned(); 500],
            warnings: vec!["a warning".to_owned(); 500],
            dry_run: false,
        }
    }

    /// A successful run's summary is JSON naming the outcome, the fingerprint, and the trigger key,
    /// within the size Kubernetes keeps however much the run changed
    #[test]
    fn a_success_summary_carries_the_fingerprint() {
        // summarize a run that changed a lot
        let report = report();
        let text = summary(&Ok(report.clone()));
        assert!(text.len() <= TERMINATION_LIMIT, "{} bytes", text.len());
        let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
        assert_eq!(parsed["outcome"], "done");
        assert_eq!(parsed["fingerprint"], report.fingerprint.as_str());
        assert_eq!(parsed["trigger_key"], report.trigger_key.as_str());
        assert_eq!(parsed["created"], 500);
        assert_eq!(parsed["added"], 500);
    }

    /// A trigger key too long for the termination message is cut with an ellipsis, so the summary
    /// stays within the size Kubernetes keeps and is still JSON naming the fingerprint
    #[test]
    fn a_long_trigger_key_keeps_the_summary_bounded() {
        // summarize runs with keys far longer than the message, in one- and multi-byte characters
        for unit in ["k", "é", "\""] {
            let mut report = report();
            report.trigger_key = format!("image={}", unit.repeat(5000));
            let text = summary(&Ok(report.clone()));
            assert!(text.len() <= TERMINATION_LIMIT, "{} bytes", text.len());
            let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
            assert_eq!(parsed["fingerprint"], report.fingerprint.as_str());
            // the key keeps its start and ends with the marker
            let key = parsed["trigger_key"].as_str().expect("the key is a string");
            assert!(key.starts_with("image="), "{key}");
            assert!(key.ends_with(ELLIPSIS), "{key}");
            assert!(
                key.len() > 1000,
                "the key was cut too far: {} bytes",
                key.len()
            );
        }
    }

    /// The summary carries the trigger key without the files password's hash
    #[test]
    fn the_summary_leaves_out_the_password_hash() {
        // summarize a run whose report still holds the hash
        let mut report = report();
        report.trigger_key = "image=thorium:1;password=c0ffee5ecret;revision=r".to_owned();
        let text = summary(&Ok(report));
        assert!(!text.contains("c0ffee5ecret"), "{text}");
        let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
        assert_eq!(parsed["trigger_key"], "image=thorium:1;revision=r");
    }

    /// A failed run's summary names the failure and its exit code, cut to the size Kubernetes keeps
    #[test]
    fn a_failure_summary_is_bounded() {
        // summarize a refusal with a very long message
        let err = Error::Generic("x".repeat(20_000));
        let text = summary(&Err(err.into()));
        assert!(text.len() <= TERMINATION_LIMIT, "{} bytes", text.len());
        let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
        assert_eq!(parsed["outcome"], "failed_systematic");
        assert_eq!(parsed["exit_code"], 1);
        assert_eq!(parsed["retryable"], false);
        assert!(parsed.get("fingerprint").is_none());
    }

    /// Refusals exit 1, unavailable work 3, and a failed statement with the retryable code, so an
    /// operator Job fails at once on the first two and retries the last
    #[test]
    fn applier_errors_map_to_the_job_exit_codes() {
        // a refusal fails the Job at once
        let refused = apply_error(ApplyError::SeedRefused {
            recorded: "2026-10-v02".to_owned(),
        });
        assert_eq!(refused.exit_code(), 1);
        // unavailable work exits like any subcommand this build lacks
        let unavailable = apply_error(ApplyError::Unavailable {
            what: "a snapshot".to_owned(),
        });
        assert_eq!(unavailable.exit_code(), 3);
        // a failed statement may succeed on a retry
        let query = apply_error(ApplyError::Query {
            statement: "CREATE TABLE".to_owned(),
            error: "timed out".to_owned(),
        });
        assert_eq!(query.exit_code(), 4);
        // a config that can't be loaded and a Scylla that can't be reached
        assert_eq!(Error::Usage("bad config".to_owned()).exit_code(), 2);
        assert_eq!(Error::Transient("unreachable".to_owned()).exit_code(), 4);
    }

    /// A seed names its revision and where it came from, no flag means no seed, and a malformed
    /// revision is a usage error that fails the Job at once
    #[test]
    fn a_seed_revision_is_parsed_before_anything_is_read() {
        // no flag, no seed
        let plain = ApplySchema {
            dry_run: false,
            wait_for_scylla: None,
            seed_revision: None,
            to_revision: None,
            accept_partition_sizes: false,
            recreate_view: Vec::new(),
            yes: false,
            force_break_lease: false,
        };
        assert!(seed_request(&plain).expect("no seed").is_none());
        // a revision is parsed and named with a source
        let seeded = ApplySchema {
            seed_revision: Some("2026-10-v01".to_owned()),
            ..plain.clone()
        };
        let seed = seed_request(&seeded)
            .expect("a seed")
            .expect("a seed was asked for");
        assert_eq!(seed.revision.as_str(), "2026-10-v01");
        assert_ne!(seed.source, "");
        // a malformed one exits 2
        let bad = ApplySchema {
            seed_revision: Some("latest".to_owned()),
            ..plain
        };
        assert_eq!(
            seed_request(&bad).expect_err("a usage error").exit_code(),
            2
        );
    }

    /// `--to-revision` converges to that revision's snapshot, no flag converges to the declared
    /// schema, and a malformed revision is a usage error
    #[test]
    fn a_target_revision_selects_the_snapshot_to_converge_to() {
        // no flag, the declared schema
        let declared = ApplySchema {
            dry_run: false,
            wait_for_scylla: None,
            seed_revision: None,
            to_revision: None,
            accept_partition_sizes: false,
            recreate_view: Vec::new(),
            yes: false,
            force_break_lease: false,
        };
        assert!(matches!(
            snapshot_target(&declared),
            Ok(ApplyTarget::Declared)
        ));
        // a revision selects its snapshot
        let older = ApplySchema {
            to_revision: Some("2026-10-v02".to_owned()),
            ..declared.clone()
        };
        match snapshot_target(&older) {
            Ok(ApplyTarget::Snapshot(revision)) => assert_eq!(revision.as_str(), "2026-10-v02"),
            other => panic!("expected a snapshot target: {other:?}"),
        }
        // a malformed one exits 2
        let bad = ApplySchema {
            to_revision: Some("older".to_owned()),
            ..declared
        };
        assert_eq!(
            snapshot_target(&bad)
                .expect_err("a usage error")
                .exit_code(),
            2
        );
    }

    /// A refusal's summary exits 1 and names every refused setting and every data type whose
    /// sampled rows didn't match, keeping both lists whole within the size Kubernetes keeps
    #[test]
    fn a_refusal_summary_names_the_refused_settings() {
        // refuse two changed settings and one sampled data type
        let refusal = ApplyError::Refused(vec![
            ApplyError::ConfigChanged {
                setting: "thorium.files.partition_size".to_owned(),
                recorded: Some("180".to_owned()),
                configured: Some("3600".to_owned()),
            },
            ApplyError::ConfigChanged {
                setting: "thorium.files.password".to_owned(),
                recorded: None,
                configured: None,
            },
            ApplyError::SampledSizeMismatch {
                data_type: "tags.files".to_owned(),
                mismatched: 3,
                suggested: Some(3600),
            },
        ]);
        let text = summary(&Err(Failure::from(refusal)));
        assert!(text.len() <= TERMINATION_LIMIT, "{} bytes", text.len());
        let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
        assert_eq!(parsed["outcome"], "failed_systematic");
        assert_eq!(parsed["exit_code"], 1);
        assert_eq!(parsed["retryable"], false);
        assert_eq!(
            parsed["refused"],
            json!(["thorium.files.partition_size", "thorium.files.password"])
        );
        assert_eq!(parsed["sampled"], json!(["tags.files"]));
        assert!(
            parsed["error"]
                .as_str()
                .is_some_and(|error| error.contains("rebucket-files")),
            "{text}"
        );
        // a failure that refused nothing has empty lists
        let text = summary(&Err(Error::Transient("down".to_owned()).into()));
        let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
        assert_eq!(parsed["refused"], json!([]));
        assert_eq!(parsed["sampled"], json!([]));
        // every setting that can be refused, refused at once with long messages, still fits with
        // the lists whole
        let every = thorium::models::schema::DATA_TYPES
            .iter()
            .map(|data_type| format!("thorium.{data_type}.partition_size"))
            .chain(std::iter::once("thorium.files.password".to_owned()))
            .collect::<Vec<_>>();
        let refusals = every
            .iter()
            .map(|setting| ApplyError::ConfigChanged {
                setting: setting.clone(),
                recorded: Some("1".repeat(300)),
                configured: Some("2".repeat(300)),
            })
            .collect::<Vec<_>>();
        let text = summary(&Err(Failure::from(ApplyError::Refused(refusals))));
        assert!(text.len() <= TERMINATION_LIMIT, "{} bytes", text.len());
        let parsed: Value = serde_json::from_str(&text).expect("the summary is JSON");
        assert_eq!(parsed["refused"], json!(every));
    }
}
