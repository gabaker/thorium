//! Handing a cluster over to the previous operator once a downgrade finished
//!
//! The newer operator runs the downgrade, since only it knows how to undo its changes, and keeps
//! running until `helm upgrade` replaces it. An older operator reapplies its own CRD and parses
//! the cluster's spec, status, and upgrade state with types that don't know what the newer one
//! added, so the last step of a downgrade strips every newer field and value from them, and
//! hand-over mode keeps the newer operator from writing any of it back: it reports only status the
//! previous types parse, and it leaves the paused components at zero, since the only Deployments it
//! could apply are the newer release's.

use kube::Client;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use thorium::Error;
use thorium::models::upgrades::{RevisionId, StepState, UpgradeState, UpgradeStatus};

use super::state::StoredState;
use super::{FIRST_HELM_REVISION, HOLD_REQUEUE_SECS, Pending, catalog, pause, state};
use crate::k8s::clusters::{ClusterMeta, cluster_name_and_namespace};
use crate::k8s::crds::{self, ClusterPhase, StatusTracker, ThoriumCluster, UpgradeSpec};

/// The key in the upgrade state's schema markers naming the revision a downgrade this operator ran
/// left the cluster at, which puts the operator in hand-over mode while the cluster stays there
pub const HANDED_OVER_MARKER: &str = "handed_over";

/// Whether an outcome is one the previous release's state and status types can't read
///
/// # Arguments
///
/// * `outcome` - The outcome of a history entry
fn newer_only(outcome: StepState) -> bool {
    // the previous release knew six states
    matches!(
        outcome,
        StepState::Reingesting | StepState::Waiting | StepState::Unknown
    )
}

/// Whether the cluster is in hand-over mode: a downgrade this operator ran left it below this
/// operator's latest revision, and nobody asked to move it up again
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `spec` - The cluster's upgrade settings
#[must_use]
pub(super) fn applies(state: &UpgradeState, spec: &UpgradeSpec) -> bool {
    // a downgrade this operator ran recorded the revision it left the cluster at
    let handed = state
        .schema
        .get(HANDED_OVER_MARKER)
        .is_some_and(|revision| revision == state.revision.as_str());
    // it only holds while the cluster is below what this operator deploys
    let below = state.revision < catalog().latest();
    // and a target above the cluster's revision starts an upgrade, which ends hand-over; so does a
    // dev cluster that follows the latest revision with no explicit target
    let raised = match spec.target_revision.as_deref() {
        Some(target) => target
            .parse::<RevisionId>()
            .is_ok_and(|target| target > state.revision),
        None => spec.auto_target_dev && catalog().latest() > state.revision,
    };
    handed && below && !raised
}

/// Build the upgrade state in the layout the previous release reads
///
/// Entries holding an outcome the previous release can't parse are removed. The fields it doesn't
/// know stay, since it ignores them and the paused replica counts it doesn't read live there.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
#[must_use]
pub fn previous_state(state: &UpgradeState) -> UpgradeState {
    // the same state without the entries the previous parser would reject
    let mut previous = state.clone();
    previous.applied.retain(|entry| !newer_only(entry.outcome));
    previous
}

/// Rewrite a handed-over state in the previous layout when the hand-over stopped before it did
///
/// The state is only saved when it still holds an entry the previous release can't parse.
///
/// # Arguments
///
/// * `client` - The kube client to save with
/// * `namespace` - The namespace the state lives in
/// * `stored` - The recorded upgrade state, rewritten when needed
pub(super) async fn keep_previous_layout(
    client: &Client,
    namespace: &str,
    stored: &mut StoredState,
) -> Result<(), Error> {
    // a state the hand-over already rewrote holds nothing the previous parser rejects
    if !stored
        .state
        .applied
        .iter()
        .any(|entry| newer_only(entry.outcome))
    {
        return Ok(());
    }
    // drop those entries, keeping the marker and everything else
    stored.state = previous_state(&stored.state);
    state::save(client, namespace, stored).await
}

/// Build the upgrade status the previous release parses, with nothing planned
///
/// # Arguments
///
/// * `current` - The revision the cluster is at
/// * `target` - The revision the cluster was reverted to
pub(super) fn previous_status(current: &RevisionId, target: Option<&RevisionId>) -> UpgradeStatus {
    // no planned steps, conditions, drift, or losses: only what the previous types know
    UpgradeStatus {
        current: Some(current.to_string()),
        target: target.map(ToString::to_string),
        latest: Some(catalog().latest().to_string()),
        ..UpgradeStatus::default()
    }
}

/// The phase a handed-over cluster reports, one the operator of the revision it was reverted to
/// parses
///
/// The first Helm release's operator, and the pre-Helm one before it, know only `Provisioning`,
/// `Ready`, and `Error`, and fail to list every `ThoriumCluster` when one holds another phase. Of
/// those, `Error` is the one that stops tools waiting for `Ready`, as `UpgradeRequired` does for
/// the releases that know it.
///
/// # Arguments
///
/// * `revision` - The revision the cluster was reverted to
fn phase(revision: &RevisionId) -> ClusterPhase {
    // a revision up to the first Helm release's is deployed by an operator without the newer phases
    let first_helm = FIRST_HELM_REVISION.parse::<RevisionId>().ok();
    if first_helm.is_some_and(|first_helm| *revision <= first_helm) {
        ClusterPhase::Error
    } else {
        ClusterPhase::UpgradeRequired
    }
}

/// Describe what an admin does next, with the replica counts the paused components must return to
///
/// # Arguments
///
/// * `revision` - The revision the cluster was reverted to
/// * `paused` - The paused components' recorded replica counts, by Deployment name
fn message(revision: &RevisionId, paused: &BTreeMap<String, i32>) -> String {
    // the counts the status keeps showing until the previous operator restores them
    let counts = if paused.is_empty() {
        String::new()
    } else {
        let list = paused
            .iter()
            .map(|(name, count)| format!("{name} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            " The components paused for the downgrade stay scaled to zero until the previous \
             operator's rollout restores them to their recorded replica counts: {list}; an \
             operator from before this feature doesn't read them, so scale them back by hand to \
             those counts."
        )
    };
    format!(
        "This cluster was reverted to revision {revision} and is handed over: deploy the previous \
         Thorium operator and images now (helm upgrade to the previous release). This operator \
         changes nothing more until then.{counts}"
    )
}

/// Remove the spec fields and approval fields the previous release doesn't have
///
/// Only a spec that holds some of them is patched.
///
/// # Arguments
///
/// * `client` - The kube client to patch the cluster with
/// * `cluster` - The cluster, whose spec is stripped
pub(super) async fn strip_spec(client: &Client, cluster: &ThoriumCluster) -> Result<(), Error> {
    // the fields the previous release's spec doesn't have
    let upgrade = &cluster.spec.upgrade;
    let approvals_carry = upgrade.approvals.iter().any(|approval| {
        approval.accept_loss
            || approval.confirm.is_some()
            || !approval.accept_issues.is_empty()
            || approval.accept_backup_gaps
    });
    if !upgrade.dangerous_downgrade_lossy
        && upgrade.drift_acknowledged.is_empty()
        && upgrade.starting_revision.is_none()
        && !approvals_carry
    {
        return Ok(());
    }
    // a merge patch removes a field with null and replaces a list whole
    let approvals = upgrade
        .approvals
        .iter()
        .map(|approval| json!({"step": approval.step, "backup": approval.backup}))
        .collect::<Vec<_>>();
    let patch = json!({"spec": {"upgrade": {
        "dangerous_downgrade_lossy": null,
        "drift_acknowledged": null,
        "starting_revision": null,
        "approvals": approvals,
    }}});
    // patch the cluster itself, not its status
    let (name, namespace) = cluster_name_and_namespace(cluster)?;
    let api: Api<ThoriumCluster> = Api::namespaced(client.clone(), &namespace);
    api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to remove the newer fields from the spec of ThoriumCluster \
                 {namespace}/{name}: {error}"
            ))
        })?;
    Ok(())
}

/// Hand a cluster over after the downgrade this operator ran finished
///
/// The upgrade state is rewritten in the previous layout and marked as handed over, the newer spec
/// and status fields are removed, and the status says what an admin does next.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade that just lowered the revision, whose state is rewritten
/// * `target` - The revision the cluster was reverted to
pub(super) async fn finish(
    meta: &ClusterMeta,
    pending: &mut Pending,
    target: &RevisionId,
) -> Result<Action, Error> {
    // the state in the previous layout, marked so later reconciles stay in hand-over mode
    let mut previous = previous_state(&pending.stored.state);
    previous
        .schema
        .insert(HANDED_OVER_MARKER.to_owned(), previous.revision.to_string());
    pending.stored.state = previous;
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    // the spec and status the previous operator would trip over
    strip_spec(&meta.client, &meta.cluster).await?;
    let paused = pause::paused(&pending.stored.state);
    crds::set_handover_status(
        &meta.client,
        &meta.status,
        phase(&pending.stored.state.revision),
        message(&pending.stored.state.revision, &paused),
        previous_status(&pending.stored.state.revision, Some(target)),
    )
    .await;
    Ok(Action::requeue(Duration::from_secs(HOLD_REQUEUE_SECS)))
}

/// Hold a cluster in hand-over mode: report only what the previous types parse, strip anything
/// newer that came back, and change nothing else
///
/// # Arguments
///
/// * `client` - The kube client to patch with
/// * `tracker` - The cluster and the status last written for it
/// * `state` - The recorded upgrade state
pub(super) async fn hold(client: &Client, tracker: &StatusTracker, state: &UpgradeState) -> Action {
    // a spec that gained newer fields since is stripped again
    if let Err(error) = strip_spec(client, tracker.cluster()).await {
        println!("{error}");
    }
    // the status stays in the previous layout
    let paused = pause::paused(state);
    let target = tracker
        .cluster()
        .spec
        .upgrade
        .target_revision
        .as_deref()
        .and_then(|target| target.parse::<RevisionId>().ok());
    crds::set_handover_status(
        client,
        tracker,
        phase(&state.revision),
        message(&state.revision, &paused),
        previous_status(&state.revision, target.as_ref()),
    )
    .await;
    Action::requeue(Duration::from_secs(HOLD_REQUEUE_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cluster reverted to the first Helm release's revision or below reports `Error`, which
    /// that release's operator parses; one reverted to a later revision keeps `UpgradeRequired`
    #[test]
    fn the_phase_follows_what_the_previous_operator_parses() {
        // the revisions of the pre-Helm baseline, the first Helm release, and the first with revisions
        let revision = |id: &str| id.parse::<RevisionId>().expect("a revision id");
        // each maps to a phase the operator deploying that revision parses
        assert_eq!(phase(&revision("2026-10-v01")), ClusterPhase::Error);
        assert_eq!(phase(&revision("2026-10-v02")), ClusterPhase::Error);
        assert_eq!(
            phase(&revision("2026-10-v03")),
            ClusterPhase::UpgradeRequired
        );
    }
}
