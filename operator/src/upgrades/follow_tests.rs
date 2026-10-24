//! Tests of the operator following the record when a restore or a state reset moves it to an older
//! revision, against the in-process fake kube API

use kube::runtime::controller::Action;
use serde_json::json;
use std::time::Duration;
use thorium::models::schema::{DeploymentRecord, RecordStatus};
use thorium::models::upgrades::{
    AppliedStep, RESET_STATE_STEP, RESTORED_STEP, StepState, UpgradeState, latest,
};

use super::drift::{self, DRIFT_PURPOSE, FOLLOW_REQUEUE_SECS};
use super::handover::HANDED_OVER_MARKER;
use super::record::RUNNING_REQUEUE_SECS;
use super::record_tests::{cluster, entry, fake, mirrored, record, report, status_job, written};
use super::{
    Gate, gate,
    tests::{status_patches, tracked},
};
use crate::k8s::clusters::tests::meta_for;
use crate::k8s::crds::ThoriumCluster;

/// The revision the record is rewound to
const OLDER: &str = "2026-10-v02";

/// The steps of the latest revision, which the state holds finished
const LATEST_STEPS: [&str; 2] = ["schema-apply", "users-settings-theme"];

/// Build the finished history of the latest revision's steps
fn finished_history() -> Vec<AppliedStep> {
    // each step of the latest revision done, a few seconds apart
    let latest = latest().to_string();
    LATEST_STEPS
        .iter()
        .zip(10..)
        .map(|(step, seconds)| entry(&latest, step, StepState::Done, seconds))
        .collect()
}

/// Build a record at the older revision whose history is the finished steps and then one entry
///
/// # Arguments
///
/// * `step` - The step of the extra entry, `reset-state` or `restored`
/// * `seconds` - When the extra entry was written, in seconds after the tests' start
fn rewound(step: &str, seconds: i64) -> DeploymentRecord {
    // the finished steps, then the entry that lowered the revision
    let mut history = finished_history();
    history.push(entry(OLDER, step, StepState::Done, seconds));
    record(OLDER, history, &[])
}

/// Build the cluster, targeting the latest revision, and a state at the latest revision with every
/// step done
fn current() -> (ThoriumCluster, UpgradeState) {
    // the state is the mirror of a deployment that finished the latest revision
    let cluster = cluster(json!({"target_revision": latest().to_string()}));
    (cluster, mirrored(&latest().to_string(), finished_history()))
}

/// Run a drift check that reads a record, returning what the state held afterward and when the
/// check asked to be reconciled again
///
/// # Arguments
///
/// * `cluster` - The cluster being checked
/// * `state` - The state the operator holds
/// * `held` - The record the status Job returns
async fn checked_requeue(
    cluster: &ThoriumCluster,
    state: &UpgradeState,
    held: DeploymentRecord,
) -> (UpgradeState, Option<Action>) {
    // a finished drift status Job holding the record
    let finished = report(
        RecordStatus::Recorded {
            record: held,
            drift: None,
        },
        None,
    );
    let jobs = [status_job(DRIFT_PURPOSE, Some("Complete"))];
    let fake = fake(cluster, Some(state), &jobs, Some(&finished));
    let action = drift::check(&meta_for(cluster.clone(), &fake.client())).await;
    (
        written(&fake).expect("the read was noted in the state"),
        action,
    )
}

/// Run a drift check that reads a record, returning what the state held afterward
///
/// # Arguments
///
/// * `cluster` - The cluster being checked
/// * `state` - The state the operator holds
/// * `held` - The record the status Job returns
async fn checked(
    cluster: &ThoriumCluster,
    state: &UpgradeState,
    held: DeploymentRecord,
) -> UpgradeState {
    // the state is what matters here, not when the check asks to come back
    checked_requeue(cluster, state, held).await.0
}

/// Gate a cluster whose state `ConfigMap` holds a state
///
/// # Arguments
///
/// * `cluster` - The cluster to gate
/// * `state` - The state the `ConfigMap` holds
async fn gated(cluster: &ThoriumCluster, state: &UpgradeState) -> Gate {
    // the gate decides from the `ConfigMap` alone
    let fake = fake(cluster, Some(state), &[], None);
    gate(&fake.client(), &tracked(cluster)).await.expect("gate")
}

/// Check that a gate plans the latest revision's steps again
///
/// # Arguments
///
/// * `gate` - What the gate decided
fn assert_replans(gate: Gate) {
    // an upgrade whose plan holds every step of the latest revision
    let Gate::Upgrade(pending) = gate else {
        panic!("the gate should plan an upgrade");
    };
    let ids = pending
        .steps
        .iter()
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    for step in LATEST_STEPS {
        assert!(ids.contains(&step), "{step} is not planned in {ids:?}");
    }
}

/// A state that finished the latest revision is current before any record is read
#[tokio::test]
async fn a_finished_state_is_current() {
    // nothing in the state calls for an upgrade
    let (cluster, state) = current();
    assert!(matches!(gated(&cluster, &state).await, Gate::Current));
}

/// A record rewound by `reset-state` is followed: the state adopts its revision and history, and
/// the next reconcile plans the latest revision's steps again
#[tokio::test]
async fn a_reset_state_entry_in_the_record_moves_the_state_back() {
    // the record names the older revision and a `reset-state` entry newer than the state's last
    let (cluster, state) = current();
    let noted = checked(&cluster, &state, rewound(RESET_STATE_STEP, 20)).await;
    // the state follows the record
    assert_eq!(noted.revision.to_string(), OLDER);
    assert!(
        noted
            .applied
            .iter()
            .any(|applied| applied.step == RESET_STATE_STEP)
    );
    // and the next gate plans the steps again
    assert_replans(gated(&cluster, &noted).await);
}

/// A record rewound by a restore is followed the same way
#[tokio::test]
async fn a_restored_entry_in_the_record_moves_the_state_back() {
    // the record names the older revision and a `restored` entry newer than the state's last
    let (cluster, state) = current();
    let noted = checked(&cluster, &state, rewound(RESTORED_STEP, 20)).await;
    assert_eq!(noted.revision.to_string(), OLDER);
    assert_replans(gated(&cluster, &noted).await);
}

/// A record at an older revision with no entry that lowered it is a lagging mirror, never a
/// rewind: the state keeps its revision
#[tokio::test]
async fn a_lower_record_without_an_entry_never_lowers_the_state() {
    // the record's history is the finished steps alone
    let (cluster, state) = current();
    let noted = checked(&cluster, &state, record(OLDER, finished_history(), &[])).await;
    assert_eq!(noted.revision, latest());
    assert!(matches!(gated(&cluster, &noted).await, Gate::Current));
}

/// A `reset-state` entry older than the state's last entry was already followed, or undone by what
/// the operator did since, so it is ignored
#[tokio::test]
async fn a_reset_state_entry_older_than_the_state_is_ignored() {
    // the entry predates the state's finished steps
    let (cluster, state) = current();
    let noted = checked(&cluster, &state, rewound(RESET_STATE_STEP, 5)).await;
    assert_eq!(noted.revision, latest());
    assert!(matches!(gated(&cluster, &noted).await, Gate::Current));
}

/// A record rewound by a restore makes the check ask to be reconciled again within seconds, since
/// the plan that runs the steps again is made by the next reconcile, which a ready cluster would
/// otherwise reach only after its day-long wait
#[tokio::test]
async fn following_the_record_asks_to_be_reconciled_again_soon() {
    // the record names the older revision and a `restored` entry newer than the state's last
    let (cluster, state) = current();
    let (noted, action) = checked_requeue(&cluster, &state, rewound(RESTORED_STEP, 20)).await;
    assert_eq!(noted.revision.to_string(), OLDER);
    assert_eq!(
        action,
        Some(Action::requeue(Duration::from_secs(FOLLOW_REQUEUE_SECS)))
    );
}

/// A record that was not rewound leaves the reconcile cadence alone
#[tokio::test]
async fn a_read_that_finds_nothing_to_follow_asks_for_nothing() {
    // the record's history is the finished steps alone
    let (cluster, state) = current();
    let (_, action) =
        checked_requeue(&cluster, &state, record(OLDER, finished_history(), &[])).await;
    assert_eq!(action, None);
}

/// A check that starts a status Job asks to be reconciled again when the Job is likely done, since
/// a finished Job wakes no reconcile and a ready cluster would otherwise wait a day to read it
#[tokio::test]
async fn starting_a_read_asks_to_be_reconciled_again_soon() {
    // no status Job exists yet, so the check creates one
    let (cluster, state) = current();
    let fake = fake(&cluster, Some(&state), &[], None);
    let action = drift::check(&meta_for(cluster.clone(), &fake.client())).await;
    assert_eq!(
        action,
        Some(Action::requeue(Duration::from_secs(RUNNING_REQUEUE_SECS)))
    );
}

/// Build a state at the latest revision that still carries the hand-over marker of an abandoned
/// downgrade to the older revision
fn stale_marked() -> UpgradeState {
    // a finished latest revision, with the marker an abandoned downgrade left behind
    let (_, mut state) = current();
    state
        .schema
        .insert(HANDED_OVER_MARKER.to_owned(), OLDER.to_owned());
    state
}

/// A gate at the latest revision removes a stale hand-over marker and saves the state, so a later
/// restore to the marker's revision isn't taken for a hand-over
#[tokio::test]
async fn a_gate_removes_a_stale_hand_over_marker() {
    // a cluster with no target at the latest revision, whose state carries a stale marker
    let cluster = cluster(json!({}));
    let fake = fake(&cluster, Some(&stale_marked()), &[], None);
    let gated = gate(&fake.client(), &tracked(&cluster))
        .await
        .expect("gate");
    assert!(matches!(gated, Gate::Current));
    // the saved state no longer carries the marker
    let saved = written(&fake).expect("the state was saved");
    assert_eq!(saved.schema.get(HANDED_OVER_MARKER), None);
}

/// A restore followed before any gate removed a stale hand-over marker drops the marker, so the
/// cluster at the marker's revision is held for a target instead of being handed over
#[tokio::test]
async fn following_a_restore_drops_a_stale_hand_over_marker() {
    // the record is restored to the older revision the abandoned downgrade targeted
    let cluster = cluster(json!({}));
    let noted = checked(&cluster, &stale_marked(), rewound(RESTORED_STEP, 20)).await;
    assert_eq!(noted.revision.to_string(), OLDER);
    assert_eq!(noted.schema.get(HANDED_OVER_MARKER), None);
    // the next gate asks for a target and says nothing of a hand-over
    let fake = fake(&cluster, Some(&noted), &[], None);
    let gated = gate(&fake.client(), &tracked(&cluster))
        .await
        .expect("gate");
    assert!(matches!(gated, Gate::Hold(_)));
    let messages = status_patches(&fake)
        .iter()
        .filter_map(|status| status["message"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .all(|message| !message.contains("handed over")),
        "{messages:?}"
    );
}
