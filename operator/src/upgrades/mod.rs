//! Moves a `ThoriumCluster` between revisions before it is reconciled
//!
//! Every Thorium namespace records the revision it is at in the `thorium-upgrade-state`
//! `ConfigMap`. A cluster behind this operator's latest revision is left untouched in the
//! `UpgradeRequired` phase until a target revision is set, and is then brought to it one step
//! at a time, recording each step so an interrupted upgrade resumes where it stopped. Only a
//! cluster at the latest revision is reconciled normally.
//!
//! A namespace deployed by the minithor or megathor scripts before the Helm charts must be
//! converted by `deploy/charts/scripts/convert-to-helm.sh` first, which records its starting
//! revision. The operator refuses to touch an unconverted one.

mod detect;
mod state;
mod steps;

use kube::Client;
use kube::runtime::controller::Action;
use std::time::Duration;
use thorium::Error;
use thorium::models::upgrades::{
    self, AppliedStep, PlannedStep, RevisionId, StepState, UpgradeError, UpgradeState,
    UpgradeStatus,
};

use crate::k8s::clusters::{ClusterMeta, cluster_name_and_namespace};
use crate::k8s::crds::{
    self, ClusterPhase, StatusTracker, ThoriumCluster, ThoriumClusterStatus, UpgradeSpec,
};
pub use detect::ALLOW_INLINE_CONFIG;
use detect::Detected;
use state::StoredState;

/// The Thorium version of this operator, recorded with every revision it applies
const OPERATOR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The revision of a deployment from before the Helm charts, recorded for pre-Helm clusters
/// that have no upgrade state
const BASELINE_REVISION: &str = "2026-10-v01";

/// The revision deployed by the first Helm charts, recorded for Helm clusters created before
/// upgrade states were
const FIRST_HELM_REVISION: &str = "2026-10-v02";

/// How long in seconds to wait before rechecking a cluster held for an admin's decision; a
/// spec change (like setting a target revision) reconciles it right away
const HOLD_REQUEUE_SECS: u64 = 3600;

/// How long in seconds to wait before rechecking an unconverted pre-Helm cluster
const LEGACY_REQUEUE_SECS: u64 = 600;

/// How long in seconds to wait before rechecking a step blocked on manual work, which the
/// operator only notices by checking again
const BLOCKED_REQUEUE_SECS: u64 = 60;

/// How long in seconds to wait before rechecking a step waiting on a backend
const WAITING_REQUEUE_SECS: u64 = 15;

/// What to do with a `ThoriumCluster` before reconciling it
pub enum Gate {
    /// The cluster is at the operator's latest revision, so reconcile it normally
    Current,
    /// The cluster must be upgraded first
    Upgrade(Pending),
    /// Leave the cluster alone and check it again later
    Hold(Action),
}

/// An upgrade waiting to run once the cluster's config is resolved
pub struct Pending {
    /// The recorded upgrade state
    stored: StoredState,
    /// The revision to upgrade to
    target: RevisionId,
    /// The steps that bring the cluster to the target revision
    steps: Vec<PlannedStep>,
}

impl Pending {
    /// Build the upgrade progress to report while this upgrade runs
    fn progress(&self) -> UpgradeStatus {
        // report the recorded revision, the target, and every planned step
        progress(
            &self.stored.state.revision,
            Some(&self.target),
            self.steps.clone(),
        )
    }
}

/// Build the upgrade progress reported in a cluster's status
///
/// # Arguments
///
/// * `current` - The revision the cluster is at
/// * `target` - The revision the cluster is upgrading to, if any
/// * `steps` - The steps left to run
fn progress(
    current: &RevisionId,
    target: Option<&RevisionId>,
    steps: Vec<PlannedStep>,
) -> UpgradeStatus {
    // name the current, target, and latest revisions alongside the steps left
    UpgradeStatus {
        current: Some(current.to_string()),
        target: target.map(ToString::to_string),
        latest: Some(upgrades::latest().to_string()),
        steps,
    }
}

/// Hold a cluster in the error phase with a message an admin must act on
///
/// # Arguments
///
/// * `client` - The kube client to update the status with
/// * `tracker` - The cluster to hold and the status last written for it
/// * `message` - What is wrong and how to fix it
/// * `requeue_secs` - How long to wait before checking again
async fn hold_error(
    client: &Client,
    tracker: &StatusTracker,
    message: String,
    requeue_secs: u64,
) -> Gate {
    // log the problem and record it in the cluster's status
    eprintln!("Error: {message}");
    crds::set_status(client, tracker, ClusterPhase::Error, Some(message)).await;
    Gate::Hold(Action::requeue(Duration::from_secs(requeue_secs)))
}

/// Describe an unconverted pre-Helm deployment and how to convert it
///
/// # Arguments
///
/// * `signals` - What identified the deployment as pre-Helm
fn legacy_message(signals: &[String]) -> String {
    format!(
        "This ThoriumCluster was deployed by the minithor or megathor scripts from before the \
         Helm charts and has not been converted ({}). The operator changed nothing. Stop this \
         operator, convert the deployment with the convert-to-helm.sh script from the Thorium \
         Helm charts (see \"Converting a Pre-Helm Deployment\" in the Thorium docs), and then \
         deploy the Helm chart again. A ThoriumCluster that keeps its credentials inline on purpose can set \
         the {ALLOW_INLINE_CONFIG}=true annotation instead",
        signals.join("; ")
    )
}

/// Build the state a namespace without a recorded upgrade state starts from
///
/// Returns the state along with why it starts at its revision.
///
/// # Arguments
///
/// * `detected` - What the namespace holds
/// * `at` - When the starting revision is recorded (RFC3339)
fn starting_state(detected: Detected, at: String) -> Result<(UpgradeState, &'static str), Error> {
    // parse a revision id this build ships with
    let known = |raw: &str| {
        raw.parse::<RevisionId>()
            .map_err(|error| Error::new(error.to_string()))
    };
    // pick the revision this namespace starts at
    let (revision, message) = match detected {
        Detected::Fresh => (upgrades::latest(), "fresh install"),
        Detected::BeforeTracking => (
            known(FIRST_HELM_REVISION)?,
            "Helm deployment from before upgrade states were recorded",
        ),
        Detected::PreHelm(reason) => (known(BASELINE_REVISION)?, reason),
    };
    // record where this namespace starts
    let mut state = UpgradeState::new(revision.clone(), OPERATOR_VERSION);
    state.record(AppliedStep {
        revision: revision.to_string(),
        step: "record-revision".to_owned(),
        outcome: StepState::Done,
        message: Some(message.to_owned()),
        at: Some(at),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
    });
    Ok((state, message))
}

/// Record the starting revision of a namespace without an upgrade state
///
/// # Arguments
///
/// * `client` - The kube client to use
/// * `namespace` - The namespace of the cluster
/// * `cluster` - The cluster being reconciled
async fn seed(
    client: &Client,
    namespace: &str,
    cluster: &ThoriumCluster,
) -> Result<StoredState, Error> {
    // work out what this namespace holds and where it starts
    let detected = detect::detect(client, namespace, cluster).await?;
    let (state, message) = starting_state(detected, chrono::Utc::now().to_rfc3339())?;
    // log where this namespace starts
    println!(
        "Recording revision {} for {namespace}: {message}",
        state.revision
    );
    // the recorded Thorium version is advisory, so a mismatch is only a warning
    if let Some(warning) = state.version_mismatch() {
        println!("Warning: {warning}");
    }
    // save the new state
    let mut stored = StoredState {
        state,
        resource_version: None,
    };
    state::save(client, namespace, &mut stored).await?;
    Ok(stored)
}

/// What to do with a cluster given its recorded upgrade state and upgrade spec
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// The cluster is at the latest revision, so report this progress and reconcile it
    Current(UpgradeStatus),
    /// Something is wrong that an admin must fix, so hold the cluster in the error phase
    Error(String),
    /// The cluster is behind and has no target past its revision, so hold it until one is set
    Required {
        /// Why the cluster is held and how to continue
        message: String,
        /// The upgrade progress to report
        progress: UpgradeStatus,
    },
    /// The cluster must be brought to a target revision first
    Upgrade {
        /// The revision to upgrade to
        target: RevisionId,
        /// The steps that bring the cluster to the target revision
        steps: Vec<PlannedStep>,
    },
}

/// Decide what to do with a cluster from its recorded upgrade state and upgrade spec
///
/// # Arguments
///
/// * `state` - The cluster's recorded upgrade state
/// * `spec` - The cluster's upgrade settings
fn decide(state: &UpgradeState, spec: &UpgradeSpec) -> Decision {
    // remember the revision the cluster is at
    let current = state.revision.clone();
    // resolve the target and the steps left to the latest revision
    let latest = upgrades::latest();
    let planned = upgrades::resolve_target(spec.target_revision.as_deref(), spec.auto_target_dev)
        .and_then(|target| Ok((target, upgrades::plan(state, &latest)?)));
    let (target, remaining) = match planned {
        Ok(planned) => planned,
        Err(error) => return Decision::Error(error.to_string()),
    };
    // a target older than the cluster's revision is always a mistake
    if let Some(target) = target.as_ref().filter(|target| **target < current) {
        let error = UpgradeError::Backwards {
            current: current.clone(),
            target: target.clone(),
        };
        return Decision::Error(error.to_string());
    }
    // a cluster at the latest revision is reconciled normally
    if current == latest {
        return Decision::Current(progress(&current, target.as_ref(), Vec::new()));
    }
    match target {
        // without a target nothing changes until an admin picks one
        None => {
            let message = format!(
                "This cluster is at revision {current} and this operator deploys revision \
                 {latest}. Nothing is changed until spec.upgrade.target_revision is set to \
                 {latest} (Helm value operator.cluster.upgrade.targetRevision); the steps that \
                 will run are listed in status.upgrade.steps"
            );
            println!("{message}");
            Decision::Required {
                message,
                progress: progress(&current, None, remaining),
            }
        }
        // a cluster at an older target still can't run this operator's components
        Some(target) if target == current => Decision::Required {
            message: format!(
                "This cluster is at its target revision {target}, but this operator deploys \
                 revision {latest}; set spec.upgrade.target_revision to {latest} to continue"
            ),
            progress: progress(&current, Some(&target), remaining),
        },
        // plan the steps to the target
        Some(target) => match upgrades::plan(state, &target) {
            Ok(steps) => Decision::Upgrade { target, steps },
            Err(error) => Decision::Error(error.to_string()),
        },
    }
}

/// Decide what to do with a `ThoriumCluster` before reconciling it
///
/// This only needs the cluster and its namespace, not its resolved config, so a cluster whose
/// config can't be resolved is still held when it is behind or unconverted.
///
/// # Arguments
///
/// * `client` - The kube client to use
/// * `tracker` - The cluster being reconciled and the status last written for it
pub async fn gate(client: &Client, tracker: &StatusTracker) -> Result<Gate, Error> {
    // get the cluster snapshot this reconcile works from
    let cluster = tracker.cluster();
    // get the namespace this cluster's state lives in
    let (_, namespace) = cluster_name_and_namespace(cluster)?;
    // never touch an unconverted pre-Helm cluster, whatever state is recorded
    if let Some(signals) = detect::legacy_signals(cluster) {
        let message = legacy_message(&signals);
        return Ok(hold_error(client, tracker, message, LEGACY_REQUEUE_SECS).await);
    }
    // load the recorded state or work out where this namespace starts
    let stored = match state::load(client, &namespace).await? {
        Some(stored) => stored,
        None => seed(client, &namespace, cluster).await?,
    };
    // act on what the recorded state and upgrade spec call for
    match decide(&stored.state, &cluster.spec.upgrade) {
        Decision::Current(progress) => {
            crds::set_upgrade_progress(client, tracker, progress).await;
            Ok(Gate::Current)
        }
        Decision::Error(message) => {
            Ok(hold_error(client, tracker, message, HOLD_REQUEUE_SECS).await)
        }
        Decision::Required { message, progress } => {
            crds::set_upgrade_status(
                client,
                tracker,
                ClusterPhase::UpgradeRequired,
                Some(message),
                progress,
            )
            .await;
            Ok(Gate::Hold(Action::requeue(Duration::from_secs(
                HOLD_REQUEUE_SECS,
            ))))
        }
        Decision::Upgrade { target, steps } => Ok(Gate::Upgrade(Pending {
            stored,
            target,
            steps,
        })),
    }
}

/// Check whether a cluster's status already reports a step as blocked
///
/// # Arguments
///
/// * `status` - The status last written for the cluster being upgraded
/// * `id` - The id of the step
fn reported_blocked(status: &ThoriumClusterStatus, id: &str) -> bool {
    // look for the step among the reported upgrade steps
    status.upgrade.as_ref().is_some_and(|upgrade| {
        upgrade
            .steps
            .iter()
            .any(|step| step.step == id && step.state == StepState::Blocked)
    })
}

/// Check whether a step's upgrade history records it as blocked
///
/// A step that is still planned hasn't finished, so any blocked entry for it comes from an
/// earlier attempt at this same upgrade.
///
/// # Arguments
///
/// * `state` - The cluster's recorded upgrade state
/// * `revision` - The revision the step belongs to
/// * `id` - The id of the step
fn recorded_blocked(state: &UpgradeState, revision: &str, id: &str) -> bool {
    // look for a blocked entry for this step in the history
    state.applied.iter().any(|applied| {
        applied.revision == revision && applied.step == id && applied.outcome == StepState::Blocked
    })
}

/// Check whether a step blocked on an admin earlier in this upgrade
///
/// The upgrade history in the state `ConfigMap` is the main record: it survives the
/// `ThoriumCluster` being recreated and status rewrites that replace the reported steps (such
/// as holding the cluster when its target is cleared). The reported status covers a block the
/// history no longer holds, such as one dropped by the history cap.
///
/// # Arguments
///
/// * `status` - The status last written for the cluster being upgraded
/// * `state` - The cluster's recorded upgrade state
/// * `step` - The planned step
fn previously_blocked(
    status: &ThoriumClusterStatus,
    state: &UpgradeState,
    step: &PlannedStep,
) -> bool {
    // check the persisted history first, then the reported status
    recorded_blocked(state, &step.revision, &step.step) || reported_blocked(status, &step.step)
}

/// Turn the result of a step that blocked earlier and now has nothing to do into a done step
///
/// A step that blocked on an admin and later finds nothing to do was fixed by hand, so it is
/// recorded as done rather than skipped; a step with nothing to do from the start stays
/// skipped.
///
/// # Arguments
///
/// * `result` - What running the step returned
/// * `blocked_before` - Whether the step blocked earlier in this upgrade
fn credit_manual_fix(result: steps::StepResult, blocked_before: bool) -> steps::StepResult {
    // only a skip after an earlier block counts as a manual fix
    match result {
        steps::StepResult::Skipped(details) if blocked_before => {
            steps::StepResult::Done(Some(format!("fixed manually: {details}")))
        }
        other => other,
    }
}

/// Describe a held step and pick how long in seconds to wait before checking it again
///
/// # Arguments
///
/// * `state` - The state reported for the held step
fn hold_timing(state: StepState) -> (&'static str, u64) {
    // blocked steps need an admin, so they're checked less often than waits on a backend
    if state == StepState::Blocked {
        ("is blocked", BLOCKED_REQUEUE_SECS)
    } else {
        ("is waiting", WAITING_REQUEUE_SECS)
    }
}

/// Run one step of an upgrade and record it once it finishes
///
/// Returns the action to take when the step is blocked or waiting and must be checked again.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state and steps are updated
/// * `index` - The index of the step to run
/// * `start` - The revision the upgrade started from
async fn run_step(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    start: &RevisionId,
) -> Result<Option<Action>, Error> {
    // find this step in the catalog
    let id = pending.steps[index].step.clone();
    let (_, def) = upgrades::step(&id)
        .ok_or_else(|| Error::new(format!("Upgrade step {id} is not in the catalog")))?;
    // a step already reported as blocked keeps that status while it is checked again, so the
    // status doesn't flap between running and blocked every recheck
    let reported = meta.status.current();
    let already_blocked = reported_blocked(&reported, &id);
    // a step that blocked earlier and now has nothing to do was fixed by hand
    let blocked_before =
        previously_blocked(&reported, &pending.stored.state, &pending.steps[index]);
    // report the step we are running
    pending.steps[index].state = StepState::Running;
    pending.steps[index].message = None;
    if !already_blocked {
        let message = format!(
            "Upgrading from revision {start} to {}: running step {id}",
            pending.target
        );
        println!("{message}");
        crds::set_upgrade_status(
            &meta.client,
            &meta.status,
            ClusterPhase::Upgrading,
            Some(message),
            pending.progress(),
        )
        .await;
    }
    // run the step, recording a failure in the progress before returning it
    let result = match steps::execute(meta, def, &meta.cluster.spec.upgrade.approvals).await {
        Ok(result) => result,
        Err(error) => {
            pending.steps[index].state = StepState::Failed;
            let message = crds::error_message(&error);
            pending.steps[index].message = Some(message.clone());
            crds::set_upgrade_progress(&meta.client, &meta.status, pending.progress()).await;
            return Err(Error::new(format!("Upgrade step {id} failed: {message}")));
        }
    };
    // split finished steps from ones that must be checked again later
    let (outcome, details) = match credit_manual_fix(result, blocked_before) {
        steps::StepResult::Done(details) => (StepState::Done, details),
        steps::StepResult::Skipped(details) => (StepState::Skipped, Some(details)),
        steps::StepResult::Blocked(details) => {
            // report the block, then remember it in the history so a later manual fix is
            // credited even if the reported status is lost
            let action = hold_step(meta, pending, index, StepState::Blocked, details).await;
            if !recorded_blocked(&pending.stored.state, &pending.steps[index].revision, &id) {
                record_block(meta, pending, index).await?;
            }
            return Ok(Some(action));
        }
        steps::StepResult::Waiting(details) => {
            return Ok(Some(
                hold_step(meta, pending, index, StepState::Running, details).await,
            ));
        }
    };
    // record the finished step and move past any revision it completed
    pending.steps[index].state = outcome;
    pending.steps[index].message.clone_from(&details);
    let revision = pending.steps[index].revision.clone();
    pending.stored.state.record(AppliedStep {
        revision,
        step: id,
        outcome,
        message: details,
        at: Some(chrono::Utc::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
    });
    pending
        .stored
        .state
        .advance(&pending.target, OPERATOR_VERSION);
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    Ok(None)
}

/// Record in the upgrade history that a step blocked on an admin
///
/// The entry doesn't finish the step, so the step is still planned and checked again; it only
/// lets a later run tell a manual fix apart from a step that never had anything to do.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state is saved
/// * `index` - The index of the blocked step, whose message is recorded
async fn record_block(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
) -> Result<(), Error> {
    // record the block with what the step is waiting on
    let step = &pending.steps[index];
    let applied = AppliedStep {
        revision: step.revision.clone(),
        step: step.step.clone(),
        outcome: StepState::Blocked,
        message: step.message.clone(),
        at: Some(chrono::Utc::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
    };
    pending.stored.state.record(applied);
    // save it at the version read, like a finished step
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await
}

/// Report a step that is blocked on an admin or waiting on a backend
///
/// Returns when to check the step again.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the held step
/// * `state` - The state to report for the step
/// * `details` - What the step is blocked or waiting on
async fn hold_step(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    state: StepState,
    details: String,
) -> Action {
    // describe the hold and pick when to check the step again
    let (verb, secs) = hold_timing(state);
    // describe why the step is held
    let message = format!(
        "The upgrade to {} {verb} at step {}: {details}",
        pending.target, pending.steps[index].step
    );
    // record it in the step's progress
    pending.steps[index].state = state;
    pending.steps[index].message = Some(details);
    println!("{message}");
    crds::set_upgrade_status(
        &meta.client,
        &meta.status,
        ClusterPhase::Upgrading,
        Some(message),
        pending.progress(),
    )
    .await;
    Action::requeue(Duration::from_secs(secs))
}

/// Describe a cluster that reached a target older than the latest revision
///
/// Returns nothing when the target is the latest revision, since the cluster is then current.
///
/// # Arguments
///
/// * `state` - The cluster's upgrade state after reaching its target
/// * `target` - The target revision the cluster reached
fn target_hold(
    state: &UpgradeState,
    target: &RevisionId,
) -> Result<Option<(String, UpgradeStatus)>, UpgradeError> {
    // a cluster at the latest revision is current
    let latest = upgrades::latest();
    if *target >= latest {
        return Ok(None);
    }
    // list the steps left to the latest revision
    let remaining = upgrades::plan(state, &latest)?;
    let message = format!(
        "This cluster reached its target revision {target}, but this operator deploys \
         revision {latest}; set spec.upgrade.target_revision to {latest} to continue"
    );
    Ok(Some((message, progress(target, Some(target), remaining))))
}

/// Run the steps that bring a cluster to its target revision
///
/// Returns the action to take when the cluster can't be reconciled normally yet: a step is
/// blocked or waiting, or the target is older than this operator's latest revision.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade to run
pub async fn run(meta: &ClusterMeta, mut pending: Pending) -> Result<Option<Action>, Error> {
    // remember where this upgrade started for its messages
    let start = pending.stored.state.revision.clone();
    // the recorded Thorium version is advisory, so a mismatch is only a warning
    if let Some(warning) = pending.stored.state.version_mismatch() {
        println!("Warning: {warning}");
    }
    // run each step in order, stopping at any that must be checked again later
    for index in 0..pending.steps.len() {
        if let Some(action) = run_step(meta, &mut pending, index, &start).await? {
            return Ok(Some(action));
        }
    }
    // move past any revisions without steps
    let target = pending.target.clone();
    if pending.stored.state.advance(&target, OPERATOR_VERSION) {
        state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    }
    println!("Upgraded {} from revision {start} to {target}", meta.name);
    // a target older than the latest revision still holds the cluster
    let held = target_hold(&pending.stored.state, &target)
        .map_err(|error| Error::new(error.to_string()))?;
    if let Some((message, progress)) = held {
        crds::set_upgrade_status(
            &meta.client,
            &meta.status,
            ClusterPhase::UpgradeRequired,
            Some(message),
            progress,
        )
        .await;
        return Ok(Some(Action::requeue(Duration::from_secs(
            HOLD_REQUEUE_SECS,
        ))));
    }
    // the cluster is current, so the normal reconcile deploys it
    crds::set_upgrade_status(
        &meta.client,
        &meta.status,
        ClusterPhase::Provisioning,
        Some(format!("Upgraded to revision {target}; deploying")),
        progress(&target, Some(&target), Vec::new()),
    )
    .await;
    Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, full_spec, meta_for, namespaced_cluster, pre_helm_spec,
    };
    use thorium::models::upgrades::{
        HASH_ANNOTATION, HASH_LABEL, REVISION_LABEL, STATE_CONFIG_MAP, STATE_KEY, VERSION_LABEL,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The path of the upgrade state `ConfigMap` in the test namespace
    pub(crate) const STATE_PATH: &str =
        "/api/v1/namespaces/thorium/configmaps/thorium-upgrade-state";

    /// The path `ConfigMaps` are created at in the test namespace
    pub(crate) const CONFIG_MAPS_PATH: &str = "/api/v1/namespaces/thorium/configmaps";

    /// The path of the operator's user Secret in the test namespace
    pub(crate) const OPERATOR_PASS_PATH: &str =
        "/api/v1/namespaces/thorium/secrets/thorium-operator-pass";

    /// The path of the test cluster's status subresource
    pub(crate) const STATUS_PATH: &str =
        "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium/status";

    /// Parse a revision id that must be valid
    ///
    /// # Arguments
    ///
    /// * `raw` - The id to parse
    fn rev(raw: &str) -> RevisionId {
        raw.parse().expect("valid revision")
    }

    /// Build an upgrade spec
    ///
    /// # Arguments
    ///
    /// * `target` - The target revision, if any
    /// * `auto_target_dev` - Whether to target the latest revision without a target
    fn spec(target: Option<&str>, auto_target_dev: bool) -> UpgradeSpec {
        UpgradeSpec {
            target_revision: target.map(str::to_owned),
            auto_target_dev,
            approvals: Vec::new(),
        }
    }

    /// Record steps of revision `2026-10-v02` as done in a state
    ///
    /// # Arguments
    ///
    /// * `state` - The state to record the steps in
    /// * `steps` - The ids of the finished steps
    fn finish(state: &mut UpgradeState, steps: &[&str]) {
        // record each step as done
        for step in steps {
            state.record(AppliedStep {
                revision: FIRST_HELM_REVISION.to_owned(),
                step: (*step).to_owned(),
                outcome: StepState::Done,
                message: None,
                at: None,
                by: None,
            });
        }
    }

    /// Build the upgrade state `ConfigMap` the kube API returns for a state
    ///
    /// # Arguments
    ///
    /// * `state` - The state the `ConfigMap` holds
    pub(crate) fn state_config_map(state: &UpgradeState) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": STATE_CONFIG_MAP, "namespace": "thorium", "resourceVersion": "5"},
            "data": {STATE_KEY: serde_json::to_string(state).expect("state serializes")}
        })
    }

    /// Build a chart shaped cluster in the test namespace with upgrade settings
    ///
    /// # Arguments
    ///
    /// * `upgrade` - The cluster's `spec.upgrade`
    fn chart_cluster(upgrade: serde_json::Value) -> ThoriumCluster {
        // add the upgrade settings to a chart shaped spec
        let mut raw = full_spec();
        raw["upgrade"] = upgrade;
        namespaced_cluster(raw)
    }

    /// Build a fake kube API that answers status patches for a cluster
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster whose status is patched
    fn fake_with_status(cluster: &ThoriumCluster) -> FakeKube {
        FakeKube::default().route(
            "PATCH",
            STATUS_PATH,
            200,
            serde_json::to_value(cluster).expect("cluster serializes"),
        )
    }

    /// Get the status fields patched in a recorded request
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API that received the patches
    fn status_patches(fake: &FakeKube) -> Vec<serde_json::Value> {
        fake.writes()
            .into_iter()
            .filter(|request| request.path == STATUS_PATH)
            .map(|request| request.body["status"].clone())
            .collect()
    }

    /// Get the upgrade state last saved through a fake kube API, if any was
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API that received the writes
    fn saved_state(fake: &FakeKube) -> Option<UpgradeState> {
        // find the last replaced state ConfigMap and parse the state it holds
        let saved = fake
            .writes()
            .into_iter()
            .rfind(|request| request.method == "PUT")?;
        let raw = saved.body["data"][STATE_KEY].as_str().expect("state key");
        Some(serde_json::from_str(raw).expect("state parses"))
    }

    /// Record the reindex step of revision `2026-10-v02` as blocked in a state
    ///
    /// # Arguments
    ///
    /// * `state` - The state to record the block in
    fn block_reindex(state: &mut UpgradeState) {
        // record the block the way the operator does when the step first blocks
        state.record(AppliedStep {
            revision: FIRST_HELM_REVISION.to_owned(),
            step: "elastic-reindex-keyword-mappings".to_owned(),
            outcome: StepState::Blocked,
            message: Some("Waiting for manual work".to_owned()),
            at: None,
            by: None,
        });
    }

    /// Find how the reindex step finished in a saved state
    ///
    /// # Arguments
    ///
    /// * `state` - The saved state
    fn reindex_outcome(state: &UpgradeState) -> &AppliedStep {
        // the finished entry is the last one recorded for the step
        state
            .applied
            .iter()
            .rfind(|applied| applied.step == "elastic-reindex-keyword-mappings")
            .expect("reindex recorded")
    }

    /// Serve canned Elastic answers on a local port, returning its url
    ///
    /// # Arguments
    ///
    /// * `answer` - Picks the status and JSON body for a method and path
    pub(crate) async fn fake_elastic(answer: fn(&str, &str) -> (u16, String)) -> String {
        // listen on any free local port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        // answer every connection with one response
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // read the request head, which is all a bodiless request sends
                    let mut head = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend_from_slice(&chunk[..read]),
                        }
                    }
                    // get the method and path from the request line
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let mut line = head.split_whitespace();
                    let method = line.next().unwrap_or_default().to_owned();
                    let target = line.next().unwrap_or_default();
                    let path = target.split('?').next().unwrap_or_default();
                    // answer HEAD requests without a body
                    let (status, body) = answer(&method, path);
                    let body = if method == "HEAD" {
                        String::new()
                    } else {
                        body
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// Answer Elastic requests as a cluster whose repo results index maps group as text
    ///
    /// # Arguments
    ///
    /// * `method` - The request method
    /// * `path` - The request path
    fn text_group_elastic(method: &str, path: &str) -> (u16, String) {
        match (method, path) {
            // only the repo results index exists
            ("HEAD", "/thorium_repo_results") => (200, String::new()),
            // and it maps group as text
            ("GET", "/thorium_repo_results/_mapping") => (
                200,
                r#"{"thorium_repo_results":{"mappings":{"properties":{"group":{"type":"text"}}}}}"#
                    .to_owned(),
            ),
            _ => (404, "{}".to_owned()),
        }
    }

    /// Answer Elastic requests as a cluster whose repo results index maps group correctly
    ///
    /// # Arguments
    ///
    /// * `method` - The request method
    /// * `path` - The request path
    fn keyword_group_elastic(method: &str, path: &str) -> (u16, String) {
        match (method, path) {
            // only the repo results index exists
            ("HEAD", "/thorium_repo_results") => (200, String::new()),
            // and it maps group as a keyword
            ("GET", "/thorium_repo_results/_mapping") => (
                200,
                r#"{"thorium_repo_results":{"mappings":{"properties":{"group":{"type":"keyword"}}}}}"#
                    .to_owned(),
            ),
            _ => (404, "{}".to_owned()),
        }
    }

    /// Track the status of a cluster from its snapshot like a reconcile does
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to track
    fn tracked(cluster: &ThoriumCluster) -> StatusTracker {
        StatusTracker::new(std::sync::Arc::new(cluster.clone()))
    }

    /// Get the upgrade a gate decided on for a cluster with a recorded state
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to gate
    /// * `state` - The cluster's recorded upgrade state
    async fn pending_upgrade(
        cluster: &ThoriumCluster,
        state: &UpgradeState,
    ) -> (FakeKube, Pending) {
        // serve the recorded state and accept its replacement
        let fake = fake_with_status(cluster)
            .route("GET", STATE_PATH, 200, state_config_map(state))
            .route("PUT", STATE_PATH, 200, state_config_map(state));
        // the gate must decide to upgrade
        let Ok(Gate::Upgrade(pending)) = gate(&fake.client(), &tracked(cluster)).await else {
            panic!("the cluster should be upgraded");
        };
        (fake, pending)
    }

    /// The path of the conversion snapshot Secret in the test namespace
    pub(crate) const SNAPSHOT_PATH: &str =
        "/api/v1/namespaces/thorium/secrets/thorium-legacy-cluster";

    /// The metadata the kube API returns for a Secret that exists
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the Secret
    pub(crate) fn secret_metadata(name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "meta.k8s.io/v1",
            "kind": "PartialObjectMetadata",
            "metadata": {"name": name, "namespace": "thorium"}
        })
    }

    /// Gate a cluster in a namespace without a state and return the state it recorded and the
    /// status patches it wrote
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to gate
    /// * `secrets` - The paths and names of the Secrets the namespace holds
    async fn seeded(
        cluster: &ThoriumCluster,
        secrets: &[(&str, &str)],
    ) -> (UpgradeState, Vec<serde_json::Value>) {
        // serve the Secrets and accept the new state
        let created =
            state_config_map(&UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION));
        let mut fake = fake_with_status(cluster).route("POST", CONFIG_MAPS_PATH, 201, created);
        for (path, name) in secrets {
            fake = fake.route("GET", path, 200, secret_metadata(name));
        }
        // gate the cluster
        gate(&fake.client(), &tracked(cluster)).await.expect("gate");
        // get the state it created
        let created = fake
            .writes()
            .into_iter()
            .find(|request| request.method == "POST")
            .expect("state created");
        let state =
            serde_json::from_str(created.body["data"][STATE_KEY].as_str().expect("state key"))
                .expect("state parses");
        (state, status_patches(&fake))
    }

    /// Check that a written state `ConfigMap` is labelled and annotated for its state
    ///
    /// # Arguments
    ///
    /// * `cm` - The written `ConfigMap` as JSON
    /// * `state` - The state it holds
    fn assert_labels_match_state(cm: &serde_json::Value, state: &UpgradeState) {
        // the revision and version are labelled
        let labels = &cm["metadata"]["labels"];
        assert_eq!(labels[REVISION_LABEL], state.revision.to_string());
        assert_eq!(
            labels[VERSION_LABEL].as_str(),
            state.applied_by_version.as_deref()
        );
        // the label holds the start of the full hash in the annotation
        let full = cm["metadata"]["annotations"][HASH_ANNOTATION]
            .as_str()
            .expect("hash annotation");
        assert_eq!(full, state.hash());
        assert_eq!(labels[HASH_LABEL].as_str(), Some(&full[..32]));
    }

    /// Answer Elastic requests as a megathor deployment whose repo indexes were created
    /// without mappings and whose unused file indexes were left behind
    ///
    /// # Arguments
    ///
    /// * `method` - The request method
    /// * `path` - The request path
    fn megathor_elastic(method: &str, path: &str) -> (u16, String) {
        // every index exists, including the unused ones
        if method == "HEAD" {
            return (200, String::new());
        }
        // the repo indexes map group as text and the sample indexes as a keyword
        let kind = |index: &str| {
            if index.starts_with("thorium_repo_") {
                "text"
            } else {
                "keyword"
            }
        };
        match path.strip_suffix("/_mapping") {
            Some(index) => (
                200,
                format!(
                    r#"{{"{}":{{"mappings":{{"properties":{{"group":{{"type":"{}"}}}}}}}}}}"#,
                    index.trim_start_matches('/'),
                    kind(index.trim_start_matches('/'))
                ),
            ),
            None => (404, "{}".to_owned()),
        }
    }

    /// Answer Elastic requests as a deployment with custom index names whose repo results
    /// index maps group as text
    ///
    /// # Arguments
    ///
    /// * `method` - The request method
    /// * `path` - The request path
    fn custom_index_elastic(method: &str, path: &str) -> (u16, String) {
        match (method, path) {
            // only the custom repo results index exists
            ("HEAD", "/e2e_repo_results") => (200, String::new()),
            // and it maps group as text
            ("GET", "/e2e_repo_results/_mapping") => (
                200,
                r#"{"e2e_repo_results":{"mappings":{"properties":{"group":{"type":"text"}}}}}"#
                    .to_owned(),
            ),
            _ => (404, "{}".to_owned()),
        }
    }

    /// Gate a cluster against a fresh fake and return the fake with the tracked status
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to gate
    /// * `state` - The cluster's recorded upgrade state
    async fn gate_with_state(
        cluster: &ThoriumCluster,
        state: &UpgradeState,
    ) -> (FakeKube, ThoriumClusterStatus) {
        // serve the recorded state
        let fake = fake_with_status(cluster).route("GET", STATE_PATH, 200, state_config_map(state));
        let tracker = tracked(cluster);
        gate(&fake.client(), &tracker).await.expect("gate");
        (fake, tracker.current())
    }

    /// Build the upgrade state `convert-to-helm.sh` seeds a converted namespace with
    fn converted_state() -> UpgradeState {
        // the script records the baseline with its own step and no Thorium version
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        state.applied_by_version = None;
        state.record(AppliedStep {
            revision: BASELINE_REVISION.to_owned(),
            step: "convert-to-helm".to_owned(),
            outcome: StepState::Done,
            message: None,
            at: Some("2026-10-08T00:00:00Z".to_owned()),
            by: Some("convert-to-helm.sh".to_owned()),
        });
        state
    }

    /// The first Helm revision is in the catalog
    #[test]
    fn first_helm_revision_is_known() {
        // parse and look up the revision
        let revision: RevisionId = FIRST_HELM_REVISION.parse().expect("valid revision");
        assert!(upgrades::entry(&revision).is_some());
    }

    /// The legacy message points at the conversion script and lists every signal
    #[test]
    fn legacy_message_explains_conversion() {
        // build the message
        let message = legacy_message(&["a".to_owned(), "b".to_owned()]);
        assert!(message.contains("convert-to-helm.sh"));
        assert!(message.contains("a; b"));
        assert!(message.contains("changed nothing"));
        // it names the doc page rather than a path in the repository
        assert!(message.contains("\"Converting a Pre-Helm Deployment\" in the Thorium docs"));
        assert!(!message.contains("README"));
        assert!(!message.contains("deploy/"));
    }

    /// A cluster at the latest revision is reconciled and reports where it is
    #[test]
    fn decide_current_cluster_reconciles() {
        // a cluster at the latest revision without a target
        let state = UpgradeState::new(upgrades::latest(), OPERATOR_VERSION);
        let Decision::Current(progress) = decide(&state, &spec(None, false)) else {
            panic!("a current cluster should be reconciled");
        };
        // the progress names the current and latest revisions and has nothing left to do
        assert_eq!(progress.current, Some(upgrades::latest().to_string()));
        assert_eq!(progress.latest, Some(upgrades::latest().to_string()));
        assert_eq!(progress.target, None);
        assert_eq!(progress.steps, Vec::new());
        // a target at the latest revision is reported too
        let Decision::Current(progress) = decide(&state, &spec(Some(FIRST_HELM_REVISION), false))
        else {
            panic!("a current cluster should be reconciled");
        };
        assert_eq!(progress.target.as_deref(), Some(FIRST_HELM_REVISION));
    }

    /// A cluster behind the latest revision without a target is held and shown its steps
    #[test]
    fn decide_without_target_holds() {
        // a baseline cluster with no target
        let state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        let Decision::Required { message, progress } = decide(&state, &spec(None, false)) else {
            panic!("a cluster without a target should be held");
        };
        // the message says how to continue
        assert!(message.contains("spec.upgrade.target_revision"));
        assert!(message.contains("operator.cluster.upgrade.targetRevision"));
        // every step to the latest revision is listed as pending
        assert_eq!(progress.current.as_deref(), Some(BASELINE_REVISION));
        assert_eq!(progress.target, None);
        assert_eq!(progress.steps.len(), 4);
        assert!(
            progress
                .steps
                .iter()
                .all(|step| step.state == StepState::Pending)
        );
    }

    /// Auto targeting and an explicit target both start an upgrade to the target
    #[test]
    fn decide_targets_upgrade() {
        // a baseline cluster
        let state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        // auto targeting and an explicit target both upgrade to the latest revision
        for upgrade in [spec(None, true), spec(Some(FIRST_HELM_REVISION), false)] {
            let Decision::Upgrade { target, steps } = decide(&state, &upgrade) else {
                panic!("a targeted cluster should be upgraded");
            };
            assert_eq!(target, upgrades::latest());
            let ids = steps
                .iter()
                .map(|step| step.step.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                ids,
                [
                    "scylla-role",
                    "elastic-identity",
                    "elastic-reindex-keyword-mappings",
                    "finalize"
                ]
            );
        }
    }

    /// An interrupted upgrade resumes after its finished steps
    #[test]
    fn decide_resumes_partial_upgrade() {
        // a baseline cluster that already finished two steps
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let Decision::Upgrade { steps, .. } = decide(&state, &spec(None, true)) else {
            panic!("a targeted cluster should be upgraded");
        };
        // only the unfinished steps are planned
        let ids = steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["elastic-reindex-keyword-mappings", "finalize"]);
    }

    /// A cluster at an older target than the latest revision is held until the target moves
    #[test]
    fn decide_target_at_current_holds() {
        // a baseline cluster targeting its own revision
        let state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        let Decision::Required { message, progress } =
            decide(&state, &spec(Some(BASELINE_REVISION), false))
        else {
            panic!("a cluster at its older target should be held");
        };
        // the message names the target and how to continue
        assert!(message.contains("at its target revision 2026-10-v01"));
        assert_eq!(progress.target.as_deref(), Some(BASELINE_REVISION));
        assert_eq!(progress.steps.len(), 4);
    }

    /// Backwards, malformed, unknown, and too new revisions are errors
    #[test]
    fn decide_errors() {
        // a target older than a current cluster is refused even though nothing would run
        let current = UpgradeState::new(upgrades::latest(), OPERATOR_VERSION);
        let Decision::Error(message) = decide(&current, &spec(Some(BASELINE_REVISION), false))
        else {
            panic!("a backwards target should be an error");
        };
        assert!(message.contains("never move backwards"));
        // malformed and unknown targets are errors
        for target in ["latest", "2099-01-v01", "2026-10-v00"] {
            assert!(
                matches!(
                    decide(&current, &spec(Some(target), false)),
                    Decision::Error(_)
                ),
                "{target} should be an error"
            );
        }
        // a cluster recorded by a newer operator is an error naming the fix
        let ahead = UpgradeState::new(rev("2099-01-v01"), "9.9.9");
        let Decision::Error(message) = decide(&ahead, &spec(None, false)) else {
            panic!("a cluster ahead of the catalog should be an error");
        };
        assert!(message.contains("downgrades are not supported"));
        // a state in a newer layout is an error too, even at a known revision with a target
        let mut newer = UpgradeState::new(rev(BASELINE_REVISION), "9.9.9");
        newer.schema_version = thorium::models::upgrades::STATE_SCHEMA_VERSION + 1;
        for upgrade in [spec(None, false), spec(None, true)] {
            let Decision::Error(message) = decide(&newer, &upgrade) else {
                panic!("a state in a newer layout should be an error");
            };
            assert!(message.contains("written by a newer Thorium operator"));
            assert!(message.contains("downgrades are not supported"));
        }
    }

    /// A fresh namespace starts at the latest revision and an older Helm one at the first
    /// Helm revision, each with a finished record step
    #[test]
    fn starting_state_by_detection() {
        // a fresh install starts at the latest revision
        let (fresh, message) =
            starting_state(Detected::Fresh, "2026-10-08T00:00:00Z".to_owned()).expect("fresh");
        assert_eq!(fresh.revision, upgrades::latest());
        assert_eq!(message, "fresh install");
        assert!(fresh.finished(upgrades::latest().as_str(), "record-revision"));
        assert_eq!(fresh.applied[0].at.as_deref(), Some("2026-10-08T00:00:00Z"));
        assert_eq!(fresh.applied_by_version.as_deref(), Some(OPERATOR_VERSION));
        // a Helm deployment from before states were recorded starts at the first Helm revision
        let (before, _) = starting_state(Detected::BeforeTracking, String::new()).expect("before");
        assert_eq!(before.revision, rev(FIRST_HELM_REVISION));
        assert!(before.finished(FIRST_HELM_REVISION, "record-revision"));
        // a pre-Helm deployment starts at the baseline with the reason it is one
        let (pre_helm, message) =
            starting_state(Detected::PreHelm("why"), String::new()).expect("pre-Helm");
        assert_eq!(pre_helm.revision, rev(BASELINE_REVISION));
        assert_eq!(message, "why");
        assert_eq!(pre_helm.applied[0].message.as_deref(), Some("why"));
    }

    /// Only a step already reported as blocked keeps its blocked status while rechecked
    #[test]
    fn blocked_status_is_kept() {
        // an empty status reports nothing as blocked
        assert!(!reported_blocked(
            &ThoriumClusterStatus::default(),
            "finalize"
        ));
        // report the reindex step as blocked
        let state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        let mut steps = upgrades::plan(&state, &upgrades::latest()).expect("plan");
        steps[2].state = StepState::Blocked;
        let status = ThoriumClusterStatus {
            upgrade: Some(progress(&state.revision, None, steps)),
            ..ThoriumClusterStatus::default()
        };
        // only that step is blocked
        assert!(reported_blocked(
            &status,
            "elastic-reindex-keyword-mappings"
        ));
        assert!(!reported_blocked(&status, "finalize"));
    }

    /// Steps blocked on an admin are rechecked less often than steps waiting on a backend
    #[test]
    fn hold_timing_by_state() {
        // blocked steps wait for an admin
        assert_eq!(
            hold_timing(StepState::Blocked),
            ("is blocked", BLOCKED_REQUEUE_SECS)
        );
        // anything else waits on a backend
        assert_eq!(
            hold_timing(StepState::Running),
            ("is waiting", WAITING_REQUEUE_SECS)
        );
    }

    /// Reaching a target older than the latest revision holds the cluster with its next steps
    #[test]
    fn target_hold_below_latest() {
        // a cluster that reached the baseline revision is held with every step left
        let state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        let (message, progress) = target_hold(&state, &rev(BASELINE_REVISION))
            .expect("plan")
            .expect("held");
        assert!(message.contains("reached its target revision 2026-10-v01"));
        assert_eq!(progress.steps.len(), 4);
        assert_eq!(progress.target.as_deref(), Some(BASELINE_REVISION));
        // reaching the latest revision holds nothing
        let current = UpgradeState::new(upgrades::latest(), OPERATOR_VERSION);
        assert_eq!(
            target_hold(&current, &upgrades::latest()).expect("plan"),
            None
        );
    }

    /// An unconverted pre-Helm cluster is only marked as errored and nothing else is touched
    #[tokio::test]
    async fn gate_holds_unconverted_cluster() {
        // a cluster as the pre-Helm scripts deployed it
        let cluster = namespaced_cluster(pre_helm_spec());
        let fake = fake_with_status(&cluster);
        // the cluster is held for the slow legacy recheck
        let gated = gate(&fake.client(), &tracked(&cluster))
            .await
            .expect("gate");
        let Gate::Hold(action) = gated else {
            panic!("an unconverted cluster should be held");
        };
        assert_eq!(
            action,
            Action::requeue(Duration::from_secs(LEGACY_REQUEUE_SECS))
        );
        // the only request was the error status, so not even the state was read
        let requests = fake.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let status = &status_patches(&fake)[0];
        assert_eq!(status["phase"], "Error");
        assert!(
            status["message"]
                .as_str()
                .is_some_and(|message| message.contains("convert-to-helm.sh"))
        );
    }

    /// A fresh namespace records the latest revision and is reconciled normally
    #[tokio::test]
    async fn gate_seeds_fresh_install() {
        // a chart cluster in a namespace no operator ran in
        let cluster = chart_cluster(serde_json::json!({}));
        let created = state_config_map(&UpgradeState::new(upgrades::latest(), OPERATOR_VERSION));
        let fake = fake_with_status(&cluster).route("POST", CONFIG_MAPS_PATH, 201, created);
        // the cluster is current
        let gated = gate(&fake.client(), &tracked(&cluster))
            .await
            .expect("gate");
        assert!(matches!(gated, Gate::Current));
        // the state was created at the latest revision with a record step
        let created = fake
            .writes()
            .into_iter()
            .find(|request| request.method == "POST")
            .expect("state created");
        assert_eq!(
            created.body["metadata"]["labels"][REVISION_LABEL],
            upgrades::latest().to_string()
        );
        let state: UpgradeState =
            serde_json::from_str(created.body["data"][STATE_KEY].as_str().expect("state key"))
                .expect("state parses");
        assert!(state.finished(upgrades::latest().as_str(), "record-revision"));
        assert_eq!(state.applied.len(), 1);
        assert_eq!(state.applied[0].message.as_deref(), Some("fresh install"));
        assert_eq!(state.applied_by_version.as_deref(), Some(OPERATOR_VERSION));
        // the labels carry the version and the start of the full hash in the annotation
        assert_labels_match_state(&created.body, &state);
        // the progress names the latest revision without changing the phase
        let status = &status_patches(&fake)[0];
        assert_eq!(status["upgrade"]["current"], upgrades::latest().to_string());
        assert!(status.get("phase").is_none());
    }

    /// A Helm namespace an operator already ran in records the first Helm revision and is
    /// reconciled normally
    #[tokio::test]
    async fn gate_records_pre_tracking_helm_cluster() {
        // a chart cluster whose namespace holds the operator's user Secret
        let cluster = chart_cluster(serde_json::json!({}));
        let (state, statuses) =
            seeded(&cluster, &[(OPERATOR_PASS_PATH, "thorium-operator-pass")]).await;
        // the recorded revision is the first Helm revision with the reason
        assert_eq!(state.revision, rev(FIRST_HELM_REVISION));
        assert!(
            state.applied[0]
                .message
                .as_deref()
                .is_some_and(|message| message.contains("before upgrade states were recorded"))
        );
        // the first Helm revision is the latest, so the cluster is current without a target
        let last = statuses.last().expect("status");
        assert_eq!(last["upgrade"]["current"], FIRST_HELM_REVISION);
        assert!(last.get("phase").is_none());
    }

    /// A converted cluster that lost its state starts at the baseline again, so no upgrade
    /// step is skipped, and waits for a target
    #[tokio::test]
    async fn gate_reseeds_converted_cluster_at_baseline() {
        // a converted chart cluster whose operator ran and whose snapshot is kept
        let cluster = chart_cluster(serde_json::json!({}));
        let (state, statuses) = seeded(
            &cluster,
            &[
                (SNAPSHOT_PATH, "thorium-legacy-cluster"),
                (OPERATOR_PASS_PATH, "thorium-operator-pass"),
            ],
        )
        .await;
        // the baseline is recorded with the reason
        assert_eq!(state.revision, rev(BASELINE_REVISION));
        assert!(
            state.applied[0]
                .message
                .as_deref()
                .is_some_and(|message| message.contains("convert-to-helm.sh"))
        );
        // every upgrade step is still ahead of it
        let last = statuses.last().expect("status");
        assert_eq!(last["phase"], "UpgradeRequired");
        assert_eq!(last["upgrade"]["steps"].as_array().map(Vec::len), Some(4));
    }

    /// A pre-Helm cluster kept outside Helm with inline config starts at the baseline
    #[tokio::test]
    async fn gate_seeds_inline_cluster_at_baseline() {
        // a pre-Helm cluster that opted into inline config, deployed by an earlier operator
        let mut cluster = namespaced_cluster(pre_helm_spec());
        cluster.metadata.annotations = Some(std::collections::BTreeMap::from([(
            ALLOW_INLINE_CONFIG.to_owned(),
            "true".to_owned(),
        )]));
        let (state, _) = seeded(&cluster, &[(OPERATOR_PASS_PATH, "thorium-operator-pass")]).await;
        // the baseline is recorded so the first Helm revision's steps run
        assert_eq!(state.revision, rev(BASELINE_REVISION));
        assert!(
            state.applied[0]
                .message
                .as_deref()
                .is_some_and(|message| message.contains("kept outside Helm"))
        );
    }

    /// A state written in a newer layout holds the cluster in error without writing a state
    #[tokio::test]
    async fn gate_refuses_newer_state_layout() {
        // a state a newer operator wrote at a revision this one knows
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "9.9.9");
        state.schema_version = thorium::models::upgrades::STATE_SCHEMA_VERSION + 1;
        let fake = fake_with_status(&cluster)
            .route("GET", STATE_PATH, 200, state_config_map(&state))
            .route("PUT", STATE_PATH, 200, state_config_map(&state));
        // the cluster is held
        let gated = gate(&fake.client(), &tracked(&cluster))
            .await
            .expect("gate");
        assert!(matches!(gated, Gate::Hold(_)));
        // the only write is the error status
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        let status = &status_patches(&fake)[0];
        assert_eq!(status["phase"], "Error");
        assert!(
            status["message"]
                .as_str()
                .is_some_and(|message| message.contains("written by a newer Thorium operator"))
        );
    }

    /// A cluster behind the latest revision without a target changes nothing but its status
    #[tokio::test]
    async fn gate_holds_cluster_without_target() {
        // a cluster converted by convert-to-helm.sh without a target
        let cluster = chart_cluster(serde_json::json!({}));
        let fake = fake_with_status(&cluster).route(
            "GET",
            STATE_PATH,
            200,
            state_config_map(&converted_state()),
        );
        // the cluster is held for an admin
        let gated = gate(&fake.client(), &tracked(&cluster))
            .await
            .expect("gate");
        assert!(matches!(gated, Gate::Hold(_)));
        // the only write is the status listing the steps
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        let status = &status_patches(&fake)[0];
        assert_eq!(status["phase"], "UpgradeRequired");
        assert_eq!(status["upgrade"]["steps"].as_array().map(Vec::len), Some(4));
        assert_eq!(status["upgrade"]["steps"][2]["state"], "Pending");
    }

    /// Setting the target revision on a converted cluster plans every step of the first Helm
    /// revision without writing anything until the steps run
    #[tokio::test]
    async fn gate_starts_upgrade_to_explicit_target() {
        // a cluster converted by convert-to-helm.sh whose admin set the target revision
        let cluster = chart_cluster(serde_json::json!({"target_revision": FIRST_HELM_REVISION}));
        let fake = fake_with_status(&cluster).route(
            "GET",
            STATE_PATH,
            200,
            state_config_map(&converted_state()),
        );
        // the gate hands the upgrade to the runner
        let Ok(Gate::Upgrade(pending)) = gate(&fake.client(), &tracked(&cluster)).await else {
            panic!("a targeted cluster should be upgraded");
        };
        // every step of the target revision is planned from the script's state
        assert_eq!(pending.target, rev(FIRST_HELM_REVISION));
        let ids = pending
            .steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "scylla-role",
                "elastic-identity",
                "elastic-reindex-keyword-mappings",
                "finalize"
            ]
        );
        // nothing is written before the first step runs
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    }

    /// A malformed recorded state is an error rather than a fresh start
    #[tokio::test]
    async fn gate_rejects_bad_state() {
        // a state ConfigMap whose revision isn't valid
        let cluster = chart_cluster(serde_json::json!({}));
        let bad = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": STATE_CONFIG_MAP},
            "data": {STATE_KEY: r#"{"revision":"latest"}"#}
        });
        let fake = fake_with_status(&cluster).route("GET", STATE_PATH, 200, bad);
        // the gate fails naming the ConfigMap without writing a new state
        let Err(error) = gate(&fake.client(), &tracked(&cluster)).await else {
            panic!("a malformed state should fail the gate");
        };
        assert!(error.to_string().contains(STATE_CONFIG_MAP), "{error}");
        assert!(fake.writes().is_empty());
    }

    /// The last step of an upgrade records the new revision and hands the cluster to the
    /// normal reconcile
    #[tokio::test]
    async fn run_finishes_upgrade() {
        // a cluster that finished every step but the last
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(
            &mut state,
            &[
                "scylla-role",
                "elastic-identity",
                "elastic-reindex-keyword-mappings",
            ],
        );
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        assert_eq!(pending.steps.len(), 1);
        // run the upgrade
        let meta = meta_for(cluster, &fake.client());
        assert_eq!(run(&meta, pending).await.expect("run"), None);
        // the state was replaced at the version read with the new revision
        let written = fake
            .writes()
            .into_iter()
            .rfind(|request| request.method == "PUT")
            .expect("state saved");
        assert_eq!(written.body["metadata"]["resourceVersion"], "5");
        let saved = saved_state(&fake).expect("state saved");
        assert_eq!(saved.revision, rev(FIRST_HELM_REVISION));
        assert!(saved.finished(FIRST_HELM_REVISION, "finalize"));
        // every step is recorded once and the new revision names this operator
        for step in [
            "scylla-role",
            "elastic-identity",
            "elastic-reindex-keyword-mappings",
            "finalize",
        ] {
            let count = saved
                .applied
                .iter()
                .filter(|applied| applied.step == step)
                .count();
            assert_eq!(count, 1, "{step}");
        }
        assert_eq!(saved.applied_by_version.as_deref(), Some(OPERATOR_VERSION));
        // the labels carry the new revision, version, and the start of the full hash
        assert_labels_match_state(&written.body, &saved);
        // the cluster ends in the provisioning phase at its new revision
        let last = status_patches(&fake).pop().expect("status");
        assert_eq!(last["phase"], "Provisioning");
        assert_eq!(last["upgrade"]["current"], FIRST_HELM_REVISION);
    }

    /// A ready cluster upgraded by `auto_target_dev` gets back to ready in the same reconcile
    /// even though ready is what the reconcile started from
    #[tokio::test]
    async fn ready_restored_after_auto_targeted_upgrade() {
        // a ready cluster that finished every step but the last
        let mut cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        cluster.status = Some(ThoriumClusterStatus {
            phase: Some(ClusterPhase::Ready),
            bootstrap_hash: Some("hash".to_owned()),
            ..ThoriumClusterStatus::default()
        });
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(
            &mut state,
            &[
                "scylla-role",
                "elastic-identity",
                "elastic-reindex-keyword-mappings",
            ],
        );
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // run the upgrade, which hands the cluster to the deploy as provisioning
        let meta = meta_for(cluster, &fake.client());
        assert_eq!(run(&meta, pending).await.expect("run"), None);
        assert_eq!(
            status_patches(&fake).pop().expect("status")["phase"],
            "Provisioning"
        );
        // the finished deploy reports ready with the bootstrap the reconcile started with
        crds::set_status_with_bootstrap(
            &meta.client,
            &meta.status,
            ClusterPhase::Ready,
            None,
            "hash".to_owned(),
        )
        .await;
        // ready is written rather than skipped as matching the starting snapshot
        let last = status_patches(&fake).pop().expect("status");
        assert_eq!(last["phase"], "Ready");
        assert_eq!(last["message"], serde_json::Value::Null);
    }

    /// An index whose group field isn't a keyword blocks the upgrade on an approval
    #[tokio::test]
    async fn run_blocks_on_text_mappings() {
        // a cluster whose only unfinished data step is the reindex check
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // point the cluster at an Elastic with a text group mapping
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(text_group_elastic).await;
        // the upgrade is blocked and rechecked on the blocked interval
        let action = run(&meta, pending).await.expect("run");
        assert_eq!(
            action,
            Some(Action::requeue(Duration::from_secs(BLOCKED_REQUEUE_SECS)))
        );
        // the block is recorded without finishing the step or moving the revision
        let saved = saved_state(&fake).expect("block recorded");
        assert!(recorded_blocked(
            &saved,
            FIRST_HELM_REVISION,
            "elastic-reindex-keyword-mappings"
        ));
        assert!(!saved.finished(FIRST_HELM_REVISION, "elastic-reindex-keyword-mappings"));
        assert_eq!(saved.revision, rev(BASELINE_REVISION));
        // the status names the index and the approval it waits for
        let last = status_patches(&fake).pop().expect("status");
        assert_eq!(last["phase"], "Upgrading");
        assert_eq!(last["upgrade"]["steps"][0]["state"], "Blocked");
        let message = last["upgrade"]["steps"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("thorium_repo_results"));
        assert!(message.contains("Waiting for an approval"));
        assert!(message.contains("operator.cluster.upgrade.approvals"));
    }

    /// An approved reindex is still blocked, on the manual procedure since the operator can't
    /// reindex on its own
    #[tokio::test]
    async fn run_approved_reindex_waits_for_manual_work() {
        // a cluster that approved the reindex with a backup
        let cluster = chart_cluster(serde_json::json!({
            "auto_target_dev": true,
            "approvals": [{"step": "elastic-reindex-keyword-mappings", "backup": "snapshot-1"}]
        }));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // point the cluster at an Elastic with a text group mapping
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(text_group_elastic).await;
        // the upgrade is blocked and the step is left unfinished
        let action = run(&meta, pending).await.expect("run");
        assert_eq!(
            action,
            Some(Action::requeue(Duration::from_secs(BLOCKED_REQUEUE_SECS)))
        );
        let saved = saved_state(&fake).expect("block recorded");
        assert!(!saved.finished(FIRST_HELM_REVISION, "elastic-reindex-keyword-mappings"));
        // the status gives the manual procedure rather than asking for an approval
        let last = status_patches(&fake).pop().expect("status");
        let message = last["upgrade"]["steps"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("Waiting for manual work"));
        assert!(message.contains("search-streamer"));
        assert!(!message.contains("Waiting for an approval"));
    }

    /// Correct mappings skip the reindex step and finish the upgrade
    #[tokio::test]
    async fn run_skips_reindex_when_mappings_are_correct() {
        // a cluster whose only unfinished steps are the reindex check and finalize
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // point the cluster at an Elastic with a keyword group mapping
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(keyword_group_elastic).await;
        // the upgrade finishes
        assert_eq!(run(&meta, pending).await.expect("run"), None);
        // the reindex step is recorded as skipped rather than fixed manually
        let saved = saved_state(&fake).expect("state saved");
        let reindex = reindex_outcome(&saved);
        assert_eq!(reindex.outcome, StepState::Skipped);
        assert!(
            !reindex
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("fixed manually")
        );
        assert_eq!(saved.revision, rev(FIRST_HELM_REVISION));
    }

    /// An unreachable Elastic leaves the step running and rechecks it soon
    #[tokio::test]
    async fn run_waits_on_unreachable_backend() {
        // a cluster whose only unfinished data step is the reindex check
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // point the cluster at a local port nothing listens on
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = format!("http://{addr}");
        // the step waits on the backend
        let action = run(&meta, pending).await.expect("run");
        assert_eq!(
            action,
            Some(Action::requeue(Duration::from_secs(WAITING_REQUEUE_SECS)))
        );
        let last = status_patches(&fake).pop().expect("status");
        assert_eq!(last["upgrade"]["steps"][0]["state"], "Running");
    }

    /// A step already reported as blocked isn't reported as running again while rechecked
    #[tokio::test]
    async fn run_keeps_blocked_status_while_rechecking() {
        // a cluster whose status already reports the reindex step as blocked
        let mut cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        let mut steps = pending.steps.clone();
        steps[0].state = StepState::Blocked;
        cluster.status = Some(ThoriumClusterStatus {
            upgrade: Some(progress(&state.revision, Some(&pending.target), steps)),
            ..ThoriumClusterStatus::default()
        });
        let before = status_patches(&fake).len();
        // recheck the step against an Elastic that still maps group as text
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(text_group_elastic).await;
        run(&meta, pending).await.expect("run");
        // the only new status is the blocked one, never a running one
        let patches = status_patches(&fake);
        let new = &patches[before..];
        assert_eq!(new.len(), 1, "{new:?}");
        assert_eq!(new[0]["upgrade"]["steps"][0]["state"], "Blocked");
    }

    /// Only a skip after an earlier block is credited as a manual fix
    #[test]
    fn manual_fix_is_credited_after_a_block() {
        // a skip after a block becomes done with the skip reason
        assert_eq!(
            credit_manual_fix(steps::StepResult::Skipped("nothing to do".to_owned()), true),
            steps::StepResult::Done(Some("fixed manually: nothing to do".to_owned()))
        );
        // a skip without an earlier block stays a skip
        assert_eq!(
            credit_manual_fix(
                steps::StepResult::Skipped("nothing to do".to_owned()),
                false
            ),
            steps::StepResult::Skipped("nothing to do".to_owned())
        );
        // other results are left alone whether or not the step blocked before
        for result in [
            steps::StepResult::Done(None),
            steps::StepResult::Blocked("still blocked".to_owned()),
            steps::StepResult::Waiting("down".to_owned()),
        ] {
            assert_eq!(credit_manual_fix(result.clone(), true), result);
        }
    }

    /// A block only counts for the same step of the same revision
    #[test]
    fn recorded_block_matches_step_and_revision() {
        // a state with the reindex step blocked
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        assert!(!recorded_blocked(
            &state,
            FIRST_HELM_REVISION,
            "elastic-reindex-keyword-mappings"
        ));
        block_reindex(&mut state);
        // the block is found for that step only
        assert!(recorded_blocked(
            &state,
            FIRST_HELM_REVISION,
            "elastic-reindex-keyword-mappings"
        ));
        assert!(!recorded_blocked(&state, FIRST_HELM_REVISION, "finalize"));
        assert!(!recorded_blocked(
            &state,
            BASELINE_REVISION,
            "elastic-reindex-keyword-mappings"
        ));
        // a block entry doesn't finish the step
        assert!(!state.finished(FIRST_HELM_REVISION, "elastic-reindex-keyword-mappings"));
    }

    /// A step whose history records a block and that now has nothing to do is recorded as
    /// done and fixed manually, and the upgrade finishes
    #[tokio::test]
    async fn run_credits_manual_fix_recorded_in_history() {
        // a cluster whose history records the reindex step as blocked
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        block_reindex(&mut state);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // the admin fixed the mappings by hand
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(keyword_group_elastic).await;
        assert_eq!(run(&meta, pending).await.expect("run"), None);
        // the reindex step is recorded as done and fixed manually
        let saved = saved_state(&fake).expect("state saved");
        let reindex = reindex_outcome(&saved);
        assert_eq!(reindex.outcome, StepState::Done);
        assert!(
            reindex
                .message
                .as_deref()
                .unwrap_or_default()
                .starts_with("fixed manually")
        );
        assert_eq!(saved.revision, rev(FIRST_HELM_REVISION));
    }

    /// A step the status reports as blocked, without a block in the history, is also credited
    /// as fixed manually once it has nothing to do
    #[tokio::test]
    async fn run_credits_manual_fix_reported_in_status() {
        // a cluster whose status reports the reindex step as blocked
        let mut cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        let mut steps = pending.steps.clone();
        steps[0].state = StepState::Blocked;
        cluster.status = Some(ThoriumClusterStatus {
            upgrade: Some(progress(&state.revision, Some(&pending.target), steps)),
            ..ThoriumClusterStatus::default()
        });
        // the admin fixed the mappings by hand
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(keyword_group_elastic).await;
        assert_eq!(run(&meta, pending).await.expect("run"), None);
        // the reindex step is recorded as done and fixed manually
        let saved = saved_state(&fake).expect("state saved");
        let reindex = reindex_outcome(&saved);
        assert_eq!(reindex.outcome, StepState::Done);
        assert!(
            reindex
                .message
                .as_deref()
                .unwrap_or_default()
                .starts_with("fixed manually")
        );
    }

    /// A step whose block is already in the history isn't recorded again while rechecked
    #[tokio::test]
    async fn run_records_a_block_once() {
        // a cluster whose history already records the reindex step as blocked
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        block_reindex(&mut state);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        // the mappings still need manual work
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(text_group_elastic).await;
        let action = run(&meta, pending).await.expect("run");
        assert_eq!(
            action,
            Some(Action::requeue(Duration::from_secs(BLOCKED_REQUEUE_SECS)))
        );
        // the state isn't written again
        assert!(saved_state(&fake).is_none());
    }

    /// Targets outside the catalog are unknown, including ones older than the cluster's
    /// revision, since the target is resolved before it is compared with the cluster
    #[test]
    fn decide_targets_outside_catalog() {
        // a baseline cluster
        let state = UpgradeState::new(rev(BASELINE_REVISION), OPERATOR_VERSION);
        // a well formed target past the catalog and one before it are both unknown
        for target in ["2026-10-v09", "2026-09-v01"] {
            let Decision::Error(message) = decide(&state, &spec(Some(target), false)) else {
                panic!("{target} should be an error");
            };
            assert!(
                message.contains("not known to this Thorium build"),
                "{target}: {message}"
            );
        }
    }

    /// A current cluster's state is never rewritten and a status that already matches isn't
    /// written again, so redeploys and reinstalls change nothing
    #[tokio::test]
    async fn gate_current_cluster_leaves_state_alone() {
        // a chart cluster already at the latest revision
        let cluster = chart_cluster(serde_json::json!({}));
        let state = UpgradeState::new(upgrades::latest(), OPERATOR_VERSION);
        let (fake, status) = gate_with_state(&cluster, &state).await;
        // only the progress was reported and the state was left alone
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].path, STATUS_PATH);
        // the next reconcile, seeing that progress, writes nothing at all
        let mut reported = cluster.clone();
        reported.status = Some(status);
        let (fake, _) = gate_with_state(&reported, &state).await;
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    }

    /// A held cluster whose status already reports the hold isn't written again
    #[tokio::test]
    async fn gate_held_status_is_stable() {
        // a converted cluster at the baseline without a target
        let cluster = chart_cluster(serde_json::json!({}));
        let state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        let (_, status) = gate_with_state(&cluster, &state).await;
        assert_eq!(status.phase, Some(ClusterPhase::UpgradeRequired));
        // the hold message names both revisions and the Helm value to set
        let message = status.message.clone().unwrap_or_default();
        assert!(message.contains(BASELINE_REVISION) && message.contains(FIRST_HELM_REVISION));
        assert!(message.contains("operator.cluster.upgrade.targetRevision"));
        // the next reconcile, seeing that status, writes nothing
        let mut reported = cluster.clone();
        reported.status = Some(status);
        let (fake, _) = gate_with_state(&reported, &state).await;
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    }

    /// A state recorded by an older Thorium with fields this build doesn't know still upgrades,
    /// and the new revision is recorded with this operator's version
    #[tokio::test]
    async fn run_upgrades_state_from_older_version() {
        // a state edited by hand with an old version and an unknown field
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let raw = serde_json::json!({
            "schema_version": 1,
            "revision": BASELINE_REVISION,
            "applied_by_version": "0.9.0",
            "note": "edited by hand",
            "applied": [
                {"revision": FIRST_HELM_REVISION, "step": "scylla-role", "outcome": "Done"},
                {"revision": FIRST_HELM_REVISION, "step": "elastic-identity", "outcome": "Done"},
                {"revision": FIRST_HELM_REVISION, "step": "elastic-reindex-keyword-mappings", "outcome": "Done"}
            ]
        });
        let cm = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": STATE_CONFIG_MAP, "namespace": "thorium", "resourceVersion": "5"},
            "data": {STATE_KEY: raw.to_string()}
        });
        let fake = fake_with_status(&cluster)
            .route("GET", STATE_PATH, 200, cm.clone())
            .route("PUT", STATE_PATH, 200, cm);
        // the gate parses the state and plans the last step
        let Ok(Gate::Upgrade(pending)) = gate(&fake.client(), &tracked(&cluster)).await else {
            panic!("the cluster should be upgraded");
        };
        assert_eq!(
            pending.stored.state.applied_by_version.as_deref(),
            Some("0.9.0")
        );
        // the upgrade finishes and records this operator's version
        let meta = meta_for(cluster, &fake.client());
        assert_eq!(run(&meta, pending).await.expect("run"), None);
        let saved = saved_state(&fake).expect("state saved");
        assert_eq!(saved.revision, rev(FIRST_HELM_REVISION));
        assert_eq!(saved.applied_by_version.as_deref(), Some(OPERATOR_VERSION));
    }

    /// The reindex block names only the indexes that don't map group as a keyword, not the
    /// correct sample indexes or unused leftovers
    #[tokio::test]
    async fn run_blocks_naming_only_text_indexes() {
        // a megathor style cluster whose remaining data step is the reindex check
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.node = fake_elastic(megathor_elastic).await;
        // the step blocks
        run(&meta, pending).await.expect("run");
        let last = status_patches(&fake).pop().expect("status");
        assert_eq!(last["upgrade"]["steps"][0]["state"], "Blocked");
        // only the two repo indexes are named
        let message = last["upgrade"]["steps"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.contains("thorium_repo_results, thorium_repo_tags"),
            "{message}"
        );
        assert!(!message.contains("thorium_sample_"), "{message}");
        assert!(!message.contains("thorium_file_"), "{message}");
    }

    /// The reindex check uses the index names configured for the cluster
    #[tokio::test]
    async fn run_checks_custom_index_names() {
        // a cluster with custom index names whose remaining data step is the reindex check
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.elastic.results.repos = "e2e_repo_results".to_owned();
        meta.conf.elastic.results.samples = "e2e_sample_results".to_owned();
        meta.conf.elastic.tags.repos = "e2e_repo_tags".to_owned();
        meta.conf.elastic.tags.samples = "e2e_sample_tags".to_owned();
        meta.conf.elastic.node = fake_elastic(custom_index_elastic).await;
        // the step blocks on the custom index
        run(&meta, pending).await.expect("run");
        let last = status_patches(&fake).pop().expect("status");
        let message = last["upgrade"]["steps"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("e2e_repo_results"), "{message}");
        assert!(!message.contains("thorium_repo_results"), "{message}");
    }

    /// Rechecking a blocked step that is still blocked writes neither the status nor the state
    #[tokio::test]
    async fn run_blocked_recheck_writes_nothing() {
        // block the reindex step once
        let cluster = chart_cluster(serde_json::json!({"auto_target_dev": true}));
        let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
        finish(&mut state, &["scylla-role", "elastic-identity"]);
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        let mut meta = meta_for(cluster.clone(), &fake.client());
        let elastic = fake_elastic(text_group_elastic).await;
        meta.conf.elastic.node.clone_from(&elastic);
        run(&meta, pending).await.expect("run");
        // recheck with the recorded block and the reported status, as the requeue does
        let blocked = saved_state(&fake).expect("block recorded");
        let mut reported = cluster;
        reported.status = Some(meta.status.current());
        let (fake, pending) = pending_upgrade(&reported, &blocked).await;
        let mut meta = meta_for(reported, &fake.client());
        meta.conf.elastic.node = elastic;
        let action = run(&meta, pending).await.expect("run");
        assert_eq!(
            action,
            Some(Action::requeue(Duration::from_secs(BLOCKED_REQUEUE_SECS)))
        );
        // nothing was written
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    }
}
