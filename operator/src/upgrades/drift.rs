//! Schema drift in the upgrade: reading it through thoradm, reporting it in the status, and
//! blocking the steps that overlap it
//!
//! The operator never probes a backend itself: a `thoradm schema status` Job returns the capped
//! drift summary, with the spec's acknowledgements applied. The summary is noted in the upgrade
//! state's markers so the steps of an upgrade can be blocked by it from one reconcile to the next,
//! and shown in `status.upgrade.schema`, `status.upgrade.drift`, and the `SchemaDrift` condition.
//! Drift never holds a normal reconcile.

use chrono::{DateTime, Duration, Utc};
use kube::runtime::controller::Action;
use sha2::{Digest, Sha256};
use thorium::Error;
use thorium::models::schema::RecordStatus;
use thorium::models::upgrades::{
    APPLY_KEY_MARKER, DriftSummary, RETRY_VALUE_MARKER, StepDef, StepKind, UpgradeConditionType,
    UpgradeState,
};

use super::record::{self, Read};
use super::state;
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds;

/// Why the record is read for a drift check outside an upgrade
pub const DRIFT_PURPOSE: &str = "drift";

/// Why the record is read to plan an upgrade, which also reports drift
const PLAN_PURPOSE: &str = "plan";

/// The key in the state's schema markers holding the drift summary the last read reported, as JSON
const DRIFT_MARKER: &str = "drift";

/// The key in the state's schema markers holding when drift was last read, or last tried
const READ_MARKER: &str = "drift_read";

/// The key in the state's schema markers holding the hash of the acknowledgements the last read
/// applied
const ACKS_MARKER: &str = "drift_acks";

/// The key in the state's schema markers holding the schema apply the last read saw, so a schema
/// apply that finished since reads the drift again
const APPLY_MARKER: &str = "drift_apply";

/// The key in the state's schema markers holding the schema apply a drift read saw when its status
/// Job was created (empty when no apply was recorded yet), so a schema apply that finishes while
/// the Job runs still makes the drift due once the read is noted
const STARTED_APPLY_MARKER: &str = "drift_read_apply";

/// How long in seconds a read of the drift stays good before a normal reconcile reads it again
const READ_VALID_SECS: i64 = 3600;

/// How long in seconds to wait before trying a drift read that failed again
const RETRY_SECS: i64 = 300;

/// How soon the record is read again after drift blocked a step, so a fix is seen quickly
const BLOCKED_READ_SECS: i64 = 60;

/// How long in seconds to wait before reconciling again after the state followed a record that a
/// restore or a state reset moved, so the steps it undid are planned again at once
pub(super) const FOLLOW_REQUEUE_SECS: u64 = 5;

/// Whether a status Job read for this purpose probes the live schema
///
/// # Arguments
///
/// * `purpose` - Why the record is read
#[must_use]
pub fn probes(purpose: &str) -> bool {
    // planning an upgrade and checking drift report it; reading the record to recover or move it
    // doesn't need the probe
    purpose == PLAN_PURPOSE || purpose == DRIFT_PURPOSE
}

/// Serialize the spec's drift acknowledgements, as `thoradm schema status --acknowledged` takes them
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub fn acknowledgements(meta: &ClusterMeta) -> Result<String, Error> {
    // the spec's list is already in the form thoradm reads
    serde_json::to_string(&meta.cluster.spec.upgrade.drift_acknowledged)
        .map_err(|error| Error::new(format!("The drift acknowledgements can't be sent: {error}")))
}

/// Hash the acknowledgements a read applied
///
/// # Arguments
///
/// * `acks` - The acknowledgements, as serialized for the Job
fn hash(acks: &str) -> String {
    // the digest as hex
    format!("{:x}", Sha256::digest(acks.as_bytes()))
}

/// Identify the last recorded schema apply: its trigger key, and the retry annotation value it ran
/// with, since an apply retried for the same key changes the live schema too
///
/// # Arguments
///
/// * `state` - The upgrade state holding the apply's markers
fn applied(state: &UpgradeState) -> Option<String> {
    // the key alone, or the key and the retry value when the apply was retried
    let key = state.schema.get(APPLY_KEY_MARKER)?;
    match state.schema.get(RETRY_VALUE_MARKER) {
        Some(retry) => Some(format!("{key}|retry={retry}")),
        None => Some(key.clone()),
    }
}

/// Stamp the state's markers with a read or a try of the drift
///
/// # Arguments
///
/// * `state` - The upgrade state to stamp
/// * `at` - The time to note as the read's
/// * `acks` - The acknowledgements the read applied, as serialized for the Job
fn stamp(state: &mut UpgradeState, at: DateTime<Utc>, acks: &str) {
    // when and with which acknowledgements
    state.schema.insert(READ_MARKER.to_owned(), at.to_rfc3339());
    state.schema.insert(ACKS_MARKER.to_owned(), hash(acks));
    // at the schema apply the read started at, which is older than the current one when an apply
    // finished while its status Job ran; a read noted without a start began at the current apply
    let seen = match state.schema.remove(STARTED_APPLY_MARKER) {
        Some(started) => Some(started).filter(|key| !key.is_empty()),
        None => applied(state),
    };
    match seen {
        Some(key) => state.schema.insert(APPLY_MARKER.to_owned(), key),
        None => state.schema.remove(APPLY_MARKER),
    };
}

/// Note the schema apply a drift read starts at, before its status Job is created
///
/// # Arguments
///
/// * `state` - The upgrade state to note it in
fn start(state: &mut UpgradeState) {
    // the current apply, or an empty one when no apply was recorded yet
    let seen = applied(state).unwrap_or_default();
    state.schema.insert(STARTED_APPLY_MARKER.to_owned(), seen);
}

/// Note what a read of the drift found in the upgrade state
///
/// A read that didn't probe clears what was noted before.
///
/// # Arguments
///
/// * `state` - The upgrade state to note it in
/// * `summary` - What the status Job reported, if it probed
/// * `acks` - The acknowledgements the Job applied, as serialized for it
pub fn note(state: &mut UpgradeState, summary: Option<&DriftSummary>, acks: &str) {
    // keep the summary, or drop the old one when this read has none
    match summary.and_then(|summary| serde_json::to_string(summary).ok()) {
        Some(json) => state.schema.insert(DRIFT_MARKER.to_owned(), json),
        None => state.schema.remove(DRIFT_MARKER),
    };
    stamp(state, Utc::now(), acks);
}

/// Note a read that failed or found nothing to compare, so it is tried again after a while
///
/// # Arguments
///
/// * `state` - The upgrade state to note it in
/// * `acks` - The acknowledgements in the spec, as serialized for the Job
fn note_attempt(state: &mut UpgradeState, acks: &str) {
    // date the try so the read is due again after the retry delay rather than a full interval
    let at = Utc::now() - Duration::seconds(READ_VALID_SECS - RETRY_SECS);
    stamp(state, at, acks);
}

/// Read the drift summary the state last noted
///
/// # Arguments
///
/// * `state` - The upgrade state holding the last read
#[must_use]
pub fn noted(state: &UpgradeState) -> Option<DriftSummary> {
    // a summary that doesn't parse is the same as none
    state
        .schema
        .get(DRIFT_MARKER)
        .and_then(|json| serde_json::from_str(json).ok())
}

/// Whether the acknowledgements changed since the drift was last read
///
/// # Arguments
///
/// * `state` - The upgrade state holding the last read
/// * `acks` - The acknowledgements in the spec now, as serialized for the Job
#[must_use]
pub fn acknowledgements_changed(state: &UpgradeState, acks: &str) -> bool {
    // a state that never noted acknowledgements has nothing to differ from
    state
        .schema
        .get(ACKS_MARKER)
        .is_some_and(|noted| *noted != hash(acks))
}

/// Whether the drift is due to be read: never read, read too long ago, read with other
/// acknowledgements, or read before the last schema apply
///
/// # Arguments
///
/// * `state` - The upgrade state holding the last read
/// * `acks` - The acknowledgements in the spec now, as serialized for the Job
fn due(state: &UpgradeState, acks: &str) -> bool {
    // the time of the last read; a future date never holds
    let read = state
        .schema
        .get(READ_MARKER)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc));
    let fresh =
        read.is_some_and(|at| (0..READ_VALID_SECS).contains(&(Utc::now() - at).num_seconds()));
    // a schema apply that finished since the read changes what a read would find
    let since = applied(state).as_ref() != state.schema.get(APPLY_MARKER);
    !fresh || since || acknowledgements_changed(state, acks)
}

/// Explain why a step is blocked by the drift the state last noted, if it is
///
/// # Arguments
///
/// * `state` - The upgrade state holding the last read
/// * `def` - The step
#[must_use]
pub fn block(state: &UpgradeState, def: &StepDef) -> Option<String> {
    // a step that targets nothing can't overlap anything
    if def.targets.is_empty() {
        return None;
    }
    // a data step that runs a migration converts the layouts of its targets, which the schema apply
    // before it already recorded, so those objects read as changed until it finishes
    let converts = def.kind == StepKind::Data && def.thoradm_migration.is_some();
    // block on the noted objects that overlap its targets
    noted(state)?.block_message_for(def.id, def.targets, converts)
}

/// Explain why a step is blocked by the drift noted, and make the record's next read due soon
///
/// The drift is read along with the record, so a block would otherwise last until the next read
/// is due even when an admin fixed the object right away.
///
/// # Arguments
///
/// * `state` - The upgrade state holding the last read, whose read is moved when the step blocks
/// * `def` - The step
///
/// # Returns
///
/// The explanation and whether the state changed and must be saved, when the step is blocked.
#[must_use]
pub fn block_soon(state: &mut UpgradeState, def: &StepDef) -> Option<(String, bool)> {
    // the explanation, if the drift blocks the step
    let message = block(state, def)?;
    // read again within a minute, as a failed step does
    let changed = record::read_again_within(state, BLOCKED_READ_SECS);
    Some((message, changed))
}

/// Report the drift in the cluster's upgrade status and its `SchemaDrift` condition
///
/// A read that didn't probe leaves the status as it was.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `summary` - What the status Job reported, if it probed
pub async fn publish(meta: &ClusterMeta, summary: Option<&DriftSummary>) {
    // nothing was read, so nothing changes
    let Some(summary) = summary else {
        return;
    };
    // the blockers only serve the step block, so the status shows the capped summary without them
    let shown = DriftSummary {
        blockers: Vec::new(),
        blockers_truncated: false,
        ..summary.clone()
    };
    // each backend's status and the capped summary, then the condition
    crds::set_upgrade_drift(&meta.client, &meta.status, &shown).await;
    crds::set_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::SchemaDrift,
        summary.holds(),
        summary.describe(),
    )
    .await;
}

/// Read the drift when it is due and report it, without ever holding the cluster
///
/// A failure is logged and tried again after a while.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
///
/// # Returns
///
/// When to reconcile again, if sooner than the cluster's usual wait.
pub async fn check(meta: &ClusterMeta) -> Option<Action> {
    // drift is advice: whatever goes wrong here never fails the reconcile
    match try_check(meta).await {
        Ok(again) => again,
        Err(error) => {
            println!(
                "Warning: reading the schema drift of {} failed: {error}",
                meta.name
            );
            None
        }
    }
}

/// Read the drift when it is due and report it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn try_check(meta: &ClusterMeta) -> Result<Option<Action>, Error> {
    // without a recorded state there is no deployment to compare yet
    let Some(mut stored) = state::load(&meta.client, &meta.namespace).await? else {
        return Ok(None);
    };
    // nothing to do until a read is due or one is under way
    let acks = acknowledgements(meta)?;
    let reading = record::reading(meta, DRIFT_PURPOSE).await?;
    if !due(&stored.state, &acks) && !reading {
        return Ok(None);
    }
    // a read about to start notes the schema apply it starts at, since a schema apply can finish
    // while its status Job runs and the Job then reports the schema from before that apply
    if !reading {
        start(&mut stored.state);
        state::save(&meta.client, &meta.namespace, &mut stored).await?;
    }
    // read through a status Job, which a later reconcile finds finished
    let read = match record::read_report(meta, DRIFT_PURPOSE, None).await {
        Ok(read) => read,
        Err(error) => {
            note_attempt(&mut stored.state, &acks);
            state::save(&meta.client, &meta.namespace, &mut stored).await?;
            return Err(error);
        }
    };
    match read {
        // a finished status Job wakes no reconcile, so the reconcile that reads it is asked for
        Read::Wait(again) => Ok(Some(again)),
        // a failed Job is tried again after a while
        Read::Failed(reason) => {
            note_attempt(&mut stored.state, &acks);
            state::save(&meta.client, &meta.namespace, &mut stored).await?;
            Err(Error::new(format!("the status Job failed: {reason}")))
        }
        Read::Report(report) => match report.status {
            // note what was found and show it
            RecordStatus::Recorded { record, drift } => {
                // a restore or a state reset moved the record, so the state follows it
                let followed = record::rewound(&record, &stored.state);
                if followed {
                    println!(
                        "The record of {} was moved to revision {} by a restore or a state \
                         reset; following it",
                        meta.namespace, record.revision
                    );
                    record::follow(&mut stored.state, &record);
                }
                note(&mut stored.state, drift.as_ref(), &acks);
                state::save(&meta.client, &meta.namespace, &mut stored).await?;
                publish(meta, drift.as_ref()).await;
                // the steps the record undid are planned by the next reconcile, which is asked for
                Ok(followed
                    .then(|| Action::requeue(std::time::Duration::from_secs(FOLLOW_REQUEUE_SECS))))
            }
            // no record to compare with yet: try again after a while
            _ => {
                note_attempt(&mut stored.state, &acks);
                state::save(&meta.client, &meta.namespace, &mut stored).await?;
                Ok(None)
            }
        },
    }
}
