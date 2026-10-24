//! `thoradm migrate revert`: moves a cluster back to an older revision
//!
//! The revert reads the recorded revision and history, plans the reverse path through the catalog,
//! and runs the data migration of each step in order, the way the operator does for a lowered
//! `target_revision`. A step that loses data runs only with the confirmation code it shows, one
//! step per code, and a step thoradm can't run stops the revert with its manual procedure. Once
//! every step has run, the older revision is recorded.

use chrono::Utc;
use std::collections::BTreeSet;
use thorium::models::schema::{
    DeploymentRecord, MigrationRunStatus, PassCounts, RecordStatus, read_status, record_revision,
};
use thorium::models::upgrades::{
    AppliedStep, BlockReason, Catalog, ConfirmRefusal, LossGate, LossVerdict, LossySetting,
    PlannedStep, StepDef, StepLoss, StepState, UpgradeApproval, UpgradeState, confirmation_code,
};

use super::registry::Support;
use super::run;
use crate::Error;
use crate::args::{Args, MigrationTarget, RevertMigrations, RunMigration};
use crate::outcome::RunOutcome;

/// How long to wait for Scylla to accept connections, in seconds
const WAIT_FOR_SCYLLA_SECS: u64 = 60;

/// Build the state to plan a revert from: the record's revision with a `Done` entry for every step
/// its whole history finished, followed by the entries it returned
///
/// # Arguments
///
/// * `record` - The deployment's record
fn planning_state(record: &DeploymentRecord) -> UpgradeState {
    // the finished steps the returned window doesn't hold are added before every entry it holds
    let mut state = UpgradeState::new(record.revision.clone(), env!("CARGO_PKG_VERSION"));
    for finished in &record.finished {
        let held = record.history.iter().any(|entry| {
            entry.revision == finished.revision.as_str()
                && entry.step == finished.step
                && matches!(entry.outcome, StepState::Done | StepState::Skipped)
        });
        if !held {
            state.applied.push(AppliedStep {
                revision: finished.revision.to_string(),
                step: finished.step.clone(),
                outcome: StepState::Done,
                ..AppliedStep::default()
            });
        }
    }
    state.applied.extend(record.history.iter().cloned());
    state
}

/// Get what a lossy step loses with whether `thoradm backup` covers each field, the way the
/// operator's check estimates it
///
/// Object counts stay empty, since the revert runs no check pass first.
///
/// # Arguments
///
/// * `planned` - The planned step
/// * `def` - The catalog step
fn covered_loss(planned: &PlannedStep, def: &StepDef) -> Option<StepLoss> {
    // the plan's loss, with each field's coverage from the same estimate the operator's check makes
    let planned_loss = planned.loss.as_ref()?;
    let estimated = def
        .thoradm_migration
        .and_then(|migration| super::loss::estimate(migration, &PassCounts::default()));
    Some(estimated.unwrap_or_else(|| planned_loss.clone()))
}

/// List the lost fields a backup doesn't cover
///
/// # Arguments
///
/// * `loss` - What the step loses, with its coverage
fn gaps(loss: &StepLoss) -> Vec<&str> {
    // the fields whose coverage says no backup carries them
    loss.fields
        .iter()
        .filter(|field| !field.backed_up)
        .map(|field| field.field.as_str())
        .collect()
}

/// Say what a lossy step loses, whether a backup covers it, and the code that accepts it
///
/// # Arguments
///
/// * `planned` - The planned step
/// * `loss` - What it loses, with its coverage
/// * `code` - The step's confirmation code
fn loss_line(planned: &PlannedStep, loss: &StepLoss, code: &str) -> String {
    // each field with whether `thoradm backup` carries it
    let fields = loss
        .fields
        .iter()
        .map(|field| {
            let coverage = if field.backed_up {
                "covered by thoradm backup"
            } else {
                "not covered by thoradm backup"
            };
            format!("{} ({coverage})", field.field)
        })
        .collect::<Vec<_>>()
        .join(", ");
    // a field no backup carries also needs its gap accepted
    let gap = if gaps(loss).is_empty() {
        ""
    } else {
        " and --accept-backup-gaps"
    };
    format!(
        "    {} loses {fields}: {}; accept it with --confirm {code}{gap}",
        planned.step, loss.description
    )
}

/// Say which components a step expects to be scaled down while it runs, since the revert stops
/// none of them itself
///
/// Returns `None` for a step that pauses nothing.
///
/// # Arguments
///
/// * `planned` - The planned step
/// * `def` - The catalog step
fn pause_line(planned: &PlannedStep, def: &StepDef) -> Option<String> {
    // a step that pauses nothing needs no line
    if def.pause.is_empty() {
        return None;
    }
    // name each component's Deployment
    let names = def
        .pause
        .iter()
        .map(|component| component.deployment_name())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "    {} expects {names} to be stopped while it runs; this command stops none of them, so \
         scale them to zero first",
        planned.step
    ))
}

/// Say how to resume a step whose run was interrupted
///
/// A lossy step's confirmation code depends on the length of the history, which the interrupted
/// run's own entries grew, so its rerun needs the new code a dry run shows.
///
/// # Arguments
///
/// * `step` - The step that was interrupted
/// * `to` - The revision the revert moves to
/// * `lossy` - Whether the step loses data
fn interrupted_message(step: &str, to: &str, lossy: bool) -> String {
    // a lossy step needs its new code; any other step resumes with the same command
    if lossy {
        format!(
            "step {step} was interrupted; its confirmation code changed when the interrupted run \
             was recorded, so run `thoradm migrate revert --to {to} --dry-run` for its new code, \
             then run the revert again with --confirm and that code to resume it"
        )
    } else {
        format!("step {step} was interrupted; run the revert again to resume it")
    }
}

/// Judge a lossy step with the gate the operator uses, refusing it when a field no backup covers
/// hasn't had its gap accepted
///
/// # Arguments
///
/// * `planned` - The planned step
/// * `loss` - What the step loses, with its coverage
/// * `code` - The step's confirmation code, which the command repeated
/// * `cmd` - What the revert was asked to do
fn refuse_gaps(
    planned: &PlannedStep,
    loss: &StepLoss,
    code: &str,
    cmd: &RevertMigrations,
) -> Result<(), Error> {
    // the command's flags as the approval the operator's gate judges
    let approval = UpgradeApproval {
        step: planned.step.clone(),
        backup: cmd.backup.clone().unwrap_or_default(),
        accept_loss: true,
        confirm: Some(code.to_owned()),
        accept_backup_gaps: cmd.accept_backup_gaps,
        ..UpgradeApproval::default()
    };
    // a code consumed by an earlier run is refused by the run itself, so none is passed here
    let consumed = BTreeSet::new();
    let verdict = LossGate {
        setting: LossySetting::Off,
        code,
        loss,
        approval: Some(&approval),
        consumed: &consumed,
    }
    .judge();
    match verdict {
        LossVerdict::Run { .. } => Ok(()),
        LossVerdict::Blocked {
            reason:
                BlockReason::ConfirmationRefused {
                    refusal: ConfirmRefusal::BackupGap,
                    ..
                },
            ..
        } => Err(Error::Usage(format!(
            "step {} loses {}, which thoradm backup doesn't cover, so a backup can't restore it; \
             rerun with --accept-backup-gaps to accept that",
            planned.step,
            gaps(loss).join(", ")
        ))),
        LossVerdict::Blocked { message, .. } => Err(Error::Usage(message)),
    }
}

/// Check that a step may run: a lossy step has its confirmation code and its backup gaps accepted,
/// and every step names a backup
///
/// Returns what the step loses, with its backup coverage, when it is lossy.
///
/// # Arguments
///
/// * `planned` - The planned step
/// * `def` - The catalog step
/// * `cmd` - What the revert was asked to do
/// * `revision` - The revision the deployment is recorded at
/// * `history` - How many entries the applied history holds
fn gate_step(
    planned: &PlannedStep,
    def: &StepDef,
    cmd: &RevertMigrations,
    revision: &str,
    history: usize,
) -> Result<Option<StepLoss>, Error> {
    // a step that loses data runs only with its own code
    let loss = covered_loss(planned, def);
    let mut code = None;
    if let Some(loss) = &loss {
        let shown = confirmation_code(&planned.step, revision, loss, history);
        if cmd.confirm.as_deref() != Some(shown.as_str()) {
            return Err(Error::Usage(format!(
                "step {} loses data ({}) and needs its confirmation code; rerun with \
                 --confirm {shown} and --backup naming the backup you took",
                planned.step, loss.description
            )));
        }
        code = Some(shown);
    }
    // every step that changes data names the backup taken first
    if cmd
        .backup
        .as_deref()
        .is_none_or(|backup| backup.trim().is_empty())
    {
        return Err(Error::Usage(format!(
            "step {} changes data: take a backup and rerun with --backup naming it",
            planned.step
        )));
    }
    // a field no backup covers needs its gap accepted, the way the operator's gate judges it
    if let (Some(loss), Some(code)) = (&loss, &code) {
        refuse_gaps(planned, loss, code, cmd)?;
    }
    Ok(loss)
}

/// Build the `Running` entry a step's run records first
///
/// # Arguments
///
/// * `planned` - The planned step
/// * `def` - The catalog step
/// * `cmd` - What the revert was asked to do
/// * `loss` - What the step loses, if it is lossy
fn running_entry(
    planned: &PlannedStep,
    def: &StepDef,
    cmd: &RevertMigrations,
    loss: Option<&StepLoss>,
) -> AppliedStep {
    // the entry the operator would record, by the person who ran the revert
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown-user".to_owned());
    AppliedStep {
        revision: planned.revision.clone(),
        step: planned.step.clone(),
        outcome: StepState::Running,
        message: Some(format!(
            "Reverting with thoradm migrate revert to {}",
            cmd.to
        )),
        at: Some(Utc::now().to_rfc3339()),
        by: Some(format!("thoradm {} ({user})", env!("CARGO_PKG_VERSION"))),
        backup: cmd.backup.clone(),
        confirm: loss.and(cmd.confirm.clone()),
        loss: loss.cloned(),
        chain: vec![def.id.to_owned()],
        ..AppliedStep::default()
    }
}

/// Build the arguments of the run of a step's migration
///
/// # Arguments
///
/// * `migration` - The migration's id
/// * `entry` - The step's `Running` entry
/// * `cmd` - What the revert was asked to do
fn run_args(migration: &str, entry: &AppliedStep, cmd: &RevertMigrations) -> RunMigration {
    // the same defaults `migrate run` takes, with the entry and the code the revert holds
    RunMigration {
        target: MigrationTarget {
            id: Some(migration.to_owned()),
            positional_id: None,
        },
        objects: Vec::new(),
        reason: None,
        accept_issues: Vec::new(),
        running_entry: serde_json::to_string(entry).ok(),
        confirm: entry.confirm.clone(),
        force_break_lease: false,
        backup: cmd.backup.clone(),
        slices: 16,
        batch_rows: 10_000,
        checkpoint_ms: 1000,
        retries: 12,
        backoff_ms: 250,
    }
}

/// Revert a cluster to an older revision
///
/// # Arguments
///
/// * `cmd` - The revision to revert to
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &RevertMigrations, args: &Args) -> Result<RunOutcome, Error> {
    // the target must be a revision, and the config must load, before anything is touched
    let target = super::revision(&cmd.to)?;
    let conf = crate::schema::load_conf(args)?;
    let session = crate::shared::scylla::connect_waiting(&conf, Some(WAIT_FOR_SCYLLA_SECS)).await?;
    let catalog: Catalog = super::catalog();
    // the record says where the deployment is and what it finished
    let status = read_status(&session, &conf, &catalog)
        .await
        .map_err(|err| Error::Transient(err.to_string()))?;
    let RecordStatus::Recorded { record, .. } = status else {
        return Err(Error::Usage(format!(
            "the deployment records no revision to revert from ({status:?}); nothing was changed"
        )));
    };
    let state = planning_state(&record);
    let history = usize::try_from(record.history_len).unwrap_or(usize::MAX);
    // plan the way down
    let steps = catalog
        .plan(&state, &target)
        .map_err(|err| Error::Usage(err.to_string()))?;
    println!(
        "Reverting from {} to {target} runs {} steps:",
        record.revision,
        steps.len()
    );
    for planned in &steps {
        println!(
            "  {} {}: {}",
            planned.revision, planned.step, planned.description
        );
        // the components the step expects to be stopped, which the revert leaves running
        let def = catalog.step(&planned.step).map(|(_, def)| def);
        if let Some(line) = def.and_then(|def| pause_line(planned, def)) {
            println!("{line}");
        }
        // what a lossy step loses, with the backup's coverage of each field
        if let Some(loss) = def.and_then(|def| covered_loss(planned, def)) {
            let code = confirmation_code(&planned.step, record.revision.as_str(), &loss, history);
            println!("{}", loss_line(planned, &loss, &code));
        }
    }
    if cmd.dry_run {
        return Ok(RunOutcome::Complete);
    }
    // run each step in order, stopping at the first thoradm can't run or that needs its code
    for planned in &steps {
        let Some((_, def)) = catalog.step(&planned.step) else {
            return Err(Error::Usage(format!(
                "{} isn't in the catalog",
                planned.step
            )));
        };
        // a step with no migration of its own is the operator's to run
        let Some(migration) = def.thoradm_migration else {
            return Err(Error::NotImplemented(format!(
                "thoradm can't revert step {} on its own: {}",
                def.id,
                def.manual_procedure
                    .unwrap_or("it is run by the operator when the target revision is lowered")
            )));
        };
        let registered = super::find(migration)?;
        if registered.support != Support::Yes {
            return Err(super::not_implemented(registered, "revert"));
        }
        // the step's code, backup, and gap acceptance are checked before anything runs
        let loss = gate_step(planned, def, cmd, record.revision.as_str(), history)?;
        // run its migration, which records the step's own entries
        let entry = running_entry(planned, def, cmd, loss.as_ref());
        let outcome = run::run(registered, &run_args(migration, &entry, cmd), args, true).await?;
        if outcome.interrupted {
            return Err(Error::Transient(interrupted_message(
                &planned.step,
                &cmd.to,
                loss.is_some(),
            )));
        }
        if outcome.status == MigrationRunStatus::Failed {
            return Err(Error::Systematic(format!(
                "step {} failed; see `thoradm migrate issues --id {migration}`",
                planned.step
            )));
        }
        println!("Step {} finished", planned.step);
        // a lossy step's code is consumed, so a later one needs a code of its own
        if planned.loss.is_some() && steps.last().is_some_and(|last| last.step != planned.step) {
            println!(
                "Run the revert again for the next step: its confirmation code is shown by \
                 --dry-run once this step is recorded"
            );
            return Ok(RunOutcome::Complete);
        }
    }
    // every step ran, so the older revision is recorded
    record_revision(
        &session,
        &conf,
        &catalog,
        &target,
        env!("CARGO_PKG_VERSION"),
    )
    .await
    .map_err(|err| Error::Generic(err.to_string()))?;
    println!("Recorded revision {target}");
    Ok(RunOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use super::interrupted_message;

    /// An interrupted lossy step points at the dry run for its new code, and any other step is
    /// resumed by running the revert again
    #[test]
    fn an_interrupted_lossy_step_points_at_its_new_code() {
        // a lossy step names the dry run and the code it shows
        let lossy = interrupted_message("fixture-reverse", "2026-10-v01", true);
        assert!(
            lossy.contains("thoradm migrate revert --to 2026-10-v01 --dry-run"),
            "{lossy}"
        );
        assert!(lossy.contains("new code"), "{lossy}");
        // any other step resumes with the same command
        let plain = interrupted_message("users-settings-theme-reverse", "2026-10-v02", false);
        assert_eq!(
            plain,
            "step users-settings-theme-reverse was interrupted; run the revert again to resume it"
        );
    }
}
