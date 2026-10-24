//! `thoradm migrate run`: runs a data migration through the runner

use chrono::Utc;
use serde_json::json;
use std::time::Duration;
use thorium::models::schema::{
    AppliedReport, GroupTotal, MigrateError, MigrationRunStatus, PassCounts, PassOutcome,
    RunLimits, RunMode, RunOptions, append_history, claim_consumed_code, reingests, run_pass,
};
use thorium::models::upgrades::{AppliedStep, CONSUMED_REFUSAL, IssueSummary, StepState};

use super::context::{self, migrate_error};
use super::registry::{MigrationDef, Support};
use crate::Error;
use crate::args::{Args, RunMigration};
use crate::outcome::RunOutcome;
use crate::shared::termination::{TERMINATION_LIMIT, write_summary};

/// How many of the largest issue groups a summary names
const SUMMARY_GROUPS: usize = 5;

/// The groups a summary names, largest first, ties broken by code
///
/// # Arguments
///
/// * `groups` - Every issue group
fn largest(groups: &[GroupTotal]) -> Vec<serde_json::Value> {
    // the groups a person needs first, in the order the status shows them
    let mut sorted = groups
        .iter()
        .filter(|group| group.count > 0)
        .collect::<Vec<_>>();
    sorted.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    sorted
        .into_iter()
        .take(SUMMARY_GROUPS)
        .map(|group| json!({"code": group.reason, "class": group.class.as_str(), "count": group.count}))
        .collect()
}

/// Build the short summary written as the Job's termination message
///
/// # Arguments
///
/// * `migration` - The migration's id
/// * `result` - How the run ended
fn summary(migration: &str, result: &Result<PassOutcome, Error>) -> String {
    let text = match result {
        Ok(outcome) => {
            // name how the pass ended, with its counts and its largest groups
            let name = if outcome.interrupted {
                "interrupted"
            } else {
                match outcome.status {
                    MigrationRunStatus::Done => "done",
                    MigrationRunStatus::DoneWithIssues => "done_with_issues",
                    MigrationRunStatus::Failed if outcome.failure.is_some() => "failed_systematic",
                    MigrationRunStatus::Failed => "failed_issues",
                    MigrationRunStatus::Running => "interrupted",
                }
            };
            json!({
                "outcome": name,
                "migration": migration,
                "counts": outcome.counts,
                "groups": largest(&outcome.groups),
                "failure": outcome.failure,
                "resumed": outcome.resumed,
            })
        }
        Err(err) => json!({
            "outcome": "failed_systematic",
            "exit_code": err.exit_code(),
            "retryable": matches!(err, Error::Transient(_)),
            "error": err.to_string().chars().take(512).collect::<String>(),
        }),
    };
    // the summary stays whole JSON within what Kubernetes keeps
    let rendered = text.to_string();
    if rendered.len() <= TERMINATION_LIMIT {
        return rendered;
    }
    json!({"outcome": text["outcome"], "migration": migration}).to_string()
}

/// Build the run's bounds from the command's flags
///
/// # Arguments
///
/// * `cmd` - The migration to run
fn limits(cmd: &RunMigration) -> RunLimits {
    // the design's defaults, overridden by what was given
    RunLimits {
        batch_rows: cmd.batch_rows,
        checkpoint_every: Duration::from_millis(cmd.checkpoint_ms),
        retries: cmd.retries,
        backoff: Duration::from_millis(cmd.backoff_ms),
        ..RunLimits::default()
    }
}

/// How many objects a reason's group holds, summed across workers
///
/// # Arguments
///
/// * `groups` - Every issue group
/// * `reason` - The reason code
fn held(groups: &[GroupTotal], reason: &str) -> u64 {
    // the code may appear under several transforms
    groups
        .iter()
        .filter(|group| group.reason == reason)
        .map(|group| group.count)
        .sum()
}

/// How many objects a recorded group holds, refusing a code no group of the pass has
///
/// # Arguments
///
/// * `env` - The backends
/// * `key` - The pass key
/// * `reason` - The reason code
async fn group_count(env: &context::Env, key: &str, reason: &str) -> Result<u64, Error> {
    // the groups of the pass's current attempt
    let groups = thorium::models::schema::issue_groups(&env.store, key)
        .await
        .map_err(migrate_error)?;
    if groups.iter().any(|group| group.reason == reason) {
        Ok(held(&groups, reason))
    } else {
        Err(Error::Usage(format!(
            "the pass has no issue group {reason}; see `thoradm migrate issues`"
        )))
    }
}

/// The outcome of a run that was stopped and left its pass resumable
fn interrupted() -> PassOutcome {
    // nothing finished, so there are no counts, groups, or report
    PassOutcome {
        status: MigrationRunStatus::Running,
        interrupted: true,
        failure: None,
        counts: PassCounts::default(),
        groups: Vec::new(),
        attempt: String::new(),
        resumed: true,
        report: AppliedReport::default(),
        largest_batch: 0,
    }
}

/// Parse the `Running` entry the operator passed with `--running-entry`
///
/// # Arguments
///
/// * `raw` - The entry as JSON
fn parse_running_entry(raw: &str) -> Result<AppliedStep, Error> {
    // the entry is the operator's own record of the step starting, so it must name the step
    let entry = serde_json::from_str::<AppliedStep>(raw)
        .map_err(|err| Error::Usage(format!("--running-entry isn't a history entry: {err}")))?;
    if entry.step.trim().is_empty() || entry.revision.trim().is_empty() {
        return Err(Error::Usage(
            "--running-entry must name the step and its revision".to_owned(),
        ));
    }
    if entry.outcome != StepState::Running {
        return Err(Error::Usage(
            "--running-entry must be a `Running` entry".to_owned(),
        ));
    }
    Ok(entry)
}

/// Whether this thoradm runs in a Job the operator created, which the operator names in the Job's
/// environment
fn in_job() -> bool {
    // a run by hand has no Job name
    std::env::var(super::context::JOB_NAME_ENV).is_ok_and(|job| !job.is_empty())
}

/// Who a step's entry says ran it: this thoradm, and the OS user for a run by hand
fn ran_by() -> String {
    // a Job is named by its Job; a run by hand by the user who ran it
    let version = env!("CARGO_PKG_VERSION");
    if in_job() {
        format!("thoradm {version}")
    } else {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown-user".to_owned());
        format!("thoradm {version} ({user})")
    }
}

/// Build the history entry that records how a step's run ended, when the run finished its pass
///
/// The entry is the step's own record of its outcome (`Done`, or `Skipped` when the first pass found
/// nothing to change), with the work the pass found, the issue groups it left, and the codes its
/// approval accepted. A pass that didn't finish, and a run that only re-processed named objects,
/// record nothing. A run in a Job the operator created records nothing either: the operator records
/// the Job's outcome from the termination summary, so the history holds one outcome per run. A run
/// by hand, `migrate revert` included, records its own. The step is the one the operator named in
/// its `Running` entry or, for a run by hand, the catalog step that performs the migration.
///
/// # Arguments
///
/// * `def` - The registered migration
/// * `cmd` - The migration to run
/// * `running` - The `Running` entry passed, if any
/// * `in_job` - Whether the run is in a Job the operator created
/// * `outcome` - How the pass ended
fn finished_entry(
    def: &MigrationDef,
    cmd: &RunMigration,
    running: Option<&AppliedStep>,
    in_job: bool,
    outcome: &PassOutcome,
) -> Option<AppliedStep> {
    // only a pass that ended `Done` or `DoneWithIssues` finishes its step
    let finished = matches!(
        outcome.status,
        MigrationRunStatus::Done | MigrationRunStatus::DoneWithIssues
    );
    if !finished || outcome.interrupted || !cmd.objects.is_empty() {
        return None;
    }
    // an operator's Job leaves its outcome entry to the operator, but an admin's re-run of a group
    // and a run by hand record their own
    if in_job && cmd.reason.is_none() {
        return None;
    }
    // the step and revision come from the operator's entry, or from the catalog for a run by hand
    let (revision, step) = match running {
        Some(entry) => (entry.revision.clone(), entry.step.clone()),
        None => (
            super::revision_of(def.id)?,
            super::step_for(def.id)?.id.to_owned(),
        ),
    };
    // a first pass that found nothing to change is a skipped step, but a resumed pass counts only
    // what it did itself, so one that finds nothing left finished work an earlier pod did
    let counts = outcome.counts;
    let nothing = outcome.status == MigrationRunStatus::Done
        && !outcome.resumed
        && counts.migratable == 0
        && counts.written == 0
        && counts.flagged == 0;
    let held = outcome.groups.iter().filter(|group| group.count > 0);
    let accepted = outcome
        .groups
        .iter()
        .filter(|group| group.accepted)
        .map(|group| group.reason.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let issues = IssueSummary {
        groups: u32::try_from(held.clone().count()).unwrap_or(u32::MAX),
        objects: held.map(|group| group.count).sum(),
        accepted: accepted.into_iter().collect(),
    };
    Some(AppliedStep {
        revision,
        step,
        outcome: if nothing {
            StepState::Skipped
        } else if reingests(def.id) {
            // a derived rebuild only starts its reingest, which the search-streamer finishes later
            StepState::Reingesting
        } else {
            StepState::Done
        },
        message: Some(if nothing {
            format!("{} had nothing to change", def.id)
        } else {
            format!("{} finished: {}", def.id, outcome.status.as_str())
        }),
        at: Some(Utc::now().to_rfc3339()),
        by: Some(ran_by()),
        backup: cmd.backup.clone(),
        confirm: running
            .and_then(|entry| entry.confirm.clone())
            .or_else(|| cmd.confirm.clone()),
        loss: running.and_then(|entry| entry.loss.clone()),
        work: Some(counts.work_text()),
        job: running.and_then(|entry| entry.job.clone()).or_else(|| {
            std::env::var(super::context::JOB_NAME_ENV)
                .ok()
                .filter(|job| !job.is_empty())
        }),
        chain: def.chain.iter().map(|id| (*id).to_owned()).collect(),
        issues: (issues.groups > 0 || !issues.accepted.is_empty()).then_some(issues),
        ..AppliedStep::default()
    })
}

/// Consume a lossy step's confirmation code, refusing the run when it was consumed before
///
/// The code is written as a `consumed_code` row, which is never deleted, with the entry of the run
/// that claims it.
///
/// # Arguments
///
/// * `env` - The backends
/// * `def` - The registered migration
/// * `cmd` - The migration to run
/// * `code` - The confirmation code
/// * `running` - The `Running` entry the operator passed, if any
async fn consume(
    env: &context::Env,
    def: &MigrationDef,
    cmd: &RunMigration,
    code: &str,
    running: Option<&AppliedStep>,
) -> Result<(), Error> {
    // the row holds the operator's entry, or one built from the catalog step for a run by hand
    let entry = match running {
        Some(entry) => entry.clone(),
        None => AppliedStep {
            revision: super::revision_of(def.id).ok_or_else(|| {
                Error::Usage(
                    "--confirm needs --running-entry for a migration no catalog step performs"
                        .to_owned(),
                )
            })?,
            step: super::step_for(def.id).map_or_else(String::new, |step| step.id.to_owned()),
            outcome: StepState::Running,
            at: Some(Utc::now().to_rfc3339()),
            by: Some(ran_by()),
            backup: cmd.backup.clone(),
            confirm: Some(code.to_owned()),
            ..AppliedStep::default()
        },
    };
    // claim it, which fails when a row for the code exists
    let claimed = claim_consumed_code(&env.backends.scylla, &env.conf, code, &entry)
        .await
        .map_err(|err| Error::Transient(err.to_string()))?;
    if claimed {
        Ok(())
    } else {
        Err(Error::Systematic(format!(
            "the confirmation code {code} {CONSUMED_REFUSAL}; the step blocks again with a new code"
        )))
    }
}

/// Run the migration and return how its pass ended
///
/// # Arguments
///
/// * `def` - The registered migration
/// * `cmd` - The migration to run
/// * `args` - The arguments thoradm was started with
/// * `records_own` - Whether the caller is a person's command, such as `migrate revert`, that
///   records the step's outcome itself whatever the environment says about a Job
pub(super) async fn run(
    def: &MigrationDef,
    cmd: &RunMigration,
    args: &Args,
    records_own: bool,
) -> Result<PassOutcome, Error> {
    // an entry that can't be read is refused before anything is touched
    let running = cmd
        .running_entry
        .as_deref()
        .map(parse_running_entry)
        .transpose()?;
    // connect, take the lease, and trap SIGTERM before anything is written
    let env = context::connect(args).await?;
    let lease = context::take_lease(&env, cmd.force_break_lease, true).await?;
    let cancel = context::cancel_on_signal();
    // a lossy step's confirmation code is consumed before anything else is written, and a code
    // already consumed refuses the run
    if let Some(code) = cmd.confirm.as_deref()
        && let Err(err) = consume(&env, def, cmd, code, running.as_ref()).await
    {
        lease.release().await;
        return Err(err);
    }
    // the step's `Running` entry, with the replica counts the operator paused, is the first thing
    // written, so the counts survive a lost state `ConfigMap`
    if let Some(entry) = &running
        && let Err(err) =
            append_history(&env.backends.scylla, &env.conf, std::slice::from_ref(entry)).await
    {
        lease.release().await;
        return Err(Error::Transient(err.to_string()));
    }
    let spec = context::spec_for(def, cmd.accept_issues.iter().cloned(), cmd.backup.clone());
    let opts = RunOptions {
        slices: cmd.slices,
        objects: cmd.objects.clone(),
        reason: cmd.reason.clone(),
    };
    let ctx = context::context(&env, RunMode::Run, cancel, limits(cmd));
    // a re-run of one group prints the group's count before and after, and names a group that
    // exists
    let before = match &cmd.reason {
        Some(reason) => match group_count(&env, &spec.key, reason).await {
            Ok(was) => Some((reason.clone(), was)),
            Err(err) => {
                lease.release().await;
                return Err(err);
            }
        },
        None => None,
    };
    let outcome = run_pass(def.runner, &spec, &opts, &ctx, &env.store, &lease).await;
    // the step's own entry records how its pass ended, before the lease is let go; a Job name in the
    // environment hands the outcome to the operator unless a person's command is the caller
    let recorded = match &outcome {
        Ok(done) => {
            match finished_entry(def, cmd, running.as_ref(), in_job() && !records_own, done) {
                Some(entry) => append_history(&env.backends.scylla, &env.conf, &[entry])
                    .await
                    .map(|_| ())
                    .map_err(|err| Error::Transient(err.to_string())),
                None => Ok(()),
            }
        }
        Err(_) => Ok(()),
    };
    lease.release().await;
    recorded?;
    // a run stopped by the census step's own bound is an interruption, not a failure
    let outcome = match outcome {
        Err(MigrateError::Cancelled) => interrupted(),
        other => other.map_err(migrate_error)?,
    };
    if let Some((reason, was)) = before {
        let now = held(&outcome.groups, &reason);
        println!("group {reason}: {was} objects before, {now} after");
        if now > 0 {
            return Err(Error::DataIssues(format!(
                "the group {reason} still holds {now} objects"
            )));
        }
    }
    Ok(outcome)
}

/// Run a data migration
///
/// # Arguments
///
/// * `cmd` - The migration to run
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &RunMigration, args: &Args) -> Result<RunOutcome, Error> {
    // a migration thoradm can't perform yet reports its manual procedure
    let def = super::find(cmd.target.id())?;
    // a migration that can only be checked is refused before any pass row, lease, or slice exists
    if def.support != Support::Yes {
        return super::run_or_verify(&cmd.target, "run");
    }
    // run it, then summarize the outcome for the Job whatever it was
    let result = run(def, cmd, args, false).await;
    write_summary(&summary(def.id, &result));
    let outcome = result?;
    println!(
        "{}",
        json!({
            "migration": def.id,
            "status": outcome.status.as_str(),
            "interrupted": outcome.interrupted,
            "attempt": outcome.attempt,
            "resumed": outcome.resumed,
            "counts": outcome.counts,
            "groups": outcome.groups.iter().map(|group| json!({
                "transform": group.transform,
                "reason": group.reason,
                "class": group.class.as_str(),
                "count": group.count,
                "resolved": group.resolved,
                "accepted": group.accepted,
            })).collect::<Vec<_>>(),
        })
    );
    // a SIGTERM leaves the pass resumable, which a Job retries
    if outcome.interrupted {
        return Err(Error::Transient(
            "interrupted by SIGTERM; the pass resumes from its checkpoints".to_owned(),
        ));
    }
    // a pass over its group limit, or failed systematically, would fail the same way again
    ended(def.id, cmd.reason.is_some(), &outcome)
}

/// Decide the exit of a run from how its pass ended
///
/// # Arguments
///
/// * `id` - The migration's id
/// * `reasoned` - Whether the run re-ran one issue group, whose own count was already judged
/// * `outcome` - How the pass ended
fn ended(id: &str, reasoned: bool, outcome: &PassOutcome) -> Result<RunOutcome, Error> {
    // a failed pass fails the command, except that a re-run of one group has already been judged
    // by that group's own count and leaves the pass's other groups to the next run
    match outcome.status {
        MigrationRunStatus::Failed => match &outcome.failure {
            Some(message) => Err(Error::Systematic(message.clone())),
            None if reasoned => Ok(RunOutcome::Complete),
            None => Err(Error::DataIssues(format!(
                "{id}: an issue group holds more objects than the step's limit; see `thoradm migrate issues --id {id}`"
            ))),
        },
        _ => Ok(RunOutcome::Complete),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use thorium::models::schema::PassCounts;

    /// A finished first pass with these counts
    ///
    /// # Arguments
    ///
    /// * `migratable` - How many objects it found to migrate
    fn finished(migratable: u64) -> PassOutcome {
        // a pass that ended `Done` with nothing else to report
        PassOutcome {
            status: MigrationRunStatus::Done,
            interrupted: false,
            failure: None,
            counts: PassCounts {
                read: migratable,
                migratable,
                written: migratable,
                ..PassCounts::default()
            },
            groups: Vec::new(),
            attempt: "attempt".to_owned(),
            resumed: false,
            report: AppliedReport::default(),
            largest_batch: 0,
        }
    }

    /// The command that runs a migration by hand
    ///
    /// # Arguments
    ///
    /// * `id` - The migration's id
    fn command(id: &str) -> RunMigration {
        // parse it the way the CLI does
        let args = crate::args::Args::try_parse_from(["thoradm", "migrate", "run", "--id", id])
            .expect("run should parse");
        let crate::args::SubCommands::Migrate(crate::args::MigrateSubCommands::Run(run)) = args.cmd
        else {
            panic!("expected migrate run");
        };
        run
    }

    /// The entry a re-run of a group by hand records for its step
    ///
    /// # Arguments
    ///
    /// * `id` - The migration's id
    /// * `outcome` - How its pass ended
    fn entry(id: &str, outcome: &PassOutcome) -> AppliedStep {
        // the operator's `Running` entry names the step, and an admin's re-run of a group records it
        let def = super::super::find(id).expect("registered");
        finished_entry(def, &reasoned(id), Some(&running(id)), true, outcome).expect("an entry")
    }

    /// An Elastic rebuild that dropped indexes ends its step `Reingesting`, since the reingest goes on
    /// after the run; one that found nothing to drop is skipped, and other migrations end `Done`
    #[test]
    fn a_rebuild_that_dropped_indexes_ends_reingesting() {
        // two indexes dropped: the search-streamer reingests them later
        assert_eq!(
            entry("elastic-rebuild", &finished(2)).outcome,
            StepState::Reingesting
        );
        // nothing mismatched: nothing to reingest
        assert_eq!(
            entry("elastic-rebuild", &finished(0)).outcome,
            StepState::Skipped
        );
        // a migration that converts objects in place is done when its pass is
        assert_eq!(
            entry("rebucket-files", &finished(2)).outcome,
            StepState::Done
        );
    }

    /// The command that re-runs one group of a migration by hand
    ///
    /// # Arguments
    ///
    /// * `id` - The migration's id
    fn reasoned(id: &str) -> RunMigration {
        // parse it the way the CLI does
        let args = crate::args::Args::try_parse_from([
            "thoradm",
            "migrate",
            "run",
            "--id",
            id,
            "--reason",
            "bad_type:body.name",
        ])
        .expect("run should parse");
        let crate::args::SubCommands::Migrate(crate::args::MigrateSubCommands::Run(run)) = args.cmd
        else {
            panic!("expected migrate run");
        };
        run
    }

    /// The `Running` entry of an operator's Job for a migration's step
    ///
    /// # Arguments
    ///
    /// * `id` - The migration's id
    fn running(id: &str) -> AppliedStep {
        // the operator's entry names the step and its revision
        AppliedStep {
            revision: "2026-10-v02".to_owned(),
            step: id.to_owned(),
            outcome: StepState::Running,
            ..AppliedStep::default()
        }
    }

    /// A step the operator runs in a Job has its outcome recorded by the operator, from the Job's
    /// summary, so the run records none of its own; a run by hand, such as `migrate revert`, and a
    /// `--reason` re-run record theirs even beside a `Running` entry
    #[test]
    fn an_operator_run_records_no_outcome_of_its_own() {
        let def = super::super::find("rebucket-files").expect("registered");
        let running = running("rebucket-files");
        let command = command("rebucket-files");
        // a pass that ended `Done` in the operator's Job leaves the outcome to the operator
        assert!(finished_entry(def, &command, Some(&running), true, &finished(2)).is_none());
        // the same pass run by hand beside the same entry is recorded by the run itself
        assert!(finished_entry(def, &command, Some(&running), false, &finished(2)).is_some());
        // an admin's re-run of a group is recorded by the run too, whatever started it
        let reasoned = reasoned("rebucket-files");
        assert!(finished_entry(def, &reasoned, Some(&running), true, &finished(2)).is_some());
    }

    /// A pass that failed on an issue group over its limit
    fn over_limit() -> PassOutcome {
        // a failed pass with no failure message, as a group over the step's limit ends it
        PassOutcome {
            status: MigrationRunStatus::Failed,
            ..finished(0)
        }
    }

    /// A re-run of one group exits zero once that group is empty, though the pass still fails on
    /// another group, while a plain run, and a pass that failed for another reason, still fail
    #[test]
    fn a_reason_run_exits_zero_when_its_own_group_is_empty() {
        // the pass still fails on another group: the re-run of the emptied group is done
        assert!(matches!(
            ended("rebucket-files", true, &over_limit()),
            Ok(RunOutcome::Complete)
        ));
        // a run that isn't a re-run of a group fails on the group
        assert!(matches!(
            ended("rebucket-files", false, &over_limit()),
            Err(Error::DataIssues(_))
        ));
        // a pass that failed on something else fails either way
        let broken = PassOutcome {
            failure: Some("the backend broke".to_owned()),
            ..over_limit()
        };
        assert!(matches!(
            ended("rebucket-files", true, &broken),
            Err(Error::Systematic(_))
        ));
    }

    /// A resumed pass that finds nothing left finished a step an earlier pod did the work of, so
    /// the step isn't recorded skipped
    #[test]
    fn a_resumed_pass_with_nothing_left_is_not_skipped() {
        let def = super::super::find("rebucket-files").expect("registered");
        let running = running("rebucket-files");
        let command = reasoned("rebucket-files");
        // a first pass that found nothing to change is skipped
        let nothing = finished(0);
        let skipped =
            finished_entry(def, &command, Some(&running), false, &nothing).expect("an entry");
        assert_eq!(skipped.outcome, StepState::Skipped);
        // the same counts from a pass that resumed an earlier pod's are a finished step
        let resumed = PassOutcome {
            resumed: true,
            ..finished(0)
        };
        let done =
            finished_entry(def, &command, Some(&running), false, &resumed).expect("an entry");
        assert_eq!(done.outcome, StepState::Done);
    }

    /// The summary a Job leaves says whether its pass resumed an earlier pod's, so the operator
    /// doesn't read a resumed pass's own counts as the whole step's work
    #[test]
    fn the_summary_says_whether_the_pass_resumed() {
        // a pass that resumed an earlier pod's, and one that began its own
        for resumed in [true, false] {
            let outcome = PassOutcome {
                resumed,
                ..finished(0)
            };
            let text = summary("rebucket-files", &Ok(outcome));
            let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
            assert_eq!(parsed["resumed"], serde_json::json!(resumed), "{text}");
        }
    }
}
