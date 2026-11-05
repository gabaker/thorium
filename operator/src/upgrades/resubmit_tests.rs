//! Tests of resubmitting the requests a drain recorded: the Job created inside the conversion pause
//! with its in-pause flag, the hold on later steps and on restoring paused components while it is
//! due, its retries, the refresh annotation that runs it later, and the condition it raises, all on
//! the scene of the migration tests with the production drain

use thorium::models::upgrades::{
    QUIESCE_STEP, RESET_STATE_STEP, RESTORED_STEP, UPGRADE_STOPPED_STEP,
};

use super::*;
use crate::upgrades::migration::REFRESH_ANNOTATION;
use crate::upgrades::resubmit::{self, RESUBMIT_STEP};

/// The drain's step id
const DRAIN: &str = "drain-reactions";

/// The image conversion's step id
const IMAGES: &str = "images-to-tools";

/// The step of the revision after the drain's
const LATER: &str = "later-v3";

/// The `at` of the drain's `Running` entry, which is the attempt its requests are tagged with
const DRAIN_AT: &str = "2026-11-05T12:01:01+00:00";

/// The work the drain's run recorded: three requests
const CANCEL_WORK: &str = "4 reactions cancelled; requests: 3 recorded, 0 kept from an earlier \
                           run, 1 refused, 0 stale; 2 workers removed";

/// The work a resubmission that finished records
const RESUBMIT_WORK: &str = "resubmitted 2, refused 1, stale 0";

/// The components the conversion pause holds, with the counts they ran at
const COUNTS: [(&str, i32); 5] = [
    ("api", 3),
    ("scaler", 2),
    ("baremetal-scaler", 1),
    ("event-handler", 1),
    ("search-streamer", 1),
];

/// The components the conversion pause holds, all at zero
const STOPPED: [(&str, i32); 5] = [
    ("api", 0),
    ("scaler", 0),
    ("baremetal-scaler", 0),
    ("event-handler", 0),
    ("search-streamer", 0),
];

/// The step after the drain's revision, which reads nothing
static LATER_STEPS: [StepDef; 1] = [gated(LATER, &[], &[])];

/// Parse a time
///
/// # Arguments
///
/// * `at` - The time as RFC 3339
fn at(at: &str) -> DateTime<Utc> {
    // a fixed moment the test names
    DateTime::parse_from_rfc3339(at)
        .expect("a time")
        .with_timezone(&Utc)
}

/// The agentic catalog with a revision after the drain's
fn later_catalog() -> Catalog {
    // the drain's edge, then one more
    let mut edges = agentic_catalog().edges.to_vec();
    edges.push(edge(Some("2026-10-v02"), "2026-10-v03", &LATER_STEPS));
    Catalog {
        edges: Box::leak(edges.into_boxed_slice()),
        floors: &[],
    }
}

/// Build a history entry
///
/// # Arguments
///
/// * `step` - The entry's step
/// * `outcome` - Its outcome
/// * `when` - Its `at`
fn entry(step: &str, outcome: StepState, when: &str) -> AppliedStep {
    // an entry of the drain's revision
    AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: step.to_owned(),
        outcome,
        at: Some(when.to_owned()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        ..AppliedStep::default()
    }
}

/// Get the counts of some components as a pause records them
///
/// # Arguments
///
/// * `names` - The components' Deployment names
fn counts(names: &[&str]) -> BTreeMap<String, i32> {
    // the counts the components ran at
    COUNTS
        .iter()
        .filter(|(name, _)| names.contains(name))
        .map(|(name, count)| ((*name).to_owned(), *count))
        .collect()
}

/// The history of an approved drain that recorded requests, then the conversion that kept the pause
fn drained_history() -> Vec<AppliedStep> {
    // the quiesce, the drain's run with the whole pause, and the conversion
    let all = COUNTS.map(|(name, _)| name);
    vec![
        AppliedStep {
            paused: counts(&["scaler", "baremetal-scaler"]),
            chain: vec![DRAIN.to_owned()],
            ..entry(
                QUIESCE_STEP,
                StepState::Running,
                "2026-11-05T12:00:00+00:00",
            )
        },
        AppliedStep {
            paused: counts(&all),
            ..entry(DRAIN, StepState::Running, DRAIN_AT)
        },
        AppliedStep {
            job: Some("migration-drain-reactions-0a1b2c3d4e5f".to_owned()),
            work: Some(CANCEL_WORK.to_owned()),
            ..entry(DRAIN, StepState::Done, "2026-11-05T12:05:00+00:00")
        },
        AppliedStep {
            paused: counts(&all),
            ..entry(IMAGES, StepState::Running, "2026-11-05T12:06:00+00:00")
        },
        entry(IMAGES, StepState::Done, "2026-11-05T12:10:00+00:00"),
    ]
}

/// Build a scene at the drain's revision with a history and Deployments
///
/// # Arguments
///
/// * `catalog` - The catalog to plan with
/// * `target` - The target revision
/// * `history` - The recorded history
/// * `deployments` - The Deployments and their replicas
fn scene_with(
    catalog: Catalog,
    target: &str,
    history: Vec<AppliedStep>,
    deployments: &[(&str, i32)],
) -> Scene {
    // the clock after the conversion finished
    set_now(at("2026-11-05T12:30:00Z"));
    let mut scene = Scene::new(catalog, &json!([]), &[]);
    scene.retarget(Some(target));
    for (name, replicas) in deployments {
        scene.sim.add_deployment(name, *replicas);
    }
    // the state at the drain's revision, mirroring a record
    let mut state = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    for applied in history {
        state.record(applied);
    }
    scene.sim.set_state(&state);
    scene
}

/// Build a scene whose drain recorded requests and whose conversion pause still holds, at its target
fn paused_scene() -> Scene {
    // every component at zero
    scene_with(
        agentic_catalog(),
        "2026-10-v02",
        drained_history(),
        &STOPPED,
    )
}

/// Get the resubmit Jobs created, as posted
///
/// # Arguments
///
/// * `scene` - The scene
fn resubmits(scene: &Scene) -> Vec<Value> {
    // the migration Jobs running the resubmission
    migrations(scene)
        .into_iter()
        .filter(|job| Scene::args(job).iter().any(|arg| arg == RESUBMIT_STEP))
        .collect()
}

/// Get the name of a created Job
///
/// # Arguments
///
/// * `job` - The Job as posted
fn name_of(job: &Value) -> String {
    // the name in its metadata
    job["metadata"]["name"].as_str().expect("a name").to_owned()
}

/// End a resubmit Job that left no request to retry, with the entry thoradm wrote in the record
///
/// # Arguments
///
/// * `scene` - The scene
/// * `job` - The Job's name
/// * `message` - The message of thoradm's entry
fn succeed(scene: &Scene, job: &str, message: &str) {
    // thoradm's entry is in the record the status Job reads
    let mut state = scene.sim.state().expect("a state");
    state.record(AppliedStep {
        message: Some(message.to_owned()),
        work: Some(RESUBMIT_WORK.to_owned()),
        by: Some("thoradm 1.8.1".to_owned()),
        job: Some(job.to_owned()),
        chain: vec![RESUBMIT_STEP.to_owned()],
        ..entry(
            RESUBMIT_STEP,
            StepState::Done,
            "2026-11-05T12:31:00.000000000Z",
        )
    });
    scene
        .sim
        .set_record(json!({"state": "recorded", "record": record_of(&state)}));
    let summary = json!({
        "outcome": "done",
        "migration": RESUBMIT_STEP,
        "counts": PassCounts { read: 3, migratable: 2, written: 2, ..PassCounts::default() },
        "groups": [],
    });
    scene.sim.finish(job, &Finish::Complete(summary));
}

/// End a resubmit Job that failed on a transient error, leaving two requests to retry
///
/// # Arguments
///
/// * `scene` - The scene
/// * `job` - The Job's name
fn fail(scene: &Scene, job: &str) {
    // the summary counts the requests it couldn't finish as errors
    let summary = json!({
        "outcome": "transient",
        "migration": RESUBMIT_STEP,
        "counts": PassCounts { read: 3, migratable: 1, error: 2, written: 1, ..PassCounts::default() },
        "groups": [],
    });
    scene.sim.finish(
        job,
        &Finish::Failed {
            reason: "BackoffLimitExceeded".to_owned(),
            summary: Some(summary),
            log_lines: 3,
        },
    );
}

/// Get the `ReactionsResubmitted` condition's status and message
///
/// # Arguments
///
/// * `scene` - The scene
fn condition(scene: &Scene) -> Option<(String, String)> {
    // the status the operator last wrote
    scene
        .sim
        .status()
        .upgrade
        .and_then(|upgrade| {
            upgrade
                .conditions
                .into_iter()
                .find(|condition| condition.kind == UpgradeConditionType::ReactionsResubmitted)
        })
        .map(|condition| (condition.status, condition.message))
}

/// Build the cluster metadata the scene's reconciles use
///
/// # Arguments
///
/// * `scene` - The scene
fn meta(scene: &Scene) -> crate::k8s::clusters::ClusterMeta {
    // the cluster with the status the last reconcile left
    let mut cluster = scene.cluster.clone();
    cluster.status = Some(scene.sim.status());
    meta_for(cluster, &scene.sim.fake().client())
}

/// While the resubmission is due inside the pause, its Job runs with the in-pause flag before any
/// later step starts or any component is restored, and once it finished the plan goes on
#[tokio::test]
async fn the_resubmission_runs_before_the_pause_ends() {
    // the drain's revision is recorded and the next revision is the target
    let scene = scene_with(later_catalog(), "2026-10-v03", drained_history(), &STOPPED);
    scene.rounds(3).await;
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(
        Scene::flag(&jobs[0], "--in-pause").as_deref(),
        Some(DRAIN_AT)
    );
    assert!(scene.reconcile().await.is_some());
    // nothing of the later step ran and nothing was scaled
    let later = |scene: &Scene| {
        scene
            .sim
            .created()
            .into_iter()
            .filter(|job| Scene::args(job).iter().any(|arg| arg == LATER))
            .count()
    };
    assert_eq!(later(&scene), 0);
    assert_eq!(scene.history(LATER), Vec::<AppliedStep>::new());
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
    // once it finished the later step is checked
    succeed(&scene, &name_of(&jobs[0]), "Resubmitted the requests");
    scene.rounds(4).await;
    assert_eq!(resubmits(&scene).len(), 1);
    assert!(later(&scene) > 0);
}

/// The in-pause flag is passed only when the state shows the whole pause held with no restore
/// after the drain's run and every component is at zero with no live replica; a quiesce withdrawn
/// earlier in the attempt doesn't take it away
#[tokio::test]
async fn only_a_job_created_in_the_pause_gets_the_flag() {
    /// Get the in-pause flag the first resubmit Job of a scene gets
    ///
    /// # Arguments
    ///
    /// * `scene` - The scene
    async fn flag_of(scene: &Scene) -> Option<String> {
        // the one resubmit Job a few reconciles create
        scene.rounds(2).await;
        let jobs = resubmits(scene);
        assert_eq!(jobs.len(), 1, "{jobs:#?}");
        Scene::flag(&jobs[0], "--in-pause")
    }
    // a quiesce withdrawn before the approved one doesn't count
    let mut withdrawn = vec![
        AppliedStep {
            paused: counts(&["scaler", "baremetal-scaler"]),
            chain: vec![DRAIN.to_owned()],
            ..entry(
                QUIESCE_STEP,
                StepState::Running,
                "2026-11-05T11:00:00+00:00",
            )
        },
        AppliedStep {
            paused: counts(&["scaler", "baremetal-scaler"]),
            ..entry(RESTORE_STEP, StepState::Done, "2026-11-05T11:01:00+00:00")
        },
    ];
    withdrawn.extend(drained_history());
    let scene = scene_with(agentic_catalog(), "2026-10-v02", withdrawn, &STOPPED);
    assert_eq!(flag_of(&scene).await.as_deref(), Some(DRAIN_AT));
    // a restore after the drain's run, though the conversion paused the component again
    let mut restored = drained_history();
    restored.insert(
        3,
        AppliedStep {
            paused: counts(&["event-handler"]),
            ..entry(RESTORE_STEP, StepState::Done, "2026-11-05T12:05:30+00:00")
        },
    );
    let scene = scene_with(agentic_catalog(), "2026-10-v02", restored, &STOPPED);
    assert_eq!(flag_of(&scene).await, None);
    // a component whose spec asks for replicas though the state shows it paused
    let scene = paused_scene();
    scene.sim.add_deployment("api", 2);
    assert_eq!(flag_of(&scene).await, None);
    // a scaler at zero in its spec whose pod is still live
    let scene = paused_scene();
    scene.sim.set_replicas("scaler", 0, 1);
    assert_eq!(flag_of(&scene).await, None);
}

/// A drain whose first run failed and whose retry finished is resubmitted as one attempt under the
/// failed run's start, with the requests the retry counted as kept from it
#[tokio::test]
async fn a_retried_drain_is_resubmitted_under_its_first_start() {
    // the quiesce, the failed run, and the retry that kept the failed run's three requests
    let mut history = drained_history();
    let all = COUNTS.map(|(name, _)| name);
    let retry_work = "1 reactions cancelled; requests: 0 recorded, 3 kept from an earlier run, 0 \
                      refused, 0 stale; 0 workers removed";
    history.splice(
        2..3,
        [
            AppliedStep {
                job: Some("migration-drain-reactions-0a1b2c3d4e5f".to_owned()),
                ..entry(DRAIN, StepState::Failed, "2026-11-05T12:02:00+00:00")
            },
            AppliedStep {
                paused: counts(&all),
                backup: Some("snap-2".to_owned()),
                ..entry(DRAIN, StepState::Running, "2026-11-05T12:03:00+00:00")
            },
            AppliedStep {
                job: Some("migration-drain-reactions-9f8e7d6c5b4a".to_owned()),
                work: Some(retry_work.to_owned()),
                ..entry(DRAIN, StepState::Done, "2026-11-05T12:05:00+00:00")
            },
        ],
    );
    let scene = scene_with(agentic_catalog(), "2026-10-v02", history, &STOPPED);
    // the resubmission is due under the first run's start, with the kept requests
    let due = resubmit::due(&scene.sim.state().expect("a state")).expect("due");
    assert_eq!((due.at.as_str(), due.requests), (DRAIN_AT, 3));
    // and its Job runs inside the pause for that attempt
    scene.rounds(2).await;
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(
        Scene::flag(&jobs[0], "--in-pause").as_deref(),
        Some(DRAIN_AT)
    );
}

/// While a flagged resubmit Job runs, applying the new release restores no paused component, and
/// once it finished the components get their counts back
#[tokio::test]
async fn no_component_is_restored_while_a_flagged_job_runs() {
    // the flagged Job is running
    let scene = paused_scene();
    scene.rounds(2).await;
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 1);
    assert!(Scene::flag(&jobs[0], "--in-pause").is_some());
    // the reconcile is held and an apply restores nothing
    assert!(scene.reconcile().await.is_some());
    let meta = meta(&scene);
    for (name, _) in COUNTS {
        pause::restore_applied(&meta, name).await.expect("restore");
    }
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
    assert_eq!(scene.history(RESTORE_STEP), Vec::<AppliedStep>::new());
    // once it finished the apply restores each component's recorded count
    succeed(&scene, &name_of(&jobs[0]), "Resubmitted the requests");
    scene.rounds(3).await;
    assert!(scene.reconcile().await.is_none());
    pause::restore_applied(&meta, "api").await.expect("restore");
    assert_eq!(scene.sim.replicas("api"), Some(3));
}

/// A resubmission that finished isn't run again, by later reconciles or by the refresh annotation
#[tokio::test]
async fn the_resubmit_job_runs_once_per_attempt() {
    // the Job runs and finishes
    let mut scene = paused_scene();
    scene.rounds(2).await;
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 1);
    succeed(&scene, &name_of(&jobs[0]), "Resubmitted the requests");
    scene.rounds(5).await;
    // thoradm's entry is in the state, and nothing more runs
    let done = scene
        .history(RESUBMIT_STEP)
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Done)
        .count();
    assert_eq!(done, 1);
    scene.annotate(REFRESH_ANNOTATION, "again");
    scene.rounds(3).await;
    assert_eq!(resubmits(&scene).len(), 1);
    assert!(resubmit::due(&scene.sim.state().expect("a state")).is_none());
}

/// A state rebuilt from the record, which holds the drain's entries but not the operator's own,
/// still follows the resubmission: the running Job is taken over rather than run twice, and a state
/// rebuilt after it finished doesn't run it again
#[tokio::test]
async fn the_resubmission_runs_after_a_lost_state_configmap() {
    // the flagged Job is running when the state is lost and rebuilt from the record
    let scene = paused_scene();
    scene.rounds(2).await;
    let rebuilt = scene_state(drained_history());
    scene.sim.set_state(&rebuilt);
    scene.rounds(3).await;
    // the running Job is followed, not run again
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    let job = name_of(&jobs[0]);
    assert!(
        scene
            .history(RESUBMIT_STEP)
            .iter()
            .any(|entry| entry.job.as_deref() == Some(job.as_str())),
        "{:#?}",
        scene.history(RESUBMIT_STEP)
    );
    assert!(resubmit::holds(&scene.sim.state().expect("a state")));
    // it finishes, and a state rebuilt after that holds thoradm's entry, so nothing runs again
    succeed(&scene, &job, "Resubmitted the requests");
    scene.rounds(3).await;
    assert_eq!(
        condition(&scene).map(|(status, _)| status).as_deref(),
        Some("True")
    );
    let mut recorded = drained_history();
    recorded.extend(
        scene
            .history(RESUBMIT_STEP)
            .into_iter()
            .filter(|entry| entry.outcome == StepState::Done),
    );
    scene.sim.set_state(&scene_state(recorded));
    scene.rounds(3).await;
    assert_eq!(resubmits(&scene).len(), 1);
}

/// Build a state at the drain's revision holding a history, as one rebuilt from the record
///
/// # Arguments
///
/// * `history` - The history the record holds
fn scene_state(history: Vec<AppliedStep>) -> UpgradeState {
    // the revision and the history, mirroring the record
    let mut state = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    for applied in history {
        state.record(applied);
    }
    state
}

/// A restore, a reset, or a stopped upgrade after the drain ends its attempt, so nothing is
/// resubmitted on its own and nothing holds the pause
#[tokio::test]
async fn a_restored_cluster_does_not_resubmit_on_its_own() {
    for step in [RESTORED_STEP, RESET_STATE_STEP, UPGRADE_STOPPED_STEP] {
        // the entry follows the drain's
        let mut history = drained_history();
        history.push(entry(step, StepState::Done, "2026-11-05T12:20:00+00:00"));
        let mut scene = scene_with(agentic_catalog(), "2026-10-v02", history, &STOPPED);
        scene.rounds(3).await;
        scene.annotate(REFRESH_ANNOTATION, "1");
        scene.rounds(3).await;
        let state = scene.sim.state().expect("a state");
        assert!(resubmit::due(&state).is_none(), "{step}");
        assert!(!resubmit::holds(&state), "{step}");
        assert_eq!(resubmits(&scene).len(), 0, "{step}");
    }
}

/// A resubmit Job that fails is run again inside the pause up to three Jobs, then the pause is no
/// longer held: the condition names the last Job and what it left to retry, and the resubmission
/// stays due
#[tokio::test]
async fn a_failed_resubmit_is_retried_three_times_then_the_pause_ends() {
    // three Jobs, each failing
    let scene = paused_scene();
    let mut last = String::new();
    for count in 1..=3 {
        scene.rounds(2).await;
        let jobs = resubmits(&scene);
        assert_eq!(jobs.len(), count, "{jobs:#?}");
        assert!(Scene::flag(&jobs[count - 1], "--in-pause").is_some());
        last = name_of(&jobs[count - 1]);
        fail(&scene, &last);
    }
    scene.rounds(4).await;
    // no fourth Job, and nothing holds
    assert_eq!(resubmits(&scene).len(), 3);
    let state = scene.sim.state().expect("a state");
    assert!(resubmit::due(&state).is_some());
    assert!(!resubmit::holds(&state));
    assert!(scene.reconcile().await.is_none());
    let meta = meta(&scene);
    pause::restore_applied(&meta, "api").await.expect("restore");
    assert_eq!(scene.sim.replicas("api"), Some(3));
    // the condition names the last Job and the requests to retry
    let (status, message) = condition(&scene).expect("a condition");
    assert_eq!(status, "False");
    assert!(message.contains(&last), "{message}");
    assert!(message.contains("2 error"), "{message}");
    assert!(message.contains(REFRESH_ANNOTATION), "{message}");
}

/// Outside the pause a resubmission that is still due runs only when the refresh annotation
/// changes, without the in-pause flag and without holding the reconcile
#[tokio::test]
async fn a_later_failed_resubmit_waits_for_the_refresh_annotation() {
    // the pause ended with the rollout before the resubmission finished
    let mut history = drained_history();
    for (name, count) in COUNTS {
        history.push(AppliedStep {
            paused: BTreeMap::from([(name.to_owned(), count)]),
            ..entry(RESTORE_STEP, StepState::Done, "2026-11-05T12:15:00+00:00")
        });
    }
    let mut scene = scene_with(agentic_catalog(), "2026-10-v02", history, &COUNTS);
    scene.rounds(3).await;
    assert_eq!(resubmits(&scene).len(), 0);
    // the annotation runs one Job, unflagged, and the reconcile goes on
    scene.annotate(REFRESH_ANNOTATION, "1");
    assert!(scene.reconcile().await.is_none());
    scene.rounds(2).await;
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(Scene::flag(&jobs[0], "--in-pause"), None);
    assert!(!Scene::args(&jobs[0]).iter().any(|arg| arg == "--in-pause"));
    // it fails, and nothing runs until the annotation changes again
    fail(&scene, &name_of(&jobs[0]));
    scene.rounds(4).await;
    assert_eq!(resubmits(&scene).len(), 1);
    scene.annotate(REFRESH_ANNOTATION, "2");
    scene.rounds(2).await;
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 2, "{jobs:#?}");
    assert_eq!(Scene::flag(&jobs[1], "--in-pause"), None);
}

/// The condition carries counts only: the work thoradm recorded when the resubmission finished, and
/// the Job and its counts when it failed, never a name from the requests
#[tokio::test]
async fn the_condition_holds_counts_only() {
    // a failed Job first
    let scene = paused_scene();
    scene.rounds(2).await;
    let first = name_of(&resubmits(&scene)[0]);
    fail(&scene, &first);
    scene.rounds(2).await;
    let (status, message) = condition(&scene).expect("a condition");
    assert_eq!(status, "False");
    assert!(message.contains(&first), "{message}");
    assert!(message.contains("3 objects read"), "{message}");
    // then one that finishes, whose thoradm entry's message names what the condition must not
    let jobs = resubmits(&scene);
    assert_eq!(jobs.len(), 2, "{jobs:#?}");
    succeed(
        &scene,
        &name_of(&jobs[1]),
        "Resubmitted alice's reaction in pipeline-x",
    );
    scene.rounds(3).await;
    let (status, message) = condition(&scene).expect("a condition");
    assert_eq!(status, "True");
    assert!(message.contains(RESUBMIT_WORK), "{message}");
    for name in ["alice", "pipeline-x", DRAIN_AT] {
        assert!(!message.contains(name), "{message}");
    }
}

/// The counts a drain's run leaves in its `Done` entry make the resubmission due with the requests
/// it recorded and kept, tagged with the `at` of the drain's `Running` entry, and a run that
/// recorded none makes nothing due
#[test]
fn the_cancel_counts_in_the_drains_entry_make_it_due() {
    for (recorded, kept, requests) in [(3, 2, Some(5)), (0, 4, Some(4)), (0, 0, None)] {
        // the drain's run in the production catalog, at its revision
        let report = thorium::models::schema::DrainCancelReport {
            cancelled: 9,
            recorded,
            refused: 1,
            stale: 1,
            kept,
            workers_removed: 4,
            ..thorium::models::schema::DrainCancelReport::default()
        };
        let mut state = UpgradeState::new(rev("2026-10-v04"), "1.8.1");
        state.record(AppliedStep {
            revision: "2026-10-v04".to_owned(),
            ..entry(DRAIN, StepState::Running, DRAIN_AT)
        });
        state.record(AppliedStep {
            revision: "2026-10-v04".to_owned(),
            job: Some("migration-drain-reactions-0a1b2c3d4e5f".to_owned()),
            work: Some(crate::upgrades::cancel::work_text(&report)),
            ..entry(DRAIN, StepState::Done, "2026-11-05T12:05:00+00:00")
        });
        // the requests it recorded or kept are due under its start
        let due = resubmit::due(&state);
        assert_eq!(due.as_ref().map(|due| due.requests), requests, "{state:?}");
        if requests.is_some() {
            assert_eq!(due.map(|due| due.at).as_deref(), Some(DRAIN_AT));
        }
    }
}
