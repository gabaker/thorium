//! Tests of the operator's use of schema drift: the status Job's drift summary reaching the
//! status, the acknowledgements reaching the Job, the reads' cadence, and a step blocked by drift
//! in the objects it targets, all against the in-process fake kube API

use kube::runtime::controller::Action;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use thorium::Error;
use thorium::models::schema::{
    Acknowledgement, BackendDrift, DriftReport, ObjectDrift, RecordStatus,
};
use thorium::models::upgrades::{
    APPLY_KEY_MARKER, BackendSchemaStatus, CatalogEntry, DriftEntry, DriftKind, DriftSummary,
    RECORD_MARKER, RETRY_VALUE_MARKER, RevisionId, SchemaBackend, SchemaTarget, StepDef, StepState,
    UpgradeState, latest,
};

use super::drift::{self, DRIFT_PURPOSE};
use super::record::{refresh, status_spec};
use super::record_tests::{
    cluster, fake, mirrored, pending_of, print, record, report, status_job, written,
};
use super::schema_apply::tests::created_jobs;
use super::steps::fixture::{self, Check};
use super::tests::{
    LocalRecord, finish, first_helm_fixture, pending_upgrade, rev, saved_state, status_patches,
};
use super::{BASELINE_REVISION, BLOCKED_REQUEUE_SECS, Moved, Pending, RecordOps, run_with};
use crate::k8s::clusters::tests::meta_for;

/// The fixture step the tests of a run plan in the first Helm revision, checked in process
const STEP: &str = fixture::STEP;

/// Build a record status with a drift summary
///
/// # Arguments
///
/// * `drift` - The summary the status Job reports
fn recorded_with(drift: Option<DriftSummary>) -> RecordStatus {
    // a record at the latest revision
    RecordStatus::Recorded {
        record: record(&latest().to_string(), Vec::new(), &[]),
        drift,
    }
}

/// Build a drift summary with one object of each kind: a missing and an unexpected table on
/// Scylla and a changed index on Elastic
fn three_kinds() -> DriftSummary {
    // the counts and objects the status Job reports for those three
    DriftSummary {
        missing: 1,
        changed: 1,
        unexpected: 1,
        objects: vec![
            DriftEntry {
                backend: SchemaBackend::Scylla,
                object: "nodes".to_owned(),
                kind: DriftKind::Missing,
                fingerprint: None,
                lapsed: false,
            },
            DriftEntry {
                backend: SchemaBackend::Scylla,
                object: "extra".to_owned(),
                kind: DriftKind::Unexpected,
                fingerprint: Some("81aa".to_owned()),
                lapsed: false,
            },
            DriftEntry {
                backend: SchemaBackend::Elastic,
                object: "SampleTags.group".to_owned(),
                kind: DriftKind::Changed,
                fingerprint: Some("c03e".to_owned()),
                lapsed: false,
            },
        ],
        backends: BTreeMap::from([
            (SchemaBackend::Scylla, BackendSchemaStatus::Drift),
            (SchemaBackend::Elastic, BackendSchemaStatus::Drift),
            (SchemaBackend::Redis, BackendSchemaStatus::Ok),
            (SchemaBackend::S3, BackendSchemaStatus::Ok),
        ]),
        ..DriftSummary::default()
    }
}

/// Get the arguments of the first Job a fake kube API was asked to create
///
/// # Arguments
///
/// * `created` - The Jobs created
fn job_args(created: &[Value]) -> Vec<String> {
    // the container's arguments
    created[0]["spec"]["template"]["spec"]["containers"][0]["args"]
        .as_array()
        .expect("args")
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

/// Drift renders `status.upgrade.schema` per backend and a `SchemaDrift` condition whose message
/// counts each kind, the state notes what was read, and the check holds nothing: no phase is
/// written, so a normal reconcile completes
#[tokio::test]
async fn drift_reaches_the_status_and_the_reconcile_completes() {
    // a current cluster whose drift status Job finished with one object of each kind
    let cluster = cluster(json!({}));
    let state = mirrored(&latest().to_string(), Vec::new());
    let finished = report(recorded_with(Some(three_kinds())), None);
    let jobs = [status_job(DRIFT_PURPOSE, Some("Complete"))];
    let fake = fake(&cluster, Some(&state), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake.client());
    // checking reads the Job and reports it
    drift::check(&meta).await;
    let patches = status_patches(&fake);
    let last = patches
        .iter()
        .rev()
        .find(|patch| patch["upgrade"]["schema"].is_object())
        .expect("the schema status was patched");
    assert_eq!(
        last["upgrade"]["schema"],
        json!({"scylla": "drift", "elastic": "drift", "redis": "ok", "s3": "ok"})
    );
    let counts = &last["upgrade"]["drift"];
    assert_eq!(
        (
            counts["missing"].as_u64(),
            counts["changed"].as_u64(),
            counts["unexpected"].as_u64()
        ),
        (Some(1), Some(1), Some(1))
    );
    assert_eq!(counts["objects"].as_array().map(Vec::len), Some(3));
    // the condition says how many of each kind
    let conditions = patches
        .iter()
        .rev()
        .find_map(|patch| patch["upgrade"]["conditions"].as_array())
        .expect("the conditions were patched");
    let condition = conditions
        .iter()
        .find(|condition| condition["type"] == "SchemaDrift")
        .expect("the SchemaDrift condition is set");
    assert_eq!(condition["status"], "True");
    let message = condition["message"].as_str().unwrap_or_default();
    for part in [
        "1 missing",
        "1 changed",
        "1 unexpected",
        "scylla",
        "elastic",
    ] {
        assert!(message.contains(part), "{part} is missing from {message}");
    }
    // the state notes the summary, and the Job that was read is deleted
    let noted = written(&fake).expect("the read was noted in the state");
    assert_eq!(drift::noted(&noted), Some(three_kinds()));
    assert!(
        fake.writes()
            .iter()
            .any(|request| request.method == "DELETE"),
        "{:?}",
        fake.writes()
    );
    // the check wrote no phase, so the reconcile that called it goes on to deploy
    assert!(
        patches.iter().all(|patch| patch.get("phase").is_none()),
        "{patches:?}"
    );
}

/// Once drift is read it isn't read again for an hour, and a change to the spec's acknowledgements
/// reads it again at once
#[tokio::test]
async fn a_recent_drift_read_is_repeated_only_when_the_acknowledgements_change() {
    // the drift was just read with no acknowledgements
    let quiet = cluster(json!({}));
    let mut state = mirrored(&latest().to_string(), Vec::new());
    let meta_quiet = meta_for(
        quiet.clone(),
        &fake(&quiet, Some(&state), &[], None).client(),
    );
    drift::note(
        &mut state,
        Some(&three_kinds()),
        &drift::acknowledgements(&meta_quiet).unwrap(),
    );
    // checking again creates no Job
    let fake_one = fake(&quiet, Some(&state), &[], None);
    drift::check(&meta_for(quiet.clone(), &fake_one.client())).await;
    assert_eq!(created_jobs(&fake_one), Vec::<Value>::new());
    assert!(fake_one.writes().is_empty(), "{:?}", fake_one.writes());
    // an acknowledgement added to the spec starts a read right away
    let acknowledged = cluster(json!({
        "drift_acknowledged": [{"backend": "scylla", "object": "extra"}]
    }));
    let fake_two = fake(&acknowledged, Some(&state), &[], None);
    drift::check(&meta_for(acknowledged.clone(), &fake_two.client())).await;
    assert_eq!(created_jobs(&fake_two).len(), 1);
    // a read that is due, with no recent one noted, starts one too
    let fresh = mirrored(&latest().to_string(), Vec::new());
    let fake_three = fake(&quiet, Some(&fresh), &[], None);
    drift::check(&meta_for(quiet.clone(), &fake_three.client())).await;
    assert_eq!(created_jobs(&fake_three).len(), 1);
}

/// A drift read whose status Job started before a schema apply finished reports the schema as it
/// was before the apply, so noting it doesn't count as a read after that apply: the next check
/// reads the drift again rather than keeping the old report for an hour
#[tokio::test]
async fn a_drift_read_begun_before_an_apply_finished_is_read_again() {
    // a cluster whose drift was never read, at the first schema apply's key
    let cluster = cluster(json!({}));
    let mut state = mirrored(&latest().to_string(), Vec::new());
    state
        .schema
        .insert(APPLY_KEY_MARKER.to_owned(), "apply-1".to_owned());
    // the check starts a status Job
    let fake_one = fake(&cluster, Some(&state), &[], None);
    drift::check(&meta_for(cluster.clone(), &fake_one.client())).await;
    assert_eq!(created_jobs(&fake_one).len(), 1);
    // while the Job runs, a schema apply finishes and records its key
    let mut applied = written(&fake_one).unwrap_or(state);
    applied
        .schema
        .insert(APPLY_KEY_MARKER.to_owned(), "apply-2".to_owned());
    // the Job finishes with what it read before the apply, and the check notes it
    let finished = report(recorded_with(Some(three_kinds())), None);
    let jobs = [status_job(DRIFT_PURPOSE, Some("Complete"))];
    let fake_two = fake(&cluster, Some(&applied), &jobs, Some(&finished));
    drift::check(&meta_for(cluster.clone(), &fake_two.client())).await;
    let noted = written(&fake_two).expect("the read was noted in the state");
    // the next check reads the drift again, since that read began before the apply finished
    let fake_three = fake(&cluster, Some(&noted), &[], None);
    drift::check(&meta_for(cluster.clone(), &fake_three.client())).await;
    assert_eq!(created_jobs(&fake_three).len(), 1);
}

/// A schema apply run again for the same trigger key by the retry annotation changes what a read
/// would find just like a new key does, so the drift is due once the retried apply is recorded
#[tokio::test]
async fn a_retried_apply_reads_the_drift_again() {
    // the drift was just read at the current apply
    let quiet = cluster(json!({}));
    let mut state = mirrored(&latest().to_string(), Vec::new());
    state
        .schema
        .insert(APPLY_KEY_MARKER.to_owned(), "apply-1".to_owned());
    let meta = meta_for(
        quiet.clone(),
        &fake(&quiet, Some(&state), &[], None).client(),
    );
    drift::note(
        &mut state,
        Some(&three_kinds()),
        &drift::acknowledgements(&meta).unwrap(),
    );
    // the apply runs again for the same key under a retry annotation and records its value
    state
        .schema
        .insert(RETRY_VALUE_MARKER.to_owned(), "retry-1".to_owned());
    // the next check reads the drift again
    let fake_one = fake(&quiet, Some(&state), &[], None);
    drift::check(&meta_for(quiet.clone(), &fake_one.client())).await;
    assert_eq!(created_jobs(&fake_one).len(), 1);
}

/// A drift read that failed is tried again later rather than at every reconcile, and holds nothing
#[tokio::test]
async fn a_failed_drift_read_is_not_retried_at_every_reconcile() {
    // the drift status Job failed
    let cluster = cluster(json!({}));
    let state = mirrored(&latest().to_string(), Vec::new());
    let jobs = [status_job(DRIFT_PURPOSE, Some("Failed"))];
    let fake_one = fake(&cluster, Some(&state), &jobs, None);
    drift::check(&meta_for(cluster.clone(), &fake_one.client())).await;
    // the failure is noted in the state, so the next reconcile creates no Job
    let noted = written(&fake_one).expect("the attempt was noted in the state");
    assert!(noted.schema.contains_key(RECORD_MARKER));
    let fake_two = fake(&cluster, Some(&noted), &[], None);
    drift::check(&meta_for(cluster.clone(), &fake_two.client())).await;
    assert_eq!(created_jobs(&fake_two), Vec::<Value>::new());
    // and no phase was ever written
    assert!(
        status_patches(&fake_one)
            .iter()
            .all(|patch| patch.get("phase").is_none())
    );
}

/// With 50 unacknowledged and 10 acknowledged drifted objects, 4 of them acknowledged without a
/// fingerprint, the spec's acknowledgements are passed to the status Job and `status.upgrade.drift`
/// holds the four counts, `unfingerprinted: 4`, the first 20 unacknowledged objects with
/// fingerprints, and `more: 30`, never an acknowledged object
#[tokio::test]
async fn the_acknowledgements_reach_the_job_and_the_capped_summary_reaches_the_status() {
    // 60 unexpected tables, the last 10 acknowledged by the spec: four without a fingerprint
    let name = |index: usize| format!("extra_{index:02}");
    let observed = |index: usize| print(&format!("{index:064x}"));
    let acks = (50..60)
        .map(|index| Acknowledgement {
            backend: SchemaBackend::Scylla,
            object: name(index),
            fingerprint: (index >= 54).then(|| observed(index).as_str().to_owned()),
        })
        .collect::<Vec<_>>();
    let spec = serde_json::to_value(&acks).expect("the acknowledgements serialize");
    let cluster = cluster(json!({ "drift_acknowledged": spec }));
    let state = mirrored(&latest().to_string(), Vec::new());
    // no Job exists, so the check creates the status Job with the acknowledgements as an argument
    let fake_one = fake(&cluster, Some(&state), &[], None);
    drift::check(&meta_for(cluster.clone(), &fake_one.client())).await;
    let created = created_jobs(&fake_one);
    assert_eq!(created.len(), 1);
    let args = job_args(&created);
    let at = args
        .iter()
        .position(|arg| arg == "--acknowledged")
        .expect("the Job is given the acknowledgements");
    let passed: Vec<Acknowledgement> = serde_json::from_str(&args[at + 1]).expect("they parse");
    assert_eq!(passed, acks);
    assert!(!args.contains(&"--no-probe".to_owned()), "{args:?}");
    // thoradm reports the capped summary it builds from the same acknowledgements
    let objects = (0..60)
        .map(|index| ObjectDrift {
            backend: SchemaBackend::Scylla,
            object: name(index),
            kind: DriftKind::Unexpected,
            fields: Vec::new(),
            acknowledged: false,
            lapsed: false,
            fingerprint: Some(observed(index)),
        })
        .collect();
    let mut drifted = DriftReport {
        revision: latest(),
        expected: print("expected"),
        observed: print("observed"),
        backends: BTreeMap::from([(
            SchemaBackend::Scylla,
            BackendDrift {
                status: BackendSchemaStatus::Drift,
                objects,
            },
        )]),
    };
    drifted.acknowledge(&acks);
    let finished = report(recorded_with(Some(drifted.summary(&acks))), None);
    let jobs = [status_job(DRIFT_PURPOSE, Some("Complete"))];
    let fake_two = fake(&cluster, Some(&state), &jobs, Some(&finished));
    drift::check(&meta_for(cluster.clone(), &fake_two.client())).await;
    // the status holds the four counts, the unfingerprinted count, 20 objects, and the rest
    let patches = status_patches(&fake_two);
    let shown = patches
        .iter()
        .rev()
        .find_map(|patch| patch["upgrade"]["drift"].as_object())
        .expect("the drift was patched");
    assert_eq!(shown["missing"], 0);
    assert_eq!(shown["changed"], 0);
    assert_eq!(shown["unexpected"], 50);
    assert_eq!(shown["acknowledged"], 10);
    assert_eq!(shown["unfingerprinted"], 4);
    assert_eq!(shown["more"], 30);
    let listed = shown["objects"].as_array().expect("objects");
    assert_eq!(listed.len(), 20);
    for (index, entry) in listed.iter().enumerate() {
        assert_eq!(entry["object"], name(index).as_str());
        assert_eq!(entry["fingerprint"], observed(index).as_str());
    }
}

/// The status Jobs that plan an upgrade or check drift probe the live schema with the spec's
/// acknowledgements, and the ones that only read the record skip the probe
#[tokio::test]
async fn only_plan_and_drift_reads_probe() {
    // a spec with one acknowledgement
    let cluster = cluster(json!({
        "drift_acknowledged": [{"backend": "scylla", "object": "extra"}]
    }));
    let meta = meta_for(cluster.clone(), &fake(&cluster, None, &[], None).client());
    for (purpose, probes) in [
        ("plan", true),
        (DRIFT_PURPOSE, true),
        ("record", false),
        ("record-result", false),
        ("check-job-uid", false),
    ] {
        // the Job's arguments carry either the acknowledgements or `--no-probe`
        let spec = status_spec(&meta, purpose, None, "nonce").expect("a Job");
        assert_eq!(drift::probes(purpose), probes, "{purpose}");
        assert_eq!(
            spec.args.contains(&"--no-probe".to_owned()),
            !probes,
            "{purpose}"
        );
        let position = spec.args.iter().position(|arg| arg == "--acknowledged");
        assert_eq!(position.is_some(), probes, "{purpose}: {:?}", spec.args);
        if let Some(at) = position {
            let passed: Vec<Acknowledgement> = serde_json::from_str(&spec.args[at + 1]).unwrap();
            assert_eq!(passed.len(), 1);
        }
    }
}

/// A plan's read of the record notes the drift it reports in the state and reports it in the
/// status, and the cached read that follows leaves both alone
#[tokio::test]
async fn a_plan_read_notes_and_reports_its_drift() {
    // an upgrade whose plan read reports drift
    let cluster = cluster(json!({"target_revision": "2026-10-v02"}));
    let state = mirrored("2026-10-v01", Vec::new());
    let mut pending = pending_of(&cluster, &state).await;
    let held = RecordStatus::Recorded {
        record: record("2026-10-v01", Vec::new(), &[]),
        drift: Some(three_kinds()),
    };
    let finished = report(held, None);
    let jobs = [status_job("plan", Some("Complete"))];
    let fake_one = fake(&cluster, Some(&state), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_one.client());
    assert!(
        refresh(&meta, &mut pending)
            .await
            .expect("refresh")
            .is_none()
    );
    // the state the plan holds notes the drift, and so does the saved copy
    assert_eq!(drift::noted(&pending.stored.state), Some(three_kinds()));
    let noted = written(&fake_one).expect("the read was noted in the state");
    assert_eq!(drift::noted(&noted), Some(three_kinds()));
    // the status shows it
    let shown = status_patches(&fake_one)
        .into_iter()
        .rev()
        .find_map(|patch| patch["upgrade"]["drift"]["unexpected"].as_u64());
    assert_eq!(shown, Some(1));
    // a read with no drift to report (the probe skipped) clears what was noted
    let mut none = noted.clone();
    drift::note(&mut none, None, "[]");
    assert_eq!(drift::noted(&none), None);
}

/// Steps that target a drifted object's definition, for the tests of blocking
struct Targeted(&'static [SchemaTarget]);

impl RecordOps for Targeted {
    /// Plan from the state the gate read, as the other tests of the steps do
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    /// * `pending` - The upgrade about to run
    async fn refresh(
        &self,
        meta: &crate::k8s::clusters::ClusterMeta,
        pending: &mut Pending,
    ) -> Result<Option<Action>, Error> {
        // no record is read
        LocalRecord.refresh(meta, pending).await
    }

    /// Record the revision locally, as the other tests of the steps do
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    /// * `pending` - The upgrade whose state is advanced
    /// * `revision` - The revision to record
    async fn move_revision(
        &self,
        meta: &crate::k8s::clusters::ClusterMeta,
        pending: &mut Pending,
        revision: &RevisionId,
    ) -> Result<Moved, Error> {
        // the record moves locally
        LocalRecord.move_revision(meta, pending, revision).await
    }

    /// Find a step with the targets of these tests
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the step
    fn step(&self, id: &str) -> Option<(&'static CatalogEntry, &'static StepDef)> {
        // the step of the catalog the test plans with, with targets
        let (entry, def) = super::catalog().step(id)?;
        let targeted = StepDef {
            targets: self.0,
            ..*def
        };
        Some((entry, Box::leak(Box::new(targeted))))
    }
}

/// The targets of a step that changes the `comments_by_user` table
const COMMENTS: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "comments_by_user",
    field: None,
}];

/// The targets of a step that changes the `tags` table
const TAGS: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "tags",
    field: None,
}];

/// A state in the middle of the first Helm revision's edge with a changed `comments_by_user` table
/// noted as drift
fn drifted_state() -> UpgradeState {
    // every step but the reindex check and finalize is done
    let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
    finish(&mut state, &["scylla-role", "elastic-identity"]);
    let changed = DriftSummary {
        changed: 1,
        objects: vec![DriftEntry {
            backend: SchemaBackend::Scylla,
            object: "comments_by_user".to_owned(),
            kind: DriftKind::Changed,
            fingerprint: Some("c03e".to_owned()),
            lapsed: false,
        }],
        backends: BTreeMap::from([(SchemaBackend::Scylla, BackendSchemaStatus::Drift)]),
        ..DriftSummary::default()
    };
    drift::note(&mut state, Some(&changed), "[]");
    state
}

/// During an upgrade, a step whose targets overlap a drifted object blocks with an explanation
/// naming the object and the step stays unfinished, while a step with unrelated targets runs
#[tokio::test]
async fn a_step_blocks_on_drift_in_its_targets_and_unrelated_steps_run() {
    // the fixture step targets the drifted table: it blocks without running; the target is the
    // fixture step's revision, so the run ends with that step
    let _catalog = first_helm_fixture();
    let cluster = cluster(json!({"target_revision": "2026-10-v02"}));
    let state = drifted_state();
    let (fake_one, pending) = pending_upgrade(&cluster, &state).await;
    let meta = meta_for(cluster.clone(), &fake_one.client());
    let action = Box::pin(run_with(&meta, pending, &Targeted(COMMENTS)))
        .await
        .expect("run");
    assert_eq!(
        action,
        Some(Action::requeue(Duration::from_secs(BLOCKED_REQUEUE_SECS)))
    );
    // the status names the object and the step, and the step isn't finished
    let last = status_patches(&fake_one).pop().expect("status");
    let message = last["upgrade"]["steps"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("comments_by_user"), "{message}");
    assert!(message.contains(STEP), "{message}");
    assert_eq!(last["upgrade"]["steps"][0]["state"], "Blocked");
    let saved = saved_state(&fake_one).expect("the block was recorded");
    assert!(!saved.finished("2026-10-v02", STEP));
    // with unrelated targets the same step runs, and its check finds nothing to do
    let (fake_two, pending) = pending_upgrade(&cluster, &state).await;
    fixture::script(Check::Nothing("every fixture row is right".to_owned()));
    let meta = meta_for(cluster.clone(), &fake_two.client());
    let action = Box::pin(run_with(&meta, pending, &Targeted(TAGS)))
        .await
        .expect("run");
    // the run isn't blocked; a later revision may still be ahead of the target
    assert_ne!(
        action,
        Some(Action::requeue(Duration::from_secs(BLOCKED_REQUEUE_SECS)))
    );
    let saved = saved_state(&fake_two).expect("the state was saved");
    let ran = saved
        .applied
        .iter()
        .find(|applied| applied.step == STEP)
        .expect("the step was recorded");
    assert_eq!(ran.outcome, StepState::Skipped);
}

/// Blocking looks only at drift that was noted: a step with targets runs when no drift was noted,
/// and an object only the catalog's acknowledgements covered never blocks
#[test]
fn a_step_without_noted_drift_is_not_blocked() {
    // a state that noted no drift, and a step with targets
    let state = mirrored(BASELINE_REVISION, Vec::new());
    let def = &fixture::DEF;
    let targeted = StepDef {
        targets: COMMENTS,
        ..*def
    };
    assert_eq!(drift::block(&state, &targeted), None);
    // the same state with the drift noted blocks it, naming the object
    let blocked = drift::block(&drifted_state(), &targeted).expect("blocked");
    assert!(blocked.contains("comments_by_user"), "{blocked}");
    // a step without targets never blocks
    assert_eq!(drift::block(&drifted_state(), def), None);
}

/// A summary noting one drifted Redis object
///
/// # Arguments
///
/// * `kind` - How the object differs
fn redis_users(kind: DriftKind) -> DriftSummary {
    // the users key family, as `thoradm schema diff` names it
    DriftSummary {
        changed: 1,
        objects: vec![DriftEntry {
            backend: SchemaBackend::Redis,
            object: "users".to_owned(),
            kind,
            fingerprint: None,
            lapsed: false,
        }],
        backends: BTreeMap::from([(SchemaBackend::Redis, BackendSchemaStatus::Drift)]),
        ..DriftSummary::default()
    }
}

/// A data step that converts the users layout is not blocked by a changed layout of its own target,
/// while every other drift of that target, and any other step, still blocks
#[test]
fn a_converting_step_ignores_a_changed_layout_of_its_own_target() {
    // the users step, which names a migration and targets the users
    /// The schema targets of the users step, a single Redis object with no field
    const USERS: &[SchemaTarget] = &[SchemaTarget {
        backend: SchemaBackend::Redis,
        object: "users",
        field: None,
    }];
    let def = &fixture::DEF;
    let converting = StepDef {
        kind: thorium::models::upgrades::StepKind::Data,
        thoradm_migration: Some("users-settings-theme"),
        targets: USERS,
        ..*def
    };
    let mut state = UpgradeState::new(rev(BASELINE_REVISION), "1.8.1");
    // a changed layout doesn't block the step that converts it
    drift::note(&mut state, Some(&redis_users(DriftKind::Changed)), "[]");
    assert_eq!(drift::block(&state, &converting), None);
    // an unexpected object of the same family still does
    drift::note(&mut state, Some(&redis_users(DriftKind::Unexpected)), "[]");
    assert!(drift::block(&state, &converting).is_some());
    // and a step that doesn't convert a layout is still blocked by the changed one
    drift::note(&mut state, Some(&redis_users(DriftKind::Changed)), "[]");
    let other = StepDef {
        thoradm_migration: None,
        ..converting
    };
    assert!(drift::block(&state, &other).is_some());
}

/// The targets of a step that changes the 25th of 25 drifted tables
const LAST: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "table_24",
    field: None,
}];

/// The targets of a step that changes a table that hasn't drifted
const OTHER: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "table_99",
    field: None,
}];

/// A step whose targets overlap a drifted object blocks even when the object lies beyond the
/// summary's cap of listed objects
#[test]
fn a_drifted_object_beyond_the_listed_cap_still_blocks() {
    // 25 changed Scylla tables drift; the 25th is the one the step targets
    let objects = (0..25)
        .map(|index| ObjectDrift {
            backend: SchemaBackend::Scylla,
            object: format!("table_{index:02}"),
            kind: DriftKind::Changed,
            fields: Vec::new(),
            acknowledged: false,
            lapsed: false,
            fingerprint: Some(print("observed")),
        })
        .collect();
    let drifted = DriftReport {
        revision: latest(),
        expected: print("expected"),
        observed: print("observed"),
        backends: BTreeMap::from([(
            SchemaBackend::Scylla,
            BackendDrift {
                status: BackendSchemaStatus::Drift,
                objects,
            },
        )]),
    };
    // the summary lists 20 and counts the rest, and the operator notes it as the Job reports it
    let summary = drifted.summary(&[]);
    assert_eq!((summary.objects.len(), summary.more), (20, 5));
    let mut state = mirrored(BASELINE_REVISION, Vec::new());
    drift::note(&mut state, Some(&summary), "[]");
    // a step targeting the 25th table is blocked, naming it
    let def = &fixture::DEF;
    let targeted = StepDef {
        targets: LAST,
        ..*def
    };
    let blocked = drift::block(&state, &targeted).expect("blocked");
    assert!(blocked.contains("table_24"), "{blocked}");
    // a step targeting an undrifted table still runs
    let other = StepDef {
        targets: OTHER,
        ..*def
    };
    assert_eq!(drift::block(&state, &other), None);
}

/// However many objects drift, the state that notes the summary stays far below what a `ConfigMap`
/// holds
#[test]
fn a_state_with_many_blockers_fits_a_config_map() {
    // 10,000 unexpected Scylla tables reported by a probe
    let objects = (0..10_000)
        .map(|index| ObjectDrift {
            backend: SchemaBackend::Scylla,
            object: format!("an_unexpected_table_with_a_rather_long_name_{index:05}"),
            kind: DriftKind::Unexpected,
            fields: Vec::new(),
            acknowledged: false,
            lapsed: false,
            fingerprint: Some(print("observed")),
        })
        .collect::<Vec<_>>();
    let drifted = DriftReport {
        revision: latest(),
        expected: print("expected"),
        observed: print("observed"),
        backends: BTreeMap::from([(
            SchemaBackend::Scylla,
            BackendDrift {
                status: BackendSchemaStatus::Drift,
                objects,
            },
        )]),
    };
    // the operator notes the summary in the state
    let mut state = mirrored(BASELINE_REVISION, Vec::new());
    drift::note(&mut state, Some(&drifted.summary(&[])), "[]");
    let bytes = state.schema.values().map(String::len).sum::<usize>();
    assert!(bytes < 64 * 1024, "{bytes} bytes");
}

/// A step blocked by drift makes the next read of the record due within a minute, so the block
/// lifts soon after an admin fixes the object
#[test]
fn a_blocked_step_reads_the_drift_again_soon() {
    // a state that read the record just now and noted drift in the step's targets
    let mut state = drifted_state();
    state
        .schema
        .insert("record_read".to_owned(), chrono::Utc::now().to_rfc3339());
    let def = &fixture::DEF;
    let targeted = StepDef {
        targets: COMMENTS,
        ..*def
    };
    // blocking says so, and moves the read back so it lapses within a minute
    let (message, changed) = drift::block_soon(&mut state, &targeted).expect("blocked");
    assert!(message.contains("comments_by_user"), "{message}");
    assert!(changed);
    let read = chrono::DateTime::parse_from_rfc3339(&state.schema["record_read"]).expect("date");
    let age = (chrono::Utc::now() - read.with_timezone(&chrono::Utc)).num_seconds();
    assert!(age >= 900 - 61, "the read is only {age} seconds old");
    // a step that isn't blocked leaves the read alone
    let before = state.schema["record_read"].clone();
    assert!(drift::block_soon(&mut state, def).is_none());
    assert_eq!(state.schema["record_read"], before);
}

/// A run blocked by drift saves the state with the record's read moved back, so the block is
/// checked again within a minute, including a block that was already recorded
#[tokio::test]
async fn a_run_blocked_by_drift_saves_an_early_read_of_the_record() {
    // the drifted state read the record just now, planned with the fixture step the operator checks
    // in process
    let _catalog = first_helm_fixture();
    let cluster = cluster(json!({"target_revision": "2026-10-v02"}));
    let mut state = drifted_state();
    let now = chrono::Utc::now().to_rfc3339();
    state.schema.insert("record_read".to_owned(), now.clone());
    // the first run records the block, the second finds it already recorded and read the record
    // again since
    for round in 0..2 {
        let (fake, pending) = pending_upgrade(&cluster, &state).await;
        let meta = meta_for(cluster.clone(), &fake.client());
        Box::pin(run_with(&meta, pending, &Targeted(COMMENTS)))
            .await
            .expect("run");
        // the saved state's read is old enough to lapse within a minute
        let saved = saved_state(&fake).expect("the state was saved");
        let read =
            chrono::DateTime::parse_from_rfc3339(&saved.schema["record_read"]).expect("date");
        let age = (chrono::Utc::now() - read.with_timezone(&chrono::Utc)).num_seconds();
        assert!(
            age >= 900 - 61,
            "round {round}: the read is only {age} seconds old"
        );
        // the next round starts from the saved state with a fresh read
        state = saved;
        state.schema.insert("record_read".to_owned(), now.clone());
    }
}
