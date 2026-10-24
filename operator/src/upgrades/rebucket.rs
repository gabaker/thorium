//! The re-bucket steps the operator plans when the schema-apply Job refuses a changed partition size
//!
//! A partition size changed while rows of the data type exist moves where those rows belong, so
//! `thoradm schema apply` refuses it. The operator then plans the synthetic step `rebucket-<data
//! type>`: a data step that pauses the API and every component that writes Scylla, runs
//! `thoradm migrate run --id rebucket-<data type>` as a migration Job after a check and an
//! approval naming a backup, and lets the schema apply run again once the rows have moved. A
//! refused `earliest` value or files password plans no step; the status names the setting.

use kube::runtime::controller::Action;
use std::time::Duration;
use thorium::Error;
use thorium::models::schema::setting_id;
use thorium::models::upgrades::{
    PlannedStep, RevisionId, StepDef, StepKind, StepState, UpgradeComponent,
};

use super::migration::{self, Advance};
use super::state::{self, StoredState};
use super::{Pending, progress};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds::{self, ClusterPhase};

/// The marker in the upgrade state holding the attempt counter below which a failed schema apply
/// no longer holds the rollout
pub(super) const ATTEMPT_FLOOR_MARKER: &str = "apply_attempt_floor";

/// How long in seconds to wait before looking at a re-bucket step again
const REQUEUE_SECS: u64 = 15;

/// How long in seconds to wait before looking at a re-bucket step that waits for an admin
const BLOCKED_REQUEUE_SECS: u64 = 60;

/// The components that write Scylla, which no step that moves its rows may run beside
const WRITERS: &[UpgradeComponent] = &[
    UpgradeComponent::Api,
    UpgradeComponent::Scaler,
    UpgradeComponent::BaremetalScaler,
    UpgradeComponent::EventHandler,
];

/// Build the step that moves one data type's rows
///
/// # Arguments
///
/// * `id` - The step's id, which is also its migration's
/// * `description` - What the step moves
const fn step(id: &'static str, description: &'static str) -> StepDef {
    // a data step that pauses every writer and runs the migration of the same name
    StepDef {
        id,
        kind: StepKind::Data,
        description,
        pause: WRITERS,
        thoradm_migration: Some(id),
        manual_procedure: Some(
            "Run `thoradm migrate run --id <step>` against the cluster's config with the API and \
             every Scylla writer scaled to zero.",
        ),
        ..StepDef::BASE
    }
}

/// The re-bucket steps, one per data type whose rows a partition size places
pub(super) const STEPS: &[StepDef] = &[
    step(
        "rebucket-files",
        "Move files to the buckets the configured partition size gives",
    ),
    step(
        "rebucket-repos",
        "Move repos to the buckets the configured partition size gives",
    ),
    step(
        "rebucket-results",
        "Move results to the buckets the configured partition size gives",
    ),
    step(
        "rebucket-entities",
        "Move entities to the buckets the configured partition size gives",
    ),
    step(
        "rebucket-associations",
        "Move associations to the buckets the configured partition size gives",
    ),
    step(
        "rebucket-tags",
        "Move tags to the buckets the configured partition size gives",
    ),
];

/// Find the re-bucket step a refused setting calls for, if it is a partition size
///
/// # Arguments
///
/// * `setting` - The refused setting as a config path, such as `thorium.files.partition_size`
fn step_for(setting: &str) -> Option<&'static StepDef> {
    // a partition size's record id is `partition_size.<data type>`, and every tag type moves together
    let id = setting_id(setting);
    let data_type = id.strip_prefix("partition_size.")?;
    let name = if data_type.starts_with("tags.") {
        "rebucket-tags".to_owned()
    } else {
        format!("rebucket-{data_type}")
    };
    STEPS.iter().find(|step| step.id == name)
}

/// Find the step the refused settings call for first
///
/// # Arguments
///
/// * `refused` - The refused settings as config paths
fn first_step(refused: &[String]) -> Option<&'static StepDef> {
    // the first partition size among them; the apply names the rest again once it moved
    refused.iter().find_map(|setting| step_for(setting))
}

/// Plan and drive the re-bucket step a refused partition size calls for
///
/// Returns the action to take while the step waits for its check, its approval, or its Job, or
/// nothing when the refusal calls for no step. Once the step has run, the failed apply stops
/// holding the rollout and the next attempt runs soon.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which the step's records are saved in
/// * `revision` - The revision the cluster is at
/// * `refused` - The settings the schema apply refused, as config paths
pub(super) async fn drive(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    revision: &RevisionId,
    refused: &[String],
) -> Result<Option<Action>, Error> {
    // only a refused partition size plans a step
    let Some(def) = first_step(refused) else {
        return Ok(None);
    };
    // the step runs as an upgrade of one step would, over the state the apply holds
    let planned = PlannedStep {
        revision: revision.to_string(),
        step: def.id.to_owned(),
        kind: def.kind,
        description: def.description.to_owned(),
        pause: def.pause.to_vec(),
        state: StepState::Pending,
        message: None,
        phase: None,
        prerelease: false,
        reverse: false,
        pass: None,
        blocked: None,
        loss: None,
    };
    let mut pending = Pending {
        stored: stored.clone(),
        target: revision.clone(),
        steps: vec![planned],
        approvals: meta.cluster.spec.upgrade.approvals.clone(),
        requeue_hint: None,
    };
    let advanced = migration::advance(meta, &mut pending, 0, def).await;
    // keep what the step recorded whatever came of it
    *stored = pending.stored.clone();
    match advanced? {
        // the step waits, so the rollout does too, and the status shows the step
        Advance::Held { state, message } => {
            pending.steps[0].state = state;
            pending.steps[0].message = Some(message.clone());
            let secs = pending
                .requeue_hint
                .unwrap_or(if state == StepState::Blocked {
                    BLOCKED_REQUEUE_SECS
                } else {
                    REQUEUE_SECS
                });
            let text = format!(
                "The schema apply refused the partition size, so step {} moves the rows before \
                 any component rolls: {message}",
                def.id
            );
            println!("{text}");
            crds::set_upgrade_status(
                &meta.client,
                &meta.status,
                ClusterPhase::Upgrading,
                Some(text),
                progress(revision, Some(revision), pending.steps.clone()),
            )
            .await;
            Ok(Some(Action::requeue(Duration::from_secs(secs))))
        }
        // the rows are in their buckets (or none needed moving), so the apply runs again
        Advance::Skipped(_) => {
            release(meta, stored).await?;
            Ok(Some(Action::requeue(Duration::from_secs(5))))
        }
        Advance::Finished(entry) => {
            stored.state.record(*entry);
            release(meta, stored).await?;
            Ok(Some(Action::requeue(Duration::from_secs(5))))
        }
    }
}

/// Let the schema apply run again: end the held failure and put the next attempt past the one that
/// failed
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved
async fn release(meta: &ClusterMeta, stored: &mut StoredState) -> Result<(), Error> {
    // the failed attempt's counter, so the next one is named past it
    let floor = stored
        .state
        .schema
        .remove(thorium::models::upgrades::HELD_FAILURE_MARKER)
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|held| held.get("attempt").and_then(serde_json::Value::as_u64))
        .map_or(1, |attempt| attempt + 1);
    stored
        .state
        .schema
        .insert(ATTEMPT_FLOOR_MARKER.to_owned(), floor.to_string());
    state::save(&meta.client, &meta.namespace, stored).await
}
