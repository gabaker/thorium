//! `thoradm migrate check`: classifies what a migration would touch, writing nothing

use chrono::Utc;
use serde_json::json;
use thorium::models::schema::{RunLimits, RunMode, RunOptions, SAMPLE_KEYS, check_pass};

use super::context::{self, JOB_UID_ENV, migrate_error};
use super::registry::Support;
use crate::Error;
use crate::args::{Args, CheckMigration};
use crate::outcome::RunOutcome;
use crate::shared::termination::write_summary;

/// Check what a migration would touch
///
/// The check reads and classifies every object and changes nothing: its only write is its own
/// `job_result` row, which carries the report. The report goes to stdout as JSON; with `--all`
/// or `--samples` the groups follow, to stdout only.
///
/// # Arguments
///
/// * `cmd` - The migration to check
/// * `args` - The arguments thoradm was started with
async fn run(cmd: &CheckMigration, args: &Args) -> Result<(RunOutcome, Option<String>), Error> {
    // a migration thoradm can't run has nothing to check
    let def = super::find(&cmd.id)?;
    if def.support == Support::No {
        let outcome = super::run_or_verify(
            &crate::args::MigrationTarget {
                id: Some(cmd.id.clone()),
                positional_id: None,
            },
            "check",
        )?;
        return Ok((outcome, None));
    }
    // classify every object with no early stop
    let env = context::connect(args).await?;
    let cancel = context::cancel_on_signal();
    let limits = RunLimits {
        batch_rows: cmd.batch_rows,
        ..RunLimits::default()
    };
    let ctx = context::context(&env, RunMode::Check, cancel, limits);
    let spec = context::spec_for(def, std::iter::empty(), None);
    let opts = RunOptions {
        slices: cmd.slices,
        ..RunOptions::default()
    };
    let mut run = check_pass(def.runner, &spec, &opts, &ctx)
        .await
        .map_err(migrate_error)?;
    // a step that reverses a lossy one says what it would lose, with the counts of this check
    run.report.loss = super::loss::estimate(def.id, &run.report.counts);
    // the report is the check Job's own result row, and the only thing a check writes
    let id = std::env::var(JOB_UID_ENV)
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| format!("check/{}/{}", def.id, Utc::now().to_rfc3339()));
    env.store
        .write_result(&id, &run.report)
        .await
        .map_err(migrate_error)?;
    // print the report, and the groups with their samples when asked, to stdout only
    let mut printed = serde_json::to_value(&run.report)?;
    if cmd.all || cmd.samples {
        let groups = run
            .groups
            .iter()
            .map(|group| {
                let mut line = json!({
                    "transform": group.transform,
                    "reason": group.reason,
                    "class": group.class.as_str(),
                    "count": group.count,
                });
                if cmd.samples {
                    line["samples"] =
                        json!(group.samples.iter().take(SAMPLE_KEYS).collect::<Vec<_>>());
                }
                line
            })
            .collect::<Vec<_>>();
        printed["groups"] = json!(groups);
        printed["applied"] = serde_json::to_value(&run.applied)?;
    }
    println!("{printed}");
    Ok((RunOutcome::Complete, Some(id)))
}

/// Check what a migration would touch, and leave a termination summary on every exit
///
/// # Arguments
///
/// * `cmd` - The migration to check
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &CheckMigration, args: &Args) -> Result<RunOutcome, Error> {
    // the summary names the outcome and the id of the result row the check wrote
    let result = run(cmd, args).await;
    let summary = match &result {
        Ok((_, id)) => json!({"outcome": "done", "migration": cmd.id, "result_id": id}),
        Err(err) => json!({
            "outcome": "failed_systematic",
            "exit_code": err.exit_code(),
            "retryable": matches!(err, Error::Transient(_)),
            "error": err.to_string().chars().take(512).collect::<String>(),
        }),
    };
    write_summary(&summary.to_string());
    result.map(|(outcome, _)| outcome)
}
