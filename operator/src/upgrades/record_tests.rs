//! Tests of the operator's use of the deployment's record: recovering it, planning from it, and
//! moving its revision, all against the in-process fake kube API

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use kube::runtime::controller::Action;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::BTreeMap;
use thorium::models::schema::{
    AppliedSchema, CatalogIdentity, ConfigRecord, DeploymentRecord, Fingerprint, FinishedStep,
    RecordStatus, StatusReport,
};
use thorium::models::upgrades::{
    APPLY_KEY_MARKER, AppliedStep, DEPLOYMENT_MARKER, RECORD_MARKER, STATE_KEY, StepState,
    UpgradeState, latest,
};

use super::record::{
    Moved, Read, STATUS_STEP, move_revision, read_report, record_spec, recover_with, refresh,
    seed_spec, status_spec,
};
use super::schema_apply::tests::{JOBS_PATH, PODS_PATH, created_jobs, job_list};
use super::tests::{
    CONFIG_MAPS_PATH, OPERATOR_PASS_PATH, SNAPSHOT_PATH, STATE_PATH, chart_cluster,
    fake_with_status, raw_state_config_map, rev, secret_metadata, status_patches, tracked,
};
use super::{Decision, Gate, Pending, decide, gate};
use crate::k8s::clusters::tests::{FakeKube, kube_status, meta_for};
use crate::k8s::crds::ThoriumCluster;
use crate::k8s::jobs::JobKind;

/// The baseline revision
const BASE: &str = "2026-10-v01";
/// The first Helm revision
const RELEASE: &str = "2026-10-v02";

/// The name of the status Job's pod in these tests
const POD: &str = "status-pod";

/// The first step of the first Helm revision's edge
const FIRST_STEP: &str = "scylla-role";

/// Build the chart shaped cluster of these tests, with upgrade settings
///
/// # Arguments
///
/// * `upgrade` - The cluster's `spec.upgrade`
pub(super) fn cluster(upgrade: Value) -> ThoriumCluster {
    // the chart's spec with the given upgrade settings
    chart_cluster(upgrade)
}

/// Build a fingerprint from a word
///
/// # Arguments
///
/// * `word` - What makes the fingerprint its own
pub(super) fn print(word: &str) -> Fingerprint {
    // fingerprints are transparent strings
    serde_json::from_value(json!(word)).expect("a fingerprint")
}

/// Build a history entry
///
/// # Arguments
///
/// * `revision` - The revision the step belongs to
/// * `step` - The step's id
/// * `outcome` - How the step ended
/// * `seconds` - How many seconds after the tests' start it ended
pub(super) fn entry(revision: &str, step: &str, outcome: StepState, seconds: i64) -> AppliedStep {
    // the fields the mirror keeps
    let start = DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .expect("a time")
        .with_timezone(&Utc);
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome,
        message: Some(format!("{step} {outcome:?}")),
        at: Some((start + Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Nanos, true)),
        by: Some("thoradm 1.8.1".to_owned()),
        ..AppliedStep::default()
    }
}

/// Build a record at a revision
///
/// # Arguments
///
/// * `revision` - The revision the record holds
/// * `history` - The history the status returns
/// * `finished` - The steps the full history finished
pub(super) fn record(
    revision: &str,
    history: Vec<AppliedStep>,
    finished: &[(&str, &str)],
) -> DeploymentRecord {
    // the applied schema and config are the same whatever the history
    DeploymentRecord {
        revision: rev(revision),
        applied_by_version: Some("1.8.1".to_owned()),
        history_len: history.len() as u64,
        history,
        finished: finished
            .iter()
            .map(|(revision, step)| FinishedStep {
                revision: rev(revision),
                step: (*step).to_owned(),
            })
            .collect(),
        schema: AppliedSchema {
            deployment: print("applied"),
            backends: BTreeMap::new(),
            layouts: BTreeMap::new(),
        },
        config: ConfigRecord {
            partition_sizes: BTreeMap::new(),
            earliest: BTreeMap::new(),
            files_password: String::new(),
        },
    }
}

/// Build the report a status Job prints
///
/// # Arguments
///
/// * `status` - What the record holds
/// * `job_result` - The full result of a writing Job, when one was asked for
pub(super) fn report(status: RecordStatus, job_result: Option<RecordStatus>) -> StatusReport {
    // thoradm's catalog is this build's
    StatusReport {
        status,
        catalog: CatalogIdentity {
            latest: latest(),
            hash: print("catalog"),
        },
        job_result,
        check: None,
    }
}

/// Build a record status
///
/// # Arguments
///
/// * `record` - The record
fn recorded(record: DeploymentRecord) -> RecordStatus {
    // no drift summary is read
    RecordStatus::Recorded {
        record,
        drift: None,
    }
}

/// Build a Job as the kube API returns it
///
/// # Arguments
///
/// * `kind` - The Job's kind label
/// * `step` - The Job's step label
/// * `name` - The Job's name
/// * `annotations` - The annotations the Job was created with
/// * `condition` - The Job's terminal condition, or None while it runs
fn job(
    kind: &str,
    step: &str,
    name: &str,
    annotations: &[(&str, &str)],
    condition: Option<&str>,
) -> Value {
    // label it like the operator does and add the terminal condition
    let conditions: Vec<Value> = condition
        .into_iter()
        .map(|kind| {
            json!({
                "type": kind,
                "status": "True",
                "reason": if kind == "Failed" { "PodFailurePolicy" } else { "Completed" },
                "message": "fake",
                "lastTransitionTime": "2026-10-22T00:00:00Z",
            })
        })
        .collect();
    let annotations = annotations
        .iter()
        .map(|(key, value)| ((*key).to_owned(), json!(value)))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": name,
            "namespace": "thorium",
            "uid": format!("uid-{name}"),
            "labels": {
                "thorium.sandia.gov/job-kind": kind,
                "thorium.sandia.gov/step": step,
            },
            "annotations": annotations,
        },
        "spec": {"template": {"spec": {"containers": []}}},
        "status": {"conditions": conditions}
    })
}

/// Turn a Job the operator created into the Job the kube API lists afterwards, with a UID and a
/// terminal condition
///
/// # Arguments
///
/// * `created` - The Job as the operator created it
/// * `condition` - The Job's terminal condition, or None while it runs
fn as_listed(created: &Value, condition: Option<&str>) -> Value {
    // add the UID and the condition the API fills in
    let mut listed = created.clone();
    let name = created["metadata"]["name"].as_str().expect("a name");
    listed["metadata"]["uid"] = json!(format!("uid-{name}"));
    listed["status"] = json!({"conditions": condition.into_iter().map(|kind| json!({
        "type": kind,
        "status": "True",
        "reason": "Completed",
        "message": "fake",
        "lastTransitionTime": "2026-10-22T00:00:00Z",
    })).collect::<Vec<_>>()});
    listed
}

/// Build a status Job as the kube API returns it
///
/// # Arguments
///
/// * `purpose` - Why it reads the record
/// * `condition` - Its terminal condition, or None while it runs
pub(super) fn status_job(purpose: &str, condition: Option<&str>) -> Value {
    // a Job named for its purpose
    job(
        "schema-status",
        STATUS_STEP,
        &format!("status-{purpose}"),
        &[("thorium.sandia.gov/purpose", purpose)],
        condition,
    )
}

/// Build the one-pod list a finished Job's pod is found in
fn pods() -> Value {
    // a finished thoradm pod named for these tests
    json!({
        "apiVersion": "v1",
        "kind": "PodList",
        "metadata": {},
        "items": [{
            "metadata": {"name": POD, "namespace": "thorium"},
            "status": {"containerStatuses": [{
                "name": "thoradm",
                "ready": false,
                "restartCount": 0,
                "image": "registry/thorium:1.8.1",
                "imageID": "",
                "state": {"terminated": {"exitCode": 0, "message": "{\"outcome\":\"done\"}"}}
            }]}
        }]
    })
}

/// Build the log a status Job's pod leaves: some lines of tracing output and the result line
///
/// # Arguments
///
/// * `report` - The report the Job printed
pub(super) fn log_of(report: &StatusReport) -> String {
    // the result line comes after other output and before a closing line
    format!(
        "2026-10-22T00:00:00Z INFO connecting\nTHORIUM-RESULT {}\n2026-10-22T00:00:01Z INFO done\n",
        serde_json::to_string(report).expect("the report serializes")
    )
}

/// Build the fake kube API of a recovery: status patches, the state, the Jobs, and the status
/// Job's pod and log
///
/// # Arguments
///
/// * `cluster` - The cluster being recovered
/// * `state` - The state `ConfigMap`, when one exists
/// * `jobs` - The Jobs that exist
/// * `report` - What the status Job's log holds, when a Job finished
pub(super) fn fake(
    cluster: &ThoriumCluster,
    state: Option<&UpgradeState>,
    jobs: &[Value],
    report: Option<&StatusReport>,
) -> FakeKube {
    // answer status patches, the state, Job lists, creates, and deletes, and the pod's log
    let mut fake = fake_with_status(cluster)
        .route("GET", JOBS_PATH, 200, job_list(jobs))
        .route(
            "POST",
            JOBS_PATH,
            201,
            jobs.first()
                .cloned()
                .unwrap_or_else(|| job("x", "x", "created", &[], None)),
        )
        .route("GET", PODS_PATH, 200, pods())
        .route(
            "POST",
            CONFIG_MAPS_PATH,
            201,
            raw_state_config_map(&UpgradeState::new(rev(RELEASE), "1.8.1")),
        );
    for item in jobs {
        let name = item["metadata"]["name"].as_str().expect("a name");
        fake = fake.route(
            "DELETE",
            &format!("{JOBS_PATH}/{name}"),
            200,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}),
        );
    }
    if let Some(report) = report {
        fake = fake.route(
            "GET",
            &format!("{PODS_PATH}/{POD}/log"),
            200,
            Value::String(log_of(report)),
        );
    }
    match state {
        Some(state) => fake
            .route("GET", STATE_PATH, 200, raw_state_config_map(state))
            .route("PUT", STATE_PATH, 200, raw_state_config_map(state)),
        None => fake.route("GET", STATE_PATH, 404, kube_status(404, "NotFound")),
    }
}

/// Get the state written to the state `ConfigMap` through a fake kube API, by create or replace
///
/// # Arguments
///
/// * `fake` - The fake kube API
pub(super) fn written(fake: &FakeKube) -> Option<UpgradeState> {
    // the last create or replace of the state ConfigMap
    let request = fake.writes().into_iter().rfind(|request| {
        (request.method == "POST" && request.path == CONFIG_MAPS_PATH)
            || (request.method == "PUT" && request.path == STATE_PATH)
    })?;
    let raw = request.body["data"][STATE_KEY].as_str().expect("a state");
    Some(serde_json::from_str(raw).expect("the state parses"))
}

/// A prepare step that does nothing, as when the backends are ready
///
/// # Arguments
///
/// * `_` - The cluster, which is ignored
pub(super) async fn ready(
    _: &crate::k8s::clusters::ClusterMeta,
) -> Result<Option<Action>, thorium::Error> {
    // nothing to wait on
    Ok(None)
}

/// A state that mirrors a record
///
/// # Arguments
///
/// * `revision` - The revision the state holds
/// * `entries` - The state's history
pub(super) fn mirrored(revision: &str, entries: Vec<AppliedStep>) -> UpgradeState {
    // a state carrying the record marker
    let mut state = UpgradeState::new(rev(revision), "1.8.1");
    for applied in entries {
        state.record(applied);
    }
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    state
}

/// Gate a cluster against a fake kube API
///
/// # Arguments
///
/// * `fake` - The fake kube API
/// * `cluster` - The cluster to gate
async fn gated(fake: &FakeKube, cluster: &ThoriumCluster) -> Gate {
    // the gate decides from the state ConfigMap alone
    gate(&fake.client(), &tracked(cluster)).await.expect("gate")
}

/// The gate asks for the record when the state is missing or mirrors none, writing nothing, and
/// plans as before from a state that mirrors one
#[tokio::test]
async fn the_gate_needs_a_record_when_the_state_is_missing_or_unmirrored() {
    // a missing state needs a record and the gate writes nothing for it
    let cluster = cluster(json!({}));
    let fake = fake(&cluster, None, &[], None);
    assert!(matches!(gated(&fake, &cluster).await, Gate::NeedsRecord));
    assert!(
        fake.writes().iter().all(|request| request.path
            == "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium/status"),
        "{:?}",
        fake.writes()
    );
    // a state without the record marker needs one too
    let unmirrored = UpgradeState::new(rev(RELEASE), "1.8.1");
    let fake = self::fake(&cluster, Some(&unmirrored), &[], None);
    assert!(matches!(gated(&fake, &cluster).await, Gate::NeedsRecord));
    // a state that mirrors a record at the latest revision needs none
    let current = mirrored(&latest().to_string(), Vec::new());
    let fake = self::fake(&cluster, Some(&current), &[], None);
    assert!(matches!(gated(&fake, &cluster).await, Gate::Current));
}

/// With the state deleted and a status Job reporting a keyspace with a record, recovery creates no
/// Job but the status Job until the state is rebuilt with the recorded revision and history, with
/// or without the operator's own Secret, and the plan from it runs no recorded step again
#[tokio::test]
async fn a_lost_state_is_rebuilt_from_the_recorded_revision() {
    // no status Job exists yet: recovery prepares once and creates only the status Job
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let fake_one = fake(&cluster, None, &[], None);
    let meta = meta_for(cluster.clone(), &fake_one.client());
    let prepared = Cell::new(0);
    let action = recover_with(&meta, async |_| {
        prepared.set(prepared.get() + 1);
        Ok(None)
    })
    .await
    .expect("recover");
    assert!(action.is_some());
    assert_eq!(prepared.get(), 1);
    let created = created_jobs(&fake_one);
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(
        created[0]["metadata"]["labels"]["thorium.sandia.gov/job-kind"],
        "schema-status"
    );
    assert!(written(&fake_one).is_none());
    // the status Job finished and reports a record at the baseline with one step done
    let history = vec![
        entry(BASE, "convert-to-helm", StepState::Done, 0),
        entry(RELEASE, FIRST_STEP, StepState::Done, 1),
    ];
    let status = recorded(record(BASE, history.clone(), &[(RELEASE, FIRST_STEP)]));
    let finished = report(status, None);
    let mut rebuilt = Vec::new();
    for secret in [false, true] {
        let jobs = [status_job("record", Some("Complete"))];
        let mut fake_two = fake(&cluster, None, &jobs, Some(&finished));
        if secret {
            fake_two = fake_two.route(
                "GET",
                OPERATOR_PASS_PATH,
                200,
                secret_metadata("thorium-operator-pass"),
            );
        }
        let meta = meta_for(cluster.clone(), &fake_two.client());
        let action = recover_with(&meta, ready).await.expect("recover");
        // the state is rebuilt and no other Job is created
        assert!(action.is_none(), "secret {secret}: {action:?}");
        assert!(created_jobs(&fake_two).is_empty(), "secret {secret}");
        let state = written(&fake_two).expect("the state was written");
        assert_eq!(state.revision, rev(BASE));
        assert_eq!(state.applied.len(), 2);
        assert_eq!(state.applied[1].step, FIRST_STEP);
        assert!(state.schema.contains_key(RECORD_MARKER));
        rebuilt.push(state);
    }
    // the secret made no difference to what was rebuilt
    assert_eq!(rebuilt[0], rebuilt[1]);
    // the plan from the rebuilt state runs every step but the recorded one
    let upgrade = cluster.spec.upgrade.clone();
    let Decision::Upgrade { steps, .. } = decide(&rebuilt[0], &upgrade) else {
        panic!("the cluster should be upgraded");
    };
    assert!(
        steps.iter().all(|step| step.step != FIRST_STEP),
        "{steps:?}"
    );
    assert_ne!(steps.len(), 0);
}

/// A keyspace without a Thorium record is a fresh install even when the operator's own Secret
/// exists: the state is created at the latest revision
#[tokio::test]
async fn no_keyspace_is_a_fresh_install_whatever_secrets_exist() {
    // the status Job found no keyspace, and the operator's Secret exists
    let cluster = cluster(json!({}));
    let finished = report(RecordStatus::NoKeyspace, None);
    let jobs = [status_job("record", Some("Complete"))];
    let fake = fake(&cluster, None, &jobs, Some(&finished)).route(
        "GET",
        OPERATOR_PASS_PATH,
        200,
        secret_metadata("thorium-operator-pass"),
    );
    let meta = meta_for(cluster.clone(), &fake.client());
    let action = recover_with(&meta, ready).await.expect("recover");
    assert!(action.is_none(), "{action:?}");
    // the state starts at the latest revision, marked as mirroring the record, and no Job ran
    let state = written(&fake).expect("the state was written");
    assert_eq!(state.revision, latest());
    assert!(state.schema.contains_key(RECORD_MARKER));
    assert_eq!(created_jobs(&fake).len(), 0);
}

/// A status Job that can't reach Scylla, or whose login fails, is reported and retried, and the
/// namespace is never taken for a fresh install
#[tokio::test]
async fn an_unreachable_or_refused_scylla_is_retried_not_taken_for_fresh() {
    // each answer that isn't a record
    let cluster = cluster(json!({}));
    for (status, said) in [
        (
            RecordStatus::Unreachable {
                message: "no route to host".to_owned(),
            },
            "no route to host",
        ),
        (RecordStatus::AuthFailed, "login"),
    ] {
        let finished = report(status, None);
        let jobs = [status_job("record", Some("Complete"))];
        let fake = fake(&cluster, None, &jobs, Some(&finished));
        let meta = meta_for(cluster.clone(), &fake.client());
        let action = recover_with(&meta, ready).await.expect("recover");
        // the operator waits, writes no state, and says why
        assert!(action.is_some(), "{said}");
        assert!(written(&fake).is_none(), "{said}");
        let messages = status_patches(&fake)
            .iter()
            .filter_map(|status| status["message"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|message| message.contains(said)),
            "{said} isn't in {messages:?}"
        );
    }
}

/// On an empty namespace the `thorium` Secret and the Scylla role come before the status Job, and
/// no Job is created until they are
#[tokio::test]
async fn the_secret_and_the_role_come_before_the_status_job() {
    // a prepare step that writes the Secret the way the operator does
    let cluster = cluster(json!({}));
    let fake = fake(&cluster, None, &[], None).route(
        "PUT",
        "/api/v1/namespaces/thorium/secrets/thorium",
        200,
        json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "thorium"}}),
    );
    let meta = meta_for(cluster.clone(), &fake.client());
    let writer = fake.client();
    recover_with(&meta, async move |_| {
        // the Secret write that bootstraps the Job's mount
        let secrets: kube::Api<k8s_openapi::api::core::v1::Secret> =
            kube::Api::namespaced(writer, "thorium");
        let secret = k8s_openapi::api::core::v1::Secret {
            metadata: kube::api::ObjectMeta {
                name: Some("thorium".to_owned()),
                ..kube::api::ObjectMeta::default()
            },
            ..k8s_openapi::api::core::v1::Secret::default()
        };
        secrets
            .replace("thorium", &kube::api::PostParams::default(), &secret)
            .await
            .expect("the Secret is written");
        Ok(None)
    })
    .await
    .expect("recover");
    // every Job creation comes after the Secret write
    let requests = fake.requests();
    let secret_at = requests
        .iter()
        .position(|request| request.path.ends_with("/secrets/thorium"))
        .expect("the Secret was written");
    let job_at = requests
        .iter()
        .position(|request| request.method == "POST" && request.path == JOBS_PATH)
        .expect("the status Job was created");
    assert!(secret_at < job_at, "{requests:?}");
}

/// Pick the suggestion out of the message a blocked recovery reports
///
/// # Arguments
///
/// * `fake` - The fake kube API that received the status patches
fn blocked_message(fake: &FakeKube) -> String {
    // the newest status message that names the starting revision
    status_patches(fake)
        .iter()
        .filter_map(|status| status["message"].as_str().map(str::to_owned))
        .rfind(|message| message.contains("starting_revision"))
        .unwrap_or_default()
}

/// A keyspace with data and no record blocks, running only the status Job, with a message naming
/// the case, the field to set, and the revision today's detection would pick: the first Helm
/// revision, or the baseline for a converted pre-Helm deployment
#[tokio::test]
async fn a_keyspace_without_a_record_blocks_for_the_starting_revision() {
    // the status Job found data and no record
    let cluster = cluster(json!({}));
    let finished = report(RecordStatus::NoRecord { suggested: None }, None);
    for (converted, suggested) in [(false, RELEASE), (true, BASE)] {
        let jobs = [status_job("record", Some("Complete"))];
        let mut fake = fake(&cluster, None, &jobs, Some(&finished));
        if converted {
            fake = fake.route(
                "GET",
                SNAPSHOT_PATH,
                200,
                secret_metadata("thorium-legacy-cluster"),
            );
        }
        let meta = meta_for(cluster.clone(), &fake.client());
        let action = recover_with(&meta, ready).await.expect("recover");
        assert!(action.is_some(), "converted {converted}");
        // nothing was created or written but status patches
        assert!(created_jobs(&fake).is_empty(), "converted {converted}");
        assert!(written(&fake).is_none(), "converted {converted}");
        // the message names the case, the field, and the suggestion
        let message = blocked_message(&fake);
        assert!(message.contains("no record"), "{message}");
        assert!(
            message.contains("spec.upgrade.starting_revision"),
            "{message}"
        );
        assert!(
            message.contains(suggested),
            "converted {converted}: {message}"
        );
    }
}

/// Once the starting revision is set, the operator runs `schema apply --seed-revision` naming it
/// and where it came from, with no state mounted, and waits while that Job runs
#[tokio::test]
async fn the_starting_revision_is_seeded_by_the_apply_job() {
    // the field is set and no seed Job exists
    let cluster = cluster(json!({"starting_revision": BASE}));
    let finished = report(RecordStatus::NoRecord { suggested: None }, None);
    let jobs = [status_job("record", Some("Complete"))];
    let fake_one = fake(&cluster, None, &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_one.client());
    let action = recover_with(&meta, ready).await.expect("recover");
    assert!(action.is_some());
    // exactly one Job was created: the apply that seeds the revision
    let created = created_jobs(&fake_one);
    assert_eq!(created.len(), 1, "{created:?}");
    let pod = &created[0]["spec"]["template"]["spec"];
    let container = &pod["containers"][0];
    let args = container["args"].as_array().expect("args");
    let args = args.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--seed-revision", BASE]),
        "{args:?}"
    );
    assert!(args.contains(&"apply"), "{args:?}");
    let env = container["env"].as_array().expect("env");
    assert!(
        env.iter().any(|var| var["name"] == "THORIUM_SEED_SOURCE"
            && var["value"] == "spec.upgrade.starting_revision"),
        "{env:?}"
    );
    assert!(
        !pod["volumes"]
            .as_array()
            .expect("volumes")
            .iter()
            .any(|volume| volume["name"] == "state"),
        "the seed Job mounts no state"
    );
    // while that Job runs, no second one is created
    let running = [
        status_job("record", Some("Complete")),
        job(
            "schema-apply",
            "seed-revision",
            "seed-running",
            &[("thorium.sandia.gov/revision", BASE)],
            None,
        ),
    ];
    let fake_two = fake(&cluster, None, &running, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_two.client());
    let action = recover_with(&meta, ready).await.expect("recover");
    assert!(action.is_some());
    assert!(
        created_jobs(&fake_two).is_empty(),
        "{:?}",
        created_jobs(&fake_two)
    );
}

/// A state that exists but mirrors no record, over a keyspace with no record, is seeded by a
/// `migrate record --seed` Job with the state mounted before any schema-apply Job exists
#[tokio::test]
async fn an_existing_state_is_seeded_before_the_first_apply_job() {
    // a state at the first Helm revision with a step done, and a keyspace with no record
    let cluster = cluster(json!({}));
    let mut state = UpgradeState::new(rev(RELEASE), "1.8.1");
    state.record(entry(RELEASE, FIRST_STEP, StepState::Done, 1));
    let finished = report(RecordStatus::NoRecord { suggested: None }, None);
    let jobs = [status_job("record", Some("Complete"))];
    let fake_one = fake(&cluster, Some(&state), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_one.client());
    let action = recover_with(&meta, ready).await.expect("recover");
    assert!(action.is_some());
    // one Job, the seed, with the state mounted read-only and no token, and no apply Job
    let created = created_jobs(&fake_one);
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(
        created[0]["metadata"]["labels"]["thorium.sandia.gov/job-kind"],
        "record"
    );
    let pod = &created[0]["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    let args = pod["containers"][0]["args"].as_array().expect("args");
    let args = args.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    assert!(args.contains(&"--seed"), "{args:?}");
    assert!(
        args.windows(2).any(|pair| pair == ["--revision", RELEASE]),
        "{args:?}"
    );
    assert!(
        args.windows(2).any(|pair| pair == ["--state", "/state"]),
        "{args:?}"
    );
    let mounts = pod["containers"][0]["volumeMounts"]
        .as_array()
        .expect("mounts");
    let mount = mounts
        .iter()
        .find(|mount| mount["name"] == "state")
        .expect("the state mount");
    assert_eq!(mount["readOnly"], true);
    // once the seed ran, the status reads the record and the state is marked as mirroring it
    let seeded = report(
        recorded(record(
            RELEASE,
            state.applied.clone(),
            &[(RELEASE, FIRST_STEP)],
        )),
        None,
    );
    let jobs = [status_job("record", Some("Complete"))];
    let fake_two = fake(&cluster, Some(&state), &jobs, Some(&seeded));
    let meta = meta_for(cluster.clone(), &fake_two.client());
    let action = recover_with(&meta, ready).await.expect("recover");
    assert!(action.is_none(), "{action:?}");
    let after = written(&fake_two).expect("the state was rewritten");
    assert!(after.schema.contains_key(RECORD_MARKER));
    assert_eq!(after.revision, rev(RELEASE));
    assert_eq!(created_jobs(&fake_two).len(), 0);
}

/// Gate a cluster whose state mirrors a record at a revision and return its pending upgrade
///
/// # Arguments
///
/// * `cluster` - The cluster to gate
/// * `state` - The state it holds
pub(super) async fn pending_of(cluster: &ThoriumCluster, state: &UpgradeState) -> Pending {
    // the gate must decide to upgrade
    let fake = fake(cluster, Some(state), &[], None);
    let Gate::Upgrade(pending) = gated(&fake, cluster).await else {
        panic!("the cluster should be upgraded");
    };
    pending
}

/// A state edited by hand is rewritten from the next result, keeping an entry the operator
/// recorded since the last copy
#[tokio::test]
async fn an_edited_state_is_rewritten_from_the_record() {
    // the record is at the first Helm revision, but the state was edited back to the baseline
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let history = vec![
        entry(RELEASE, FIRST_STEP, StepState::Done, 1),
        entry(RELEASE, "finalize", StepState::Done, 2),
    ];
    let held = record(RELEASE, history, &[(RELEASE, FIRST_STEP)]);
    let finished = report(recorded(held), None);
    let edited = mirrored(
        BASE,
        vec![entry(RELEASE, "elastic-identity", StepState::Skipped, 400)],
    );
    let mut pending = pending_of(&cluster, &edited).await;
    assert!(
        pending.steps.iter().any(|step| step.step == FIRST_STEP),
        "control: {:?}",
        pending.steps
    );
    // refresh reads the record and rewrites the state from it
    let jobs = [status_job("plan", Some("Complete"))];
    let fake = fake(&cluster, Some(&edited), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake.client());
    let action = refresh(&meta, &mut pending).await.expect("refresh");
    assert!(action.is_none(), "{action:?}");
    let rewritten = written(&fake).expect("the state was rewritten");
    assert_eq!(rewritten.revision, rev(RELEASE));
    assert!(rewritten.schema.contains_key(RECORD_MARKER));
    // the operator's own entry that the record lacks is kept for the next copy
    assert!(
        rewritten
            .applied
            .iter()
            .any(|applied| applied.step == "elastic-identity"
                && applied.outcome == StepState::Skipped),
        "the operator's own entry was lost: {:?}",
        rewritten.applied
    );
    assert!(
        rewritten
            .applied
            .iter()
            .any(|applied| applied.step == FIRST_STEP)
    );
    // the plan from the record has nothing left to run
    assert!(pending.steps.is_empty(), "{:?}", pending.steps);
}

/// A step recorded `Done`, followed by 250 more entries, is in the status Job's finished set and
/// isn't planned again, although the state's window of 200 entries doesn't hold it
#[tokio::test]
async fn a_finished_step_outside_the_window_is_not_planned_again() {
    // the record is at the baseline; its newest 200 entries are notes, and the step finished before
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let history = (50..250)
        .map(|second| entry(RELEASE, "other-step", StepState::Blocked, second))
        .collect::<Vec<_>>();
    assert_eq!(history.len(), 200);
    let mut full = record(BASE, history, &[(RELEASE, FIRST_STEP)]);
    full.history_len = 251;
    let finished = report(recorded(full), None);
    // the state the operator holds mirrors the window, which lacks the finished step
    let window = mirrored(BASE, Vec::new());
    let mut pending = pending_of(&cluster, &window).await;
    assert!(
        pending.steps.iter().any(|step| step.step == FIRST_STEP),
        "control: {:?}",
        pending.steps
    );
    let jobs = [status_job("plan", Some("Complete"))];
    let fake = fake(&cluster, Some(&window), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake.client());
    let action = refresh(&meta, &mut pending).await.expect("refresh");
    assert!(action.is_none(), "{action:?}");
    // the plan leaves the finished step out and keeps the rest
    assert!(
        pending.steps.iter().all(|step| step.step != FIRST_STEP),
        "{:?}",
        pending.steps
    );
    assert!(
        pending
            .steps
            .iter()
            .any(|step| step.step == "elastic-identity"),
        "{:?}",
        pending.steps
    );
}

/// The record Job isn't created while another writing Job runs, since one writer runs at a time
#[tokio::test]
async fn the_record_job_waits_for_a_writing_job() {
    // a plan whose steps are all recorded, and a migration Job that is still running
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let state = mirrored(
        BASE,
        vec![entry(RELEASE, FIRST_STEP, StepState::Skipped, 1)],
    );
    let mut pending = pending_of(&cluster, &state).await;
    let writing = [job(
        "migration",
        "users-v2",
        "migration-users-v2-aaaaaaaaaaaa",
        &[],
        None,
    )];
    let fake_one = fake(&cluster, Some(&state), &writing, None);
    let meta = meta_for(cluster.clone(), &fake_one.client());
    // the revision isn't recorded and no record Job is created
    let moved = move_revision(&meta, &mut pending, &rev(RELEASE))
        .await
        .expect("move");
    assert!(matches!(moved, Moved::Wait(_)), "{moved:?}");
    assert_eq!(created_jobs(&fake_one).len(), 0);
}

/// Moving the revision creates a `migrate record` Job that mounts the state, waits while it runs,
/// and once it finished reads its result row through a status Job and mirrors the record
#[tokio::test]
async fn the_record_job_moves_the_revision_and_the_state_follows() {
    // a plan whose steps are all recorded
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let state = mirrored(
        BASE,
        vec![entry(RELEASE, FIRST_STEP, StepState::Skipped, 1)],
    );
    let mut pending = pending_of(&cluster, &state).await;
    // no record Job yet: one is created, naming the revision and mounting the state
    let fake_one = fake(&cluster, Some(&state), &[], None);
    let meta = meta_for(cluster.clone(), &fake_one.client());
    let moved = move_revision(&meta, &mut pending, &rev(RELEASE))
        .await
        .expect("move");
    assert!(matches!(moved, Moved::Wait(_)), "{moved:?}");
    let created = created_jobs(&fake_one);
    assert_eq!(created.len(), 1, "{created:?}");
    let args = created[0]["spec"]["template"]["spec"]["containers"][0]["args"]
        .as_array()
        .expect("args");
    let args = args.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    assert!(
        args.windows(2).any(|pair| pair == ["--revision", RELEASE]),
        "{args:?}"
    );
    assert!(
        args.windows(2).any(|pair| pair == ["--state", "/state"]),
        "{args:?}"
    );
    assert!(!args.contains(&"--seed"), "{args:?}");
    // while it runs nothing else is created
    let running = [as_listed(&created[0], None)];
    let fake_two = fake(&cluster, Some(&state), &running, None);
    let meta = meta_for(cluster.clone(), &fake_two.client());
    let moved = move_revision(&meta, &mut pending, &rev(RELEASE))
        .await
        .expect("move");
    assert!(matches!(moved, Moved::Wait(_)), "{moved:?}");
    assert_eq!(created_jobs(&fake_two).len(), 0);
    // once it finished, a status Job reads its result row; with that Job done, the state follows
    let moved_record = record(
        RELEASE,
        vec![
            entry(RELEASE, FIRST_STEP, StepState::Skipped, 1),
            entry(RELEASE, "finalize", StepState::Done, 2),
        ],
        &[(RELEASE, FIRST_STEP)],
    );
    let result = report(recorded(moved_record.clone()), Some(recorded(moved_record)));
    let done = [
        as_listed(&created[0], Some("Complete")),
        status_job("record-result", Some("Complete")),
    ];
    let fake_three = fake(&cluster, Some(&state), &done, Some(&result));
    let meta = meta_for(cluster.clone(), &fake_three.client());
    let moved = move_revision(&meta, &mut pending, &rev(RELEASE))
        .await
        .expect("move");
    assert!(matches!(moved, Moved::Done), "{moved:?}");
    let after = written(&fake_three).expect("the state was rewritten");
    assert_eq!(after.revision, rev(RELEASE));
    assert!(
        after
            .applied
            .iter()
            .any(|applied| applied.step == "finalize")
    );
    // the Job's UID keys the row the status Job was asked for
    let status = created_jobs(&fake_three);
    assert!(
        status.is_empty(),
        "the status Job already existed: {status:?}"
    );
}

/// Only the record Job mounts the state `ConfigMap`; the status Job and the seed apply Job don't
#[tokio::test]
async fn only_the_record_job_mounts_the_state() {
    // build each of the Jobs this module creates
    let cluster = cluster(json!({}));
    let fake = FakeKube::default();
    let meta = meta_for(cluster, &fake.client());
    let revision = rev(RELEASE);
    let specs = [
        (status_spec(&meta, "plan", None, "n1").expect("spec"), false),
        (
            status_spec(&meta, "record-result", Some("uid-1"), "n2").expect("spec"),
            false,
        ),
        (seed_spec(&meta, &revision).expect("spec"), false),
        (
            record_spec(&meta, &revision, false, "hash").expect("spec"),
            true,
        ),
        (
            record_spec(&meta, &revision, true, "hash").expect("spec"),
            true,
        ),
    ];
    for (spec, mounts) in specs {
        assert_eq!(spec.mount_state, mounts, "{:?}", spec.kind);
        let built = serde_json::to_value(crate::k8s::jobs::build(&spec, &meta).expect("build"))
            .expect("json");
        let volumes = built["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .expect("volumes");
        assert_eq!(
            volumes
                .iter()
                .any(|volume| volume["configMap"]["name"] == "thorium-upgrade-state"),
            mounts,
            "{:?}",
            spec.kind
        );
        // every Job learns its own UID from its pod's label, which keys its result row
        let env = built["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .expect("env");
        let uid = env
            .iter()
            .find(|var| var["name"] == "THORIUM_JOB_UID")
            .expect("the Job's UID");
        assert_eq!(
            uid["valueFrom"]["fieldRef"]["fieldPath"],
            "metadata.labels['batch.kubernetes.io/controller-uid']",
            "{:?}",
            spec.kind
        );
    }
    // the kinds are the ones the operator finds its Jobs by
    assert_eq!(
        status_spec(&meta, "plan", None, "n").expect("spec").kind,
        JobKind::SchemaStatus
    );
    assert_eq!(
        record_spec(&meta, &revision, false, "h")
            .expect("spec")
            .kind,
        JobKind::Record
    );
    assert_eq!(
        seed_spec(&meta, &revision).expect("spec").kind,
        JobKind::SchemaApply
    );
    // the status Job asks for a result row by the writing Job's UID
    let with_result = status_spec(&meta, "record-result", Some("uid-9"), "n").expect("spec");
    assert!(
        with_result
            .args
            .windows(2)
            .any(|pair| pair == ["--result", "uid-9"]),
        "{:?}",
        with_result.args
    );
}

/// A status report far bigger than the termination message's 4 KiB reaches the operator intact
/// through the Job's log line
#[tokio::test]
async fn a_large_report_reaches_the_operator_through_the_log_line() {
    // a record with 200 history entries
    let cluster = cluster(json!({}));
    let history = (0..200)
        .map(|second| entry(RELEASE, "other-step", StepState::Blocked, second))
        .collect();
    let big = report(recorded(record(RELEASE, history, &[])), None);
    assert!(serde_json::to_string(&big).expect("json").len() > 4096);
    let jobs = [status_job("plan", Some("Complete"))];
    let fake = fake(&cluster, None, &jobs, Some(&big));
    let meta = meta_for(cluster.clone(), &fake.client());
    // the report comes back as it was printed
    match read_report(&meta, "plan", None).await.expect("read") {
        Read::Report(read) => assert_eq!(*read, big),
        other => panic!("expected the report, got {other:?}"),
    }
    // and the finished Job is deleted so the next read runs a fresh one
    assert!(
        fake.writes()
            .iter()
            .any(|request| request.method == "DELETE"),
        "{:?}",
        fake.writes()
    );
}

/// A gated step that blocked and was then fixed by hand is recorded `Skipped` with the note
/// `fixed manually`, never `Done`, so the copy rule doesn't strand it
#[test]
fn a_gated_step_fixed_by_hand_is_credited_as_skipped() {
    // a skip after a block is a manual fix
    let credited = super::credit_manual_fix(
        super::steps::StepResult::Skipped("nothing to do".to_owned()),
        true,
    );
    match credited {
        super::steps::StepResult::Skipped(details) => {
            assert!(details.starts_with("fixed manually: "), "{details}");
        }
        other => panic!("expected a skip, got {other:?}"),
    }
    // a skip with no earlier block stays a skip, as before
    let plain = super::credit_manual_fix(
        super::steps::StepResult::Skipped("nothing to do".to_owned()),
        false,
    );
    assert!(
        matches!(plain, super::steps::StepResult::Skipped(details) if details == "nothing to do")
    );
}

/// A starting revision that is still set once a record exists is reported as stale in the status,
/// and changes nothing
#[tokio::test]
async fn a_leftover_starting_revision_is_reported_stale() {
    // a state that mirrors a record, and a spec that still names a starting revision
    let cluster = cluster(json!({"starting_revision": BASE}));
    let current = mirrored(&latest().to_string(), Vec::new());
    let fake = fake(&cluster, Some(&current), &[], None);
    assert!(matches!(gated(&fake, &cluster).await, Gate::Current));
    // the status lists it among the stale entries, and nothing but status was written
    let stale = status_patches(&fake)
        .iter()
        .filter_map(|status| status["upgrade"]["stale_approvals"].as_array().cloned())
        .flatten()
        .filter_map(|entry| entry.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        stale
            .iter()
            .any(|entry| entry.contains("starting_revision") && entry.contains(BASE)),
        "{stale:?}"
    );
    assert!(
        fake.writes()
            .iter()
            .all(|request| request.method == "PATCH"),
        "{:?}",
        fake.writes()
    );
}

/// The finalize step runs the record Job instead of recording the step in the state, and the
/// state's revision stays where it is until the Job's result mirrors the record
#[tokio::test]
async fn the_finalize_step_runs_the_record_job() {
    // a plan whose only step left is finalize, over a record that holds the others
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let entries = vec![
        entry(RELEASE, "scylla-role", StepState::Done, 1),
        entry(RELEASE, "elastic-identity", StepState::Done, 2),
        entry(
            RELEASE,
            "elastic-reindex-keyword-mappings",
            StepState::Skipped,
            3,
        ),
    ];
    let state = mirrored(BASE, entries.clone());
    let finished = thorium::models::schema::finished_steps(
        &entries,
        &thorium::models::upgrades::Catalog::PRODUCTION,
    );
    let mut held = record(BASE, entries, &[]);
    held.finished = finished;
    let status = report(recorded(held), None);
    let pending = pending_of(&cluster, &state).await;
    assert_eq!(
        pending
            .steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        ["finalize"]
    );
    // the record is read, then the step creates the Job and waits
    let jobs = [status_job("plan", Some("Complete"))];
    let fake = fake(&cluster, Some(&state), &jobs, Some(&status));
    let meta = meta_for(cluster.clone(), &fake.client());
    let action = super::run(&meta, pending).await.expect("run");
    assert!(action.is_some(), "the upgrade waits for the Job");
    // one record Job was created, for the revision the edge reaches
    let created = created_jobs(&fake);
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(
        created[0]["metadata"]["labels"]["thorium.sandia.gov/job-kind"],
        "record"
    );
    let args = created[0]["spec"]["template"]["spec"]["containers"][0]["args"]
        .as_array()
        .expect("args");
    let args = args.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    assert!(
        args.windows(2).any(|pair| pair == ["--revision", RELEASE]),
        "{args:?}"
    );
    // the state wasn't advanced, and the operator recorded no finalize entry of its own
    let after = written(&fake);
    assert!(
        after
            .as_ref()
            .is_none_or(|state| state.revision == rev(BASE)
                && state
                    .applied
                    .iter()
                    .all(|applied| applied.step != "finalize")),
        "{after:?}"
    );
}

/// A log with a second result line, which the Job's own output never holds, is trusted for
/// neither
#[tokio::test]
async fn a_second_result_line_is_not_trusted() {
    // the status Job's log holds its report twice
    let cluster = cluster(json!({}));
    let finished = report(RecordStatus::NoKeyspace, None);
    let line = log_of(&finished);
    let twice = format!("{line}{line}");
    let jobs = [status_job("record", Some("Complete"))];
    let fake = fake(&cluster, None, &jobs, None).route(
        "GET",
        &format!("{PODS_PATH}/{POD}/log"),
        200,
        Value::String(twice),
    );
    let meta = meta_for(cluster.clone(), &fake.client());
    // reading it fails instead of choosing one of the lines
    let read = read_report(&meta, "record", None).await;
    assert!(read.is_err(), "{read:?}");
}

/// The record isn't read again for the same plan within a few minutes, so a step blocked on an
/// admin doesn't start a status Job at every recheck
#[tokio::test]
async fn a_recent_read_is_not_repeated() {
    // a plan over a state the operator never read the record for
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let state = mirrored(BASE, Vec::new());
    let held = record(BASE, Vec::new(), &[]);
    let finished = report(recorded(held), None);
    let mut pending = pending_of(&cluster, &state).await;
    // the first refresh reads the record and notes when it did
    let jobs = [status_job("plan", Some("Complete"))];
    let fake_one = fake(&cluster, Some(&state), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_one.client());
    assert!(
        refresh(&meta, &mut pending)
            .await
            .expect("refresh")
            .is_none()
    );
    let noted = written(&fake_one).expect("the read was noted in the state");
    assert!(
        noted.schema.contains_key("record_read"),
        "{:?}",
        noted.schema
    );
    assert_eq!(pending.stored.state, noted);
    // a second refresh right after reads nothing and creates no Job
    let fake_two = fake(&cluster, Some(&noted), &[], None);
    let meta = meta_for(cluster.clone(), &fake_two.client());
    assert!(
        refresh(&meta, &mut pending)
            .await
            .expect("refresh")
            .is_none()
    );
    assert!(
        created_jobs(&fake_two).is_empty(),
        "{:?}",
        created_jobs(&fake_two)
    );
}

/// A read made while another image was deployed is not reused: the new image's thoradm knows its
/// own catalog, so the record is read again at once instead of after the read's validity
#[tokio::test]
async fn a_read_by_another_image_is_not_reused() {
    // a first refresh reads the record while the cluster runs one image
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let state = mirrored(BASE, Vec::new());
    let held = record(BASE, Vec::new(), &[]);
    let finished = report(recorded(held), None);
    let mut pending = pending_of(&cluster, &state).await;
    let jobs = [status_job("plan", Some("Complete"))];
    let fake_one = fake(&cluster, Some(&state), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_one.client());
    assert!(
        refresh(&meta, &mut pending)
            .await
            .expect("refresh")
            .is_none()
    );
    let noted = written(&fake_one).expect("the read was noted in the state");
    // the same cluster with another image version reads the record again and waits for the Job
    let mut upgraded = cluster.clone();
    upgraded.spec.version = format!("{}-next", upgraded.spec.version);
    let fake_two = fake(&upgraded, Some(&noted), &[], None);
    let meta = meta_for(upgraded.clone(), &fake_two.client());
    let action = refresh(&meta, &mut pending).await.expect("refresh");
    assert!(
        action.is_some(),
        "the recent read of another image was reused"
    );
    assert_eq!(
        created_jobs(&fake_two).len(),
        1,
        "{:?}",
        created_jobs(&fake_two)
    );
}

/// A step the full history finished but the state's window lost stays finished on the next
/// reconcile, which reuses the recent read instead of reading the record again
#[tokio::test]
async fn r7b_a_finished_step_outside_the_window_stays_finished_on_the_next_reconcile() {
    // the first reconcile reads a record whose finished set holds a step the state lacks
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let window = mirrored(BASE, Vec::new());
    let held = record(BASE, Vec::new(), &[(RELEASE, FIRST_STEP)]);
    let finished = report(recorded(held), None);
    let mut first = pending_of(&cluster, &window).await;
    let jobs = [status_job("plan", Some("Complete"))];
    let fake_one = fake(&cluster, Some(&window), &jobs, Some(&finished));
    let meta = meta_for(cluster.clone(), &fake_one.client());
    assert!(refresh(&meta, &mut first).await.expect("refresh").is_none());
    let noted = written(&fake_one).expect("the read was noted in the state");
    // the next reconcile gates from that state, whose window still lacks the step
    let mut second = pending_of(&cluster, &noted).await;
    assert!(
        second.steps.iter().any(|step| step.step == FIRST_STEP),
        "control"
    );
    let fake_two = fake(&cluster, Some(&noted), &[], None);
    let meta = meta_for(cluster.clone(), &fake_two.client());
    assert!(
        refresh(&meta, &mut second)
            .await
            .expect("refresh")
            .is_none()
    );
    // no read ran, and the plan still leaves the finished step out
    assert_eq!(created_jobs(&fake_two).len(), 0);
    assert!(
        second.steps.iter().all(|step| step.step != FIRST_STEP),
        "{:?}",
        second.steps
    );
}

/// A read marker dated in the future doesn't hold the cache open: the record is read again
#[tokio::test]
async fn r7b_a_future_read_marker_does_not_hold_forever() {
    let cluster = cluster(json!({"target_revision": RELEASE}));
    let mut state = mirrored(BASE, Vec::new());
    state
        .schema
        .insert("record_read".to_owned(), "2999-01-01T00:00:00Z".to_owned());
    let mut pending = pending_of(&cluster, &state).await;
    let fake = fake(&cluster, Some(&state), &[], None);
    let meta = meta_for(cluster.clone(), &fake.client());
    let action = refresh(&meta, &mut pending).await.expect("refresh");
    // a status Job was created to read the record
    assert!(action.is_some());
    assert_eq!(created_jobs(&fake).len(), 1);
}

/// A step the history finished with a `Skipped` entry counts as finished, so planning adds no
/// entry for it
#[test]
fn r7b_planning_treats_a_skipped_entry_as_finished() {
    let held = record(BASE, Vec::new(), &[(RELEASE, FIRST_STEP)]);
    let window = mirrored(
        BASE,
        vec![entry(RELEASE, FIRST_STEP, StepState::Skipped, 1)],
    );
    let planning = super::record::planning_state(&held, &window);
    assert_eq!(planning.applied.len(), window.applied.len());
}

/// A read of the record is moved to lapse soon only when it would last longer, and a state never
/// read is left alone
#[test]
fn a_read_can_be_made_to_lapse_soon() {
    // a state read just now would stay good for a quarter of an hour
    let mut state = UpgradeState::new(rev(RELEASE), "1.8.1");
    assert!(!super::record::read_again_within(&mut state, 60));
    state
        .schema
        .insert("record_read".to_owned(), Utc::now().to_rfc3339());
    assert!(super::record::read_again_within(&mut state, 60));
    // the read now lapses within a minute
    let at = DateTime::parse_from_rfc3339(&state.schema["record_read"])
        .expect("a time")
        .with_timezone(&Utc);
    let age = (Utc::now() - at).num_seconds();
    assert!((830..=850).contains(&age), "{age}");
    // moving it again changes nothing
    assert!(!super::record::read_again_within(&mut state, 60));
}

/// A mirror forgets the schema-apply markers of an apply the record no longer holds, so the next
/// upgrade applies the schema again, and keeps them while the record holds that apply
#[test]
fn a_mirror_forgets_an_apply_the_record_replaced() {
    // a state that remembers an apply of one deployment fingerprint
    let mut remembered = UpgradeState::new(rev(RELEASE), "1.8.1");
    remembered.schema.insert(
        APPLY_KEY_MARKER.to_owned(),
        "image=new;revision=r".to_owned(),
    );
    remembered
        .schema
        .insert(DEPLOYMENT_MARKER.to_owned(), "newer".to_owned());
    // a record that holds the same fingerprint keeps what the state remembers
    let same = record(RELEASE, Vec::new(), &[]);
    let mut held = same.clone();
    held.schema.deployment = print("newer");
    let kept = super::record::mirror(&held, Some(&remembered));
    assert!(kept.schema.contains_key(APPLY_KEY_MARKER));
    assert!(kept.schema.contains_key(DEPLOYMENT_MARKER));
    // a record holding another fingerprint, such as an older revision's after a revert, forgets
    // it: the key names the record's schema, which no apply's trigger key matches
    let forgotten = super::record::mirror(&same, Some(&remembered));
    let key = forgotten
        .schema
        .get(APPLY_KEY_MARKER)
        .expect("a key naming the record");
    assert_eq!(key, &format!("record:{}", same.schema.deployment.as_str()));
    assert!(!forgotten.schema.contains_key(DEPLOYMENT_MARKER));
}
