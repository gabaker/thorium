//! Cancelling in-flight work when an approval asks a step to
//!
//! A step that cancels on approval waits like any check-only step until an approval opts in with
//! `cancel_in_flight` and repeats the step's confirmation code. The operator then records a quiesce
//! entry, a `Running` entry holding the replica counts of the step's `quiesce` components and the
//! code, which it consumes, saves the state, and only then scales those components to zero. After
//! the approval's grace, counted from the entry's `at` so an operator restart keeps it, the step's
//! run starts with its whole pause, whatever its check reports. A quiesce whose step stops being
//! the next one with a usable approval before its run starts is ended by scaling the components
//! back to their recorded counts.

use chrono::{DateTime, Utc};
use kube::Client;
use std::collections::BTreeSet;
use thorium::Error;
use thorium::models::upgrades::{
    AppliedStep, BlockReason, ConfirmRefusal, PlannedStep, QUIESCE_STEP, RESET_STATE_STEP,
    RESTORED_STEP, RevisionId, StepDef, StepState, UpgradeApproval, UpgradeState, consumed_codes,
    drain_code, grace_secs,
};

use super::migration::{self, Advance, Approved};
use super::pause::{self, RESTORE_STEP};
use super::state::{self, StoredState};
use super::{OPERATOR_VERSION, Pending, catalog};
use crate::k8s::clusters::ClusterMeta;

/// Find the newest quiesce entry of a step since the step last finished, with its position
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
fn newest<'a>(state: &'a UpgradeState, step: &str) -> Option<(usize, &'a AppliedStep)> {
    // walk back from the newest entry to the first that decides the step's attempt
    for (index, entry) in state.applied.iter().enumerate().rev() {
        // a reset or a restore moved the revision, so a quiesce before it is over
        if entry.step == RESET_STATE_STEP || entry.step == RESTORED_STEP {
            return None;
        }
        // a quiesce before the step finished belonged to that run
        if entry.step == step && entry.outcome.finishes() {
            return None;
        }
        if entry.step == QUIESCE_STEP && entry.chain.first().map(String::as_str) == Some(step) {
            return Some((index, entry));
        }
    }
    None
}

/// Find the open quiesce of a step: its newest quiesce entry when no run of the step started after
/// it and no restore ended it
///
/// A restore ends a quiesce when it names one of the quiesced components, or any restore does when
/// the quiesce found none of them deployed.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
#[must_use]
pub(super) fn open<'a>(state: &'a UpgradeState, step: &str) -> Option<&'a AppliedStep> {
    // the newest quiesce, unless a later entry started the run or ended the quiesce
    let (index, quiesce) = newest(state, step)?;
    let ended = state.applied[index + 1..].iter().any(|entry| {
        (entry.step == step && entry.outcome == StepState::Running)
            || (entry.step == RESTORE_STEP
                && (quiesce.paused.is_empty()
                    || entry
                        .paused
                        .keys()
                        .any(|name| quiesce.paused.contains_key(name))))
    });
    (!ended).then_some(quiesce)
}

/// Find the quiesce whose code an approval of a step must repeat now: the one before the step's
/// open run, or else the open quiesce
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
fn held<'a>(state: &'a UpgradeState, step: &str) -> Option<&'a AppliedStep> {
    // a run that started took over the quiesce recorded before it
    if let Some(run) = migration::open_run(state, step) {
        let started = state
            .applied
            .iter()
            .position(|entry| std::ptr::eq(entry, run))?;
        return newest(state, step)
            .filter(|(index, _)| *index < started)
            .map(|(_, quiesce)| quiesce);
    }
    open(state, step)
}

/// Get the confirmation code an approval of a step that cancels on approval must repeat now
///
/// While a quiesce or the run that followed it is open, that is the code the quiesce recorded;
/// otherwise it is the step's current code, which changes with every history entry.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step
/// * `target` - The revision the upgrade moves to
#[must_use]
pub(super) fn code(state: &UpgradeState, step: &StepDef, target: &RevisionId) -> String {
    // the recorded code of a quiesce still in use, or the current one
    held(state, step.id)
        .and_then(|quiesce| quiesce.confirm.clone())
        .unwrap_or_else(|| {
            drain_code(
                step.id,
                state.revision.as_str(),
                target.as_str(),
                &state.applied,
            )
        })
}

/// Judge the approvals of a step that cancels on approval: the first that opts in with a backup and
/// repeats the code, or why the first that opts in doesn't count
///
/// Returns the usable approval, or the refusal of an opted-in approval whose code is missing,
/// wrong, or consumed, or nothing when no approval opts in with a backup.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step
/// * `target` - The revision the upgrade moves to
/// * `approvals` - The approvals set on the cluster
fn judge<'a>(
    state: &UpgradeState,
    step: &StepDef,
    target: &RevisionId,
    approvals: &'a [UpgradeApproval],
) -> Result<&'a UpgradeApproval, Option<ConfirmRefusal>> {
    // only an approval with a backup and the opt-in can cancel
    let opted = approvals
        .iter()
        .filter(|approval| {
            approval.step == step.id
                && approval.cancel_in_flight
                && !approval.backup.trim().is_empty()
        })
        .collect::<Vec<_>>();
    // the code to repeat, and the codes earlier quiesces consumed, less the one still in use
    let expected = code(state, step, target);
    let own = held(state, step.id).and_then(|quiesce| quiesce.confirm.clone());
    let consumed = consumed_codes(state, step.id)
        .into_iter()
        .filter(|code| Some(code) != own.as_ref())
        .collect::<BTreeSet<_>>();
    let given = |approval: &UpgradeApproval| {
        approval
            .confirm
            .as_deref()
            .map_or_else(String::new, |code| code.trim().to_owned())
    };
    // the first that repeats the code counts, unless that code is spent
    if let Some(approval) = opted
        .iter()
        .find(|approval| given(approval) == expected && !consumed.contains(&expected))
    {
        return Ok(approval);
    }
    // otherwise the first opted-in approval says why none counts
    Err(opted.first().map(|approval| {
        let given = given(approval);
        if given.is_empty() {
            ConfirmRefusal::Missing
        } else if consumed.contains(&given) {
            ConfirmRefusal::Consumed
        } else {
            ConfirmRefusal::Mismatch
        }
    }))
}

/// Get the approval that lets a step that cancels on approval start: one with a backup,
/// `cancel_in_flight`, and the code the step asks for now, which no earlier quiesce consumed
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step
/// * `target` - The revision the upgrade moves to
/// * `approvals` - The approvals set on the cluster
#[must_use]
pub(super) fn usable<'a>(
    state: &UpgradeState,
    step: &StepDef,
    target: &RevisionId,
    approvals: &'a [UpgradeApproval],
) -> Option<&'a UpgradeApproval> {
    // a refusal or no opt-in alike leave the step waiting
    judge(state, step, target, approvals).ok()
}

/// Show the code of a step that cancels on approval while it waits, and say how to use it or why
/// the approval given doesn't match
///
/// Returns the text to add to the step's wait message.
///
/// # Arguments
///
/// * `pending` - The upgrade being run, whose step shows the code
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `approvals` - The approvals set on the cluster
pub(super) fn show_code(
    pending: &mut Pending,
    index: usize,
    step: &StepDef,
    approvals: &[UpgradeApproval],
) -> String {
    // the code the step asks for now
    let state = &pending.stored.state;
    let confirm = code(state, step, &pending.target);
    match judge(state, step, &pending.target, approvals) {
        // an opted-in approval with a code that doesn't count says why
        Err(Some(refusal)) => {
            let why = match refusal {
                ConfirmRefusal::Missing => "the approval repeats no confirmation code",
                ConfirmRefusal::Consumed => {
                    "the approval's confirmation code doesn't match (an earlier quiesce consumed \
                     it)"
                }
                _ => "the approval's confirmation code doesn't match",
            };
            let text = format!("; {why}; the current code is {confirm}");
            pending.steps[index].blocked =
                Some(BlockReason::ConfirmationRefused { confirm, refusal });
            text
        }
        // otherwise the step says how to cancel instead of waiting
        _ => {
            let text = format!(
                ". To cancel what is in flight instead, take a backup and add the approval \
                 {{step: {}, backup: <where your backup is>, cancelInFlight: true, confirm: \
                 {confirm}}} (Helm value operator.cluster.upgrade.approvals[]; cancel_in_flight: \
                 true in the ThoriumCluster), repeating the code the step shows as confirm",
                step.id
            );
            pending.steps[index].blocked = Some(BlockReason::CancelApproval { confirm });
            text
        }
    }
}

/// How many seconds of a quiesce's grace are left, counted from its recorded start
///
/// A start that can't be read leaves no grace.
///
/// # Arguments
///
/// * `quiesce` - The quiesce entry
/// * `approval` - The approval, which sets the grace
fn grace_left(quiesce: &AppliedStep, approval: &UpgradeApproval) -> u64 {
    // the grace ends its length after the recorded start
    let Some(at) = quiesce
        .at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
    else {
        return 0;
    };
    let grace = i64::try_from(grace_secs(approval)).unwrap_or(i64::MAX);
    let left = (at + chrono::Duration::seconds(grace) - migration::now()).num_milliseconds();
    // part of a second left still waits a whole one
    u64::try_from(left).map_or(0, |left| left.div_ceil(1000))
}

/// Build the quiesce entry of a step, with the counts of its quiesced components
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `approval` - The approval that started it
/// * `counts` - The replica counts of the components it stops
fn entry(
    pending: &Pending,
    index: usize,
    step: &StepDef,
    approval: &UpgradeApproval,
    counts: &pause::Counts,
) -> AppliedStep {
    // the code the approval repeated, which this entry consumes
    let confirm = approval
        .confirm
        .as_deref()
        .map_or_else(String::new, |code| code.trim().to_owned());
    let names = counts.counts.keys().cloned().collect::<Vec<_>>().join(", ");
    AppliedStep {
        revision: migration::step_revision(pending, index),
        step: QUIESCE_STEP.to_owned(),
        outcome: StepState::Running,
        message: Some(format!(
            "Stopping {names} before step {} cancels what is in flight",
            step.id
        )),
        at: Some(migration::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        backup: Some(approval.backup.trim().to_owned()),
        confirm: Some(confirm),
        chain: vec![step.id.to_owned()],
        paused: counts.counts.clone(),
        ..AppliedStep::default()
    }
}

/// Advance an approved step that cancels on approval and hasn't started its run: quiesce its
/// components, wait out the grace, then start the run with its whole pause
///
/// The run starts whatever the step's check reports, so the step never skips on this path.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `approval` - The usable approval
/// * `accept` - The issue codes the run is given
/// * `attempt` - The attempt counter
pub(super) async fn advance(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    approval: &UpgradeApproval,
    accept: &[String],
    attempt: usize,
) -> Result<Advance, Error> {
    // record the quiesce with its counts before scaling anything, so they outlive a lost state
    if open(&pending.stored.state, step.id).is_none() {
        let counts = pause::read_counts(meta, &pending.stored.state, step.quiesce).await?;
        let quiesce = entry(pending, index, step, approval, &counts);
        pending.stored.state.record(quiesce);
        state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    }
    let quiesce = open(&pending.stored.state, step.id)
        .cloned()
        .ok_or_else(|| Error::new(format!("The quiesce of step {} wasn't recorded", step.id)))?;
    // the step is no longer waiting on anything an admin can answer
    pending.steps[index].blocked = None;
    // stop the quiesced components, which is nothing to do once they are at zero
    let names = quiesce
        .paused
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    pause::scale_down(meta, &names).await?;
    // hold through the grace, looking again when it ends
    let left = grace_left(&quiesce, approval);
    if left > 0 {
        pending.requeue_hint = Some(left);
        return Ok(Advance::Held {
            state: StepState::Waiting,
            message: format!(
                "Stopped {} for a grace of {} seconds before step {} cancels what is in flight; \
                 {left} seconds left",
                names.join(", "),
                grace_secs(approval),
                step.id
            ),
        });
    }
    // then run, whatever the check reports
    let migration = step.thoradm_migration.unwrap_or(step.id);
    let approved = Approved {
        backup: &approval.backup,
        accept,
        confirm: None,
        loss: None,
    };
    migration::start(meta, pending, index, step, migration, approved, attempt).await
}

/// Describe what a step that cancels on approval did before its run, for the message of the entry
/// that records the run finished
///
/// # Arguments
///
/// * `state` - The recorded upgrade state, which still holds the run open
/// * `step` - The step
#[must_use]
pub(super) fn finished_note(state: &UpgradeState, step: &StepDef) -> String {
    // the quiesce the run took over names what stopped first, and when
    held(state, step.id).map_or_else(String::new, |quiesce| {
        let names = quiesce.paused.keys().cloned().collect::<Vec<_>>();
        format!(
            "; it ran after stopping {} at {}",
            if names.is_empty() {
                "nothing".to_owned()
            } else {
                names.join(", ")
            },
            quiesce.at.as_deref().unwrap_or("an unknown time")
        )
    })
}

/// End each open quiesce whose step is no longer the next one with a usable approval, before its
/// run starts: scale the quiesced components back to their recorded counts and record the restore
///
/// A quiesce stays while its step leads the plan and an approval still repeats its code. Clearing
/// or lowering the target, stopping the attempt, or removing or changing the approval ends it.
///
/// # Arguments
///
/// * `client` - The kube client to use
/// * `namespace` - The namespace the components and the state live in
/// * `stored` - The recorded upgrade state, which is saved with each restore
/// * `planned` - The target and the planned steps when the cluster is being upgraded
/// * `approvals` - The approvals set on the cluster
pub(super) async fn settle(
    client: &Client,
    namespace: &str,
    stored: &mut StoredState,
    planned: Option<(&RevisionId, &[PlannedStep])>,
    approvals: &[UpgradeApproval],
) -> Result<(), Error> {
    // the steps an open quiesce was recorded for
    let steps = stored
        .state
        .applied
        .iter()
        .filter(|entry| entry.step == QUIESCE_STEP)
        .filter_map(|entry| entry.chain.first().cloned())
        .collect::<BTreeSet<_>>();
    for id in steps {
        let Some(quiesce) = open(&stored.state, &id).cloned() else {
            continue;
        };
        // a quiesce stays while its step leads the plan with an approval that repeats its code
        let kept = planned.is_some_and(|(target, steps)| {
            steps.first().is_some_and(|first| first.step == id)
                && catalog().step(&id).is_some_and(|(_, step)| {
                    usable(&stored.state, step, target, approvals).is_some()
                })
        });
        if kept {
            continue;
        }
        // otherwise its components go back to their counts, each restore recorded
        if quiesce.paused.is_empty() {
            let revision = stored.state.revision.to_string();
            stored.state.record(AppliedStep {
                revision,
                step: RESTORE_STEP.to_owned(),
                outcome: StepState::Done,
                message: Some(format!(
                    "Ended the quiesce of step {id}, which stopped no deployment"
                )),
                at: Some(Utc::now().to_rfc3339()),
                by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
                ..AppliedStep::default()
            });
            state::save(client, namespace, stored).await?;
        } else {
            pause::restore_counts(client, namespace, stored, &quiesce.paused).await?;
        }
    }
    Ok(())
}

/// List the approvals of the planned steps that cancel on approval which don't count, each with the
/// step's current code
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `target` - The revision the upgrade moves to
/// * `approvals` - The approvals set on the cluster
/// * `steps` - The steps left in the plan
#[must_use]
pub(super) fn stale(
    state: &UpgradeState,
    target: &RevisionId,
    approvals: &[UpgradeApproval],
    steps: &[PlannedStep],
) -> Vec<String> {
    // each planned step that cancels on approval and has an approval that doesn't count
    let catalog = catalog();
    steps
        .iter()
        .filter_map(|planned| catalog.step(&planned.step).map(|(_, step)| step))
        .filter(|step| step.cancels_on_approval)
        .filter(|step| approvals.iter().any(|approval| approval.step == step.id))
        .filter_map(|step| {
            let why = match judge(state, step, target, approvals) {
                Ok(_) => return None,
                Err(Some(ConfirmRefusal::Consumed)) => {
                    "its confirmation code was consumed by an earlier quiesce"
                }
                Err(Some(ConfirmRefusal::Missing)) => "it repeats no confirmation code",
                Err(Some(_)) => "its confirmation code doesn't match",
                Err(None) => {
                    "it needs a backup, cancelInFlight: true (cancel_in_flight in the \
                     ThoriumCluster), and the confirmation code"
                }
            };
            Some(format!(
                "{}: the approval doesn't match: {why}; the current code is {}",
                step.id,
                code(state, step, target)
            ))
        })
        .collect()
}
