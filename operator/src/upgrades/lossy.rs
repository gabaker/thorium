//! Lossy downgrade steps: the loss their check estimates, the confirmation code they show when they
//! block, the approval gate, and what becomes of a run that refuses its code
//!
//! A step reversing a `LossyRevert` step is planned with its loss report, which names the fields it
//! fills. Its check Job estimates how many objects hold each field and whether `thoradm backup`
//! covers it. Outside a dev cluster's blanket setting the step then blocks, once, with a code
//! recorded on a `Blocked` entry of the history; only an approval repeating that code, with a backup
//! and `accept_loss`, lets it run. Starting the run records the code on the step's `Running` entry,
//! which consumes it, and thoradm writes a `consumed_code` row that outlives the state's history.

use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use thorium::Error;
use thorium::models::schema::CheckReport;
use thorium::models::upgrades::{
    AppliedStep, BlockReason, ConfirmRefusal, LossGate, LossVerdict, LossySetting, StepDef,
    StepLoss, StepState, UpgradeApproval, UpgradeConditionType, Work, confirmation_code,
    consumed_codes, recorded_code,
};

use super::migration::{
    self, Advance, Approved, Check, annotation, approval_hash, effective_accepted, ensure_check,
    failed, open_run, start, step_revision, work,
};
use super::record;
use super::state;
use super::{OPERATOR_VERSION, Pending, catalog};
use crate::k8s::clusters::{ClusterMeta, cluster_name_and_namespace};
use crate::k8s::crds::{self, StatusTracker, ThoriumCluster};
use crate::k8s::jobs::{self, JobPhase};
use crate::k8s::namespaces::is_dev_cluster;

/// Get the approval a lossy step is judged by: the first one set for it, whether or not it names a
/// backup, so a refusal can say what is missing
///
/// # Arguments
///
/// * `step` - The step
/// * `approvals` - The approvals set on the cluster
fn given<'a>(step: &StepDef, approvals: &'a [UpgradeApproval]) -> Option<&'a UpgradeApproval> {
    // the first approval naming the step
    approvals.iter().find(|approval| approval.step == step.id)
}

/// Decide what `dangerous_downgrade_lossy` comes to on this cluster
///
/// The Namespace is read only when the setting is set, since only a dev cluster honors it.
///
/// # Arguments
///
/// * `client` - The kube client to read the Namespace with
/// * `cluster` - The cluster, for its namespace and upgrade settings
pub(super) async fn setting(
    client: &Client,
    cluster: &ThoriumCluster,
) -> Result<LossySetting, Error> {
    // an unset setting needs no Namespace read
    let upgrade = &cluster.spec.upgrade;
    if !upgrade.dangerous_downgrade_lossy {
        return Ok(LossySetting::Off);
    }
    // the cluster must be a dev cluster to honor it
    let (_, namespace) = cluster_name_and_namespace(cluster)?;
    let dev = is_dev_cluster(client, &namespace, upgrade).await?;
    Ok(LossySetting::of(true, dev))
}

/// Raise or clear the `LossySettingRefused` condition for the cluster's setting
///
/// A Namespace that can't be read is logged and leaves the condition alone, so a failed read never
/// changes what an admin sees.
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster and the status last written for it
pub(super) async fn report(client: &Client, tracker: &StatusTracker) {
    // work out what the setting comes to
    let found = match setting(client, tracker.cluster()).await {
        Ok(found) => found,
        Err(error) => {
            println!("Failed to read the dev label of the Namespace: {error}");
            return;
        }
    };
    // name the setting and how to clear it while it is refused
    let refused = found == LossySetting::Refused;
    crds::set_upgrade_condition(
        client,
        tracker,
        UpgradeConditionType::LossySettingRefused,
        refused,
        if refused {
            "spec.upgrade.dangerous_downgrade_lossy (Helm value \
             operator.cluster.upgrade.dangerousDowngradeLossy) is set, but this cluster isn't a dev \
             cluster (autoTargetDev with the thorium.sandia.gov/dev=true label on the Namespace), so \
             the setting is ignored and every lossy step needs its own approval with its \
             confirmation code; to clear this, unset the setting or label the Namespace as a cluster \
             admin"
                .to_owned()
        } else {
            "dangerous_downgrade_lossy is unset or honored on a dev cluster".to_owned()
        },
    )
    .await;
}

/// Whether a lossy step's running Job is still approved: the dev setting is honored, or an
/// approval that accepts the loss is still set
///
/// The code was consumed when the run started, so the approval isn't judged again; it only has to
/// still be there.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `step` - The step
/// * `approvals` - The approvals set on the cluster
pub(super) async fn approved(
    meta: &ClusterMeta,
    step: &StepDef,
    approvals: &[UpgradeApproval],
) -> Result<bool, Error> {
    // a blanket setting or an approval that accepts the loss
    let honored = setting(&meta.client, &meta.cluster).await? == LossySetting::Honored;
    Ok(honored || given(step, approvals).is_some_and(|approval| approval.accept_loss))
}

/// List the ids of the steps that run before a step on the upgrade's path
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `step` - The step
fn preceding(pending: &Pending, step: &StepDef) -> Vec<&'static str> {
    // the steps of every edge on the path, in order, up to this one
    let catalog = catalog();
    let Ok(path) = catalog.path(&pending.stored.state.revision, &pending.target) else {
        return Vec::new();
    };
    path.iter()
        .flat_map(|edge| edge.steps.iter())
        .map(|candidate| candidate.id)
        .take_while(|id| *id != step.id)
        .collect()
}

/// Get the code a blocked lossy step shows, computing and recording one when the step blocks anew
///
/// The code is computed once, from the history's length at that moment, and recorded as `confirm`
/// on a `Blocked` entry the operator compares approvals with, so entries written after it don't
/// change what an admin was shown.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state records the block
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `loss` - What the step loses
async fn code(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    loss: &StepLoss,
) -> Result<String, Error> {
    // a block that already recorded a code still holds it
    let before = preceding(pending, step);
    if let Some(code) = recorded_code(&pending.stored.state, step.id, &before) {
        return Ok(code);
    }
    // otherwise this is a new block: compute the code and record it
    let state = &pending.stored.state;
    let fresh = confirmation_code(
        step.id,
        state.revision.as_str(),
        loss,
        record::history_len(state),
    );
    let entry = AppliedStep {
        revision: step_revision(pending, index),
        step: step.id.to_owned(),
        outcome: StepState::Blocked,
        message: Some(format!(
            "Step {} loses data and waits for its loss to be accepted with its confirmation code",
            step.id
        )),
        at: Some(migration::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        confirm: Some(fresh.clone()),
        loss: Some(loss.clone()),
        ..AppliedStep::default()
    };
    pending.stored.state.record(entry);
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    Ok(fresh)
}

/// Get what a step loses as its check estimated it, or as the plan names it before one ran
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `report` - The step's check report, when it was checked
fn estimate(pending: &Pending, index: usize, report: Option<&CheckReport>) -> StepLoss {
    // the check's counts win over the plan's names
    let planned = pending.steps[index].loss.clone();
    report
        .and_then(|report| report.loss.clone())
        .or(planned)
        .unwrap_or_else(|| StepLoss {
            revision: pending.steps[index].revision.clone(),
            step: pending.steps[index].step.clone(),
            description: pending.steps[index].description.clone(),
            fields: Vec::new(),
        })
}

/// Hold a lossy step on what the gate says
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `loss` - What the step loses
/// * `reason` - What the step waits for
/// * `message` - What an admin should do
fn hold(
    pending: &mut Pending,
    index: usize,
    loss: &StepLoss,
    reason: BlockReason,
    message: String,
) -> Advance {
    // the status shows the reason, the loss with its counts, and the message
    pending.steps[index].blocked = Some(reason);
    pending.steps[index].loss = Some(loss.clone());
    Advance::Held {
        state: StepState::Blocked,
        message,
    }
}

/// Add how to take the backup to a block's message
///
/// # Arguments
///
/// * `message` - The gate's message
fn with_backup_advice(message: &str) -> String {
    // the same advice a non-lossy approval gets
    format!(
        "{message}. Take the backup with `thoradm backup new` and read {} first",
        migration::BACKUP_DOCS
    )
}

/// Check a lossy step that hasn't started
///
/// Returns the check's report, or, as the error, what to do instead: hold the step while the check
/// runs, waits, or fails, or skip it when it has nothing to change.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `step` - The step
/// * `migration` - The migration it runs
/// * `approved` - Whether the step has an approval
async fn checked(
    meta: &ClusterMeta,
    pending: &mut Pending,
    step: &'static StepDef,
    migration: &str,
    approved: bool,
) -> Result<Result<Box<CheckReport>, Advance>, Error> {
    // ask for the step's check, which may still be running
    let report = match ensure_check(meta, pending, step, migration, approved).await? {
        Check::Ready(report) => report,
        Check::Wait(message) => {
            return Ok(Err(Advance::Held {
                state: StepState::Waiting,
                message,
            }));
        }
        Check::Failed(message) => {
            return Ok(Err(Advance::Held {
                state: StepState::Blocked,
                message,
            }));
        }
    };
    // a check that asks to wait holds the step, and one that finds nothing to change skips it
    if let Some(wait) = &report.wait {
        return Ok(Err(Advance::Held {
            state: StepState::Waiting,
            message: wait.reason.clone(),
        }));
    }
    if let Work::Nothing(reason) = work(step, &report) {
        return Ok(Err(Advance::Skipped(reason)));
    }
    Ok(Ok(report))
}

/// Advance a lossy step one move: check it, gate it on its confirmation code, and run it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `migration` - The migration it runs
/// * `attempt` - The attempt counter
/// * `current` - The Job of the run that started and hasn't ended, if one exists
/// * `approvals` - The approvals set on the cluster
// the Job a run started, its approvals, and the step's place in the plan are all needed to decide
#[allow(clippy::too_many_arguments)]
pub(super) async fn advance(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    migration: &str,
    attempt: usize,
    current: Option<&Job>,
    approvals: &[UpgradeApproval],
) -> Result<Advance, Error> {
    let approval = given(step, approvals);
    let open = open_run(&pending.stored.state, step.id).cloned();
    // a step that hasn't started is checked first, for its loss and for work to do
    let mut report = None;
    if open.is_none() {
        match checked(meta, pending, step, migration, approval.is_some()).await? {
            Ok(found) => report = Some(*found),
            Err(held) => return Ok(held),
        }
    }
    // what the step loses: the check's estimate, or what the run that started recorded
    let mut loss = estimate(pending, index, report.as_ref());
    if let Some(recorded) = open.as_ref().and_then(|entry| entry.loss.clone()) {
        loss = recorded;
    }
    // a Job that failed under this very approval holds the step until it is approved anew
    if let Some(job) = current
        && matches!(jobs::phase(job), JobPhase::Failed { .. })
        && let Some(approval) = approval
        && annotation(job, migration::CONFIRM_ANNOTATION).filter(|code| !code.is_empty())
            == approval.confirm.as_deref()
        && annotation(job, migration::APPROVAL_ANNOTATION)
            == Some(
                approval_hash(
                    &approval.backup,
                    &effective_accepted(&pending.stored.state, step.id, approval),
                    approval.confirm.as_deref(),
                )
                .as_str(),
            )
    {
        return retry(meta, pending, index, step, job, &loss, approval).await;
    }
    // the dev setting waives the gate; otherwise the code and the approval decide
    let setting = setting(&meta.client, &meta.cluster).await?;
    if setting == LossySetting::Honored {
        let accept = approval.map_or_else(Vec::new, |approval| {
            effective_accepted(&pending.stored.state, step.id, approval)
        });
        let backup = approval.map_or("", |approval| approval.backup.as_str());
        // a Job the blanket setting ran that failed holds the step until an approval changes it
        if let Some(job) = current
            && matches!(jobs::phase(job), JobPhase::Failed { .. })
            && annotation(job, migration::APPROVAL_ANNOTATION)
                == Some(approval_hash(backup, &accept, None).as_str())
        {
            return Ok(failed(meta, pending, index, step, job, &accept, true)
                .await?
                .advance);
        }
        let approved = Approved {
            backup,
            accept: &accept,
            confirm: None,
            loss: Some(&loss),
        };
        return start(meta, pending, index, step, migration, approved, attempt).await;
    }
    // a run that started and hasn't created its Job yet, because its components' pods or another
    // writing Job were still running, goes on with the approval it started under: its code is
    // already consumed by its own entry, so judging it again would refuse it; one whose approval
    // is gone falls through to the gate
    if let Some(open) = open.as_ref().filter(|_| current.is_none())
        && let Some(approval) = approval.filter(|approval| approval.accept_loss)
    {
        let accept = effective_accepted(&pending.stored.state, step.id, approval);
        let approved = Approved {
            backup: open.backup.as_deref().unwrap_or_default(),
            accept: &accept,
            confirm: open.confirm.as_deref(),
            loss: Some(&loss),
        };
        return start(meta, pending, index, step, migration, approved, attempt).await;
    }
    let shown = code(meta, pending, index, step, &loss).await?;
    let consumed = consumed_codes(&pending.stored.state, step.id);
    let verdict = LossGate {
        setting,
        code: &shown,
        loss: &loss,
        approval,
        consumed: &consumed,
    }
    .judge();
    match verdict {
        LossVerdict::Blocked { reason, message } => Ok(hold(
            pending,
            index,
            &loss,
            reason,
            with_backup_advice(&message),
        )),
        LossVerdict::Run { .. } => {
            // an approval that passed the gate names the backup and repeats the code
            let approval = approval.cloned().unwrap_or_default();
            let accept = effective_accepted(&pending.stored.state, step.id, &approval);
            let approved = Approved {
                backup: &approval.backup,
                accept: &accept,
                confirm: Some(&shown),
                loss: Some(&loss),
            };
            start(meta, pending, index, step, migration, approved, attempt).await
        }
    }
}

/// Hold a lossy step whose Job failed under its approval, and show the new code a retry needs
///
/// The failed run consumed its approval's code, so running the step again needs a new approval
/// repeating the code the status shows now.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `job` - The failed Job
/// * `loss` - What the step loses
/// * `approval` - The approval the Job ran under
async fn retry(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    job: &Job,
    loss: &StepLoss,
    approval: &UpgradeApproval,
) -> Result<Advance, Error> {
    // report the failure the way every migration's is
    let accept = effective_accepted(&pending.stored.state, step.id, approval);
    let failure = failed(meta, pending, index, step, job, &accept, true).await?;
    let Advance::Held { state, message } = failure.advance else {
        return Ok(failure.advance);
    };
    // the run's code is used up, so the step blocks again with a new one
    let shown = code(meta, pending, index, step, loss).await?;
    let (reason, advice) = if failure.consumed {
        (
            BlockReason::ConfirmationRefused {
                confirm: shown.clone(),
                refusal: ConfirmRefusal::Consumed,
            },
            format!(
                "Its confirmation code {} was already consumed by an earlier run of the step, so \
                 the Job refused to run",
                approval.confirm.as_deref().unwrap_or_default()
            ),
        )
    } else {
        (
            BlockReason::LossAcceptance {
                confirm: shown.clone(),
            },
            "Its confirmation code was used up when the run started".to_owned(),
        )
    };
    pending.steps[index].blocked = Some(reason);
    pending.steps[index].loss = Some(loss.clone());
    Ok(Advance::Held {
        state,
        message: format!(
            "{message}. {advice}; to run the step again, approve it again repeating the new \
             confirmation code {shown} as confirm"
        ),
    })
}
