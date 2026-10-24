//! Pausing components while a step changes the data they use, and restoring them once the new
//! release's Deployments are applied
//!
//! Before a step that pauses components runs, the operator records each Deployment's replica count
//! on the step's `Running` history entry and scales it to zero. The components stay at zero through
//! the rest of the plan. Only when the operator applies the new release's Deployments are they
//! scaled back, each to its recorded count, so old pods never run against migrated data. The counts
//! are read from the history, which the migration Job writes into `schema_migrations` as its first
//! action, so they survive a lost state `ConfigMap`.

use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{Patch, PatchParams};
use std::collections::BTreeMap;
use thorium::Error;
use thorium::models::upgrades::{AppliedStep, StepState, UpgradeComponent, UpgradeState};

use super::OPERATOR_VERSION;
use super::state::{self, StoredState};
use crate::k8s::clusters::ClusterMeta;

/// The step of the history entry the operator records when it scales paused components back up
///
/// The entry's `paused` map holds the counts it restored, which end those components' pauses.
pub const RESTORE_STEP: &str = "restore-paused";

/// How the replica counts of the components a step paused were found
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The replica count of each component to pause, by Deployment name; a component that is already
    /// paused keeps the count first recorded for it
    pub counts: BTreeMap<String, i32>,
    /// The components whose count was lost, so the count their `ThoriumCluster` spec declares was
    /// recorded
    pub from_spec: Vec<String>,
}

/// Get the components the history holds paused, by Deployment name, with the counts recorded first
///
/// A pause begins with a step's `Running` entry that holds counts and ends with the entry that
/// restored the component, so later steps pausing a component already paused keep its first count.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
#[must_use]
pub fn paused(state: &UpgradeState) -> BTreeMap<String, i32> {
    // walk the history oldest first, so a restore ends only the pauses before it
    let mut paused = BTreeMap::new();
    for entry in &state.applied {
        if entry.step == RESTORE_STEP {
            for name in entry.paused.keys() {
                paused.remove(name);
            }
        } else if entry.outcome == StepState::Running {
            for (name, count) in &entry.paused {
                paused.entry(name.clone()).or_insert(*count);
            }
        }
    }
    paused
}

/// Get the replica count a cluster's spec declares for a component
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `component` - The component
fn declared(meta: &ClusterMeta, component: UpgradeComponent) -> Option<i32> {
    // the API's count is configurable; every other component runs one replica when it is in the spec
    let components = &meta.cluster.spec.components;
    match component {
        UpgradeComponent::Api => Some(i32::from(components.api.replicas)),
        UpgradeComponent::Scaler => components.scaler.is_some().then_some(1),
        UpgradeComponent::BaremetalScaler => components.baremetal_scaler.is_some().then_some(1),
        UpgradeComponent::EventHandler => components.event_handler.is_some().then_some(1),
        UpgradeComponent::SearchStreamer => components.search_streamer.is_some().then_some(1),
        UpgradeComponent::Unknown => None,
    }
}

/// Read a Deployment, treating a missing one as absent
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The Deployment's name
async fn get(meta: &ClusterMeta, name: &str) -> Result<Option<Deployment>, Error> {
    // a component that isn't deployed has nothing to pause
    meta.deploy_api.get_opt(name).await.map_err(|error| {
        Error::new(format!(
            "Failed to read the {name} deployment in namespace {}: {error}",
            meta.namespace
        ))
    })
}

/// Scale a Deployment to a replica count
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The Deployment's name
/// * `replicas` - The count to scale to
async fn scale(meta: &ClusterMeta, name: &str, replicas: i32) -> Result<(), Error> {
    // a merge patch of the count alone leaves everything else the rollout set
    let patch = serde_json::json!({"spec": {"replicas": replicas}});
    meta.deploy_api
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to scale the {name} deployment in namespace {} to {replicas}: {error}",
                meta.namespace
            ))
        })?;
    println!(
        "Scaled the {name} deployment in namespace {} to {replicas}",
        meta.namespace
    );
    Ok(())
}

/// Read the replica count of each component a step pauses
///
/// A component the history already holds paused keeps its recorded count. One that runs now is
/// recorded at its live count. One that is at zero with no recorded pause lost its count (the state
/// was lost between scaling it down and the Job's first write), so the count its `ThoriumCluster`
/// spec declares is recorded and the component named in the result. A component with no Deployment
/// has nothing to pause.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `state` - The recorded upgrade state
/// * `components` - The components the step pauses
pub async fn read_counts(
    meta: &ClusterMeta,
    state: &UpgradeState,
    components: &[UpgradeComponent],
) -> Result<Counts, Error> {
    // the pauses the history already holds
    let held = paused(state);
    let mut found = Counts::default();
    for component in components {
        let name = component.deployment_name();
        // a component already paused keeps its first count
        if let Some(count) = held.get(name) {
            found.counts.insert(name.to_owned(), *count);
            continue;
        }
        // a component with no Deployment isn't running, so there is nothing to pause
        let Some(deployment) = get(meta, name).await? else {
            continue;
        };
        let live = deployment
            .spec
            .as_ref()
            .and_then(|spec| spec.replicas)
            .unwrap_or(1);
        // a count of zero with no recorded pause was lost, so the spec's count stands in
        if live == 0 {
            let spec = declared(meta, *component).unwrap_or(1);
            found.counts.insert(name.to_owned(), spec);
            found.from_spec.push(name.to_owned());
        } else {
            found.counts.insert(name.to_owned(), live);
        }
    }
    Ok(found)
}

/// Scale the paused components to zero
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `names` - The Deployments to scale down
pub async fn scale_down(meta: &ClusterMeta, names: &[&str]) -> Result<(), Error> {
    // scale each Deployment that isn't at zero already
    for name in names.iter().copied() {
        let Some(deployment) = get(meta, name).await? else {
            continue;
        };
        let spec = deployment
            .spec
            .as_ref()
            .and_then(|spec| spec.replicas)
            .unwrap_or(1);
        if spec != 0 {
            scale(meta, name, 0).await?;
        }
    }
    Ok(())
}

/// Check that the paused components' pods are gone
///
/// Returns what is still running, or nothing once every Deployment has no replicas left.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `names` - The Deployments that were scaled down
pub async fn drained(meta: &ClusterMeta, names: &[&str]) -> Result<Option<String>, Error> {
    // a Deployment still reporting replicas still has pods that may write
    let mut running = Vec::new();
    for name in names.iter().copied() {
        let Some(deployment) = get(meta, name).await? else {
            continue;
        };
        let live = deployment
            .status
            .as_ref()
            .and_then(|status| status.replicas)
            .unwrap_or_default();
        if live > 0 {
            running.push(format!("{name} ({live})"));
        }
    }
    if running.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "Waiting for the paused components' pods to stop: {}",
            running.join(", ")
        )))
    }
}

/// Get the pauses the recorded state holds now, for the Deployments an apply is about to patch
///
/// A state that can't be read pauses nothing, so an apply is never blocked by the pause record.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn held(meta: &ClusterMeta) -> BTreeMap<String, i32> {
    // read the state the upgrade recorded the pauses in
    match state::load(&meta.client, &meta.namespace).await {
        Ok(Some(stored)) => paused(&stored.state),
        _ => BTreeMap::new(),
    }
}

/// Scale a paused component back to its recorded count once its new Deployment is applied, and
/// record that it was restored
///
/// A component that isn't paused is left alone. A recorded count of zero stays zero.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The Deployment that was just applied
pub async fn restore(meta: &ClusterMeta, name: &str) -> Result<(), Error> {
    // only a component the history holds paused is restored
    let Some(mut stored) = state::load(&meta.client, &meta.namespace).await? else {
        return Ok(());
    };
    let Some(count) = paused(&stored.state).get(name).copied() else {
        return Ok(());
    };
    // scale it back, then record that it was, so a crash between the two restores it again
    if count > 0 {
        scale(meta, name, count).await?;
    }
    record_restored(meta, &mut stored, name, count).await
}

/// Record that a paused component was restored
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved
/// * `name` - The Deployment that was restored
/// * `count` - The replica count it was restored to
async fn record_restored(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    name: &str,
    count: i32,
) -> Result<(), Error> {
    // the entry names the component and its count, and ends its pause
    let revision = stored.state.revision.to_string();
    stored.state.record(AppliedStep {
        revision,
        step: RESTORE_STEP.to_owned(),
        outcome: StepState::Done,
        message: Some(format!(
            "Scaled the {name} deployment back to {count} replicas"
        )),
        at: Some(chrono::Utc::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        paused: BTreeMap::from([(name.to_owned(), count)]),
        ..AppliedStep::default()
    });
    state::save(&meta.client, &meta.namespace, stored).await
}
