//! Tests of what a hold does while a step's run is open: it stops the run's migration Job in the
//! foreground, records a Job that already ended, names the paused components and the two ways
//! forward, and lets the step resume when the target is set again, all on the scene of the
//! migration tests

use super::*;

/// How long in seconds a hold waits before looking at a stopping Job again
const STOPPING_SECS: u64 = 15;

/// How long in seconds a hold waits once nothing is stopping
const HOLD_SECS: u64 = 3600;

/// Get the deletes of a Job the fake saw, in order
///
/// # Arguments
///
/// * `scene` - The scene
/// * `job` - The Job's name
fn deletes_of(scene: &Scene, job: &str) -> Vec<crate::k8s::clusters::tests::FakeRequest> {
    // the DELETE requests on the Job's path
    scene
        .sim
        .fake()
        .writes()
        .into_iter()
        .filter(|request| request.method == "DELETE" && request.path.ends_with(&format!("/{job}")))
        .collect()
}

/// Get the Job list requests the fake saw
///
/// # Arguments
///
/// * `scene` - The scene
fn job_lists(scene: &Scene) -> usize {
    // the GET requests on the Jobs collection
    scene
        .sim
        .fake()
        .requests()
        .into_iter()
        .filter(|request| request.method == "GET" && request.path.ends_with("/jobs"))
        .count()
}

/// Get the seconds a reconcile asked to wait
///
/// # Arguments
///
/// * `action` - What the reconcile returned
fn secs(action: Option<Action>) -> Option<u64> {
    // the requeue the action carries, matched against each wait a hold uses since an action
    // doesn't expose its delay
    let action = action?;
    [STOPPING_SECS, HOLD_SECS, 60, 5]
        .into_iter()
        .find(|secs| action == Action::requeue(std::time::Duration::from_secs(*secs)))
}

/// Start the fixture step's Job with two paused components, returning the Job's name
///
/// # Arguments
///
/// * `scene` - The scene
async fn running_job(scene: &Scene) -> String {
    // the check finds three objects to migrate and the approval starts the Job
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let jobs = scene.sim.active("migration");
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    jobs[0].clone()
}

/// Check that the status of a held cluster names the stopped step, its Job, the paused components
/// with their counts, and the two ways forward
///
/// # Arguments
///
/// * `scene` - The scene
/// * `phase` - The phase the hold reports
/// * `job` - The stopped Job
fn assert_stopped_status(scene: &Scene, phase: &str, job: &str) {
    // the phase is the hold's, and the message explains the stop
    let status = scene.sim.status_value();
    assert_eq!(status["phase"], phase, "{status:#}");
    let message = status["message"].as_str().unwrap_or_default();
    for part in [
        "users-v2",
        job,
        "api (3)",
        "scaler (1)",
        "set spec.upgrade.target_revision",
        "snap-1",
        "by hand",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
}

/// Run a hold of a cluster whose step's Job runs, and check that the Job is deleted once in the
/// foreground pinned to the version read, the components stay at zero, the stop is audited as a
/// block, and the hold looks again soon while the Job stops and hourly once it is gone
///
/// # Arguments
///
/// * `target` - The target the admin sets, or none to clear it
/// * `phase` - The phase the hold reports
async fn a_hold_stops_the_running_job(target: Option<&str>, phase: &str) {
    // a running Job whose delete leaves it terminating until its pod has checkpointed
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    scene.sim.slow_delete();
    let job = running_job(&scene).await;
    // the admin changes the target
    scene.retarget(target);
    let action = scene.reconcile().await;
    // the Job is deleted once, in the foreground, pinned to its UID and the version read
    let deletes = deletes_of(&scene, &job);
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].body["propagationPolicy"], "Foreground");
    assert_eq!(
        deletes[0].body["preconditions"]["uid"],
        format!("uid-{job}")
    );
    assert!(
        deletes[0].body["preconditions"]["resourceVersion"].is_string(),
        "{deletes:?}"
    );
    // the components stay paused, and the status says why and what to do
    assert_eq!(scene.sim.replicas("api"), Some(0));
    assert_eq!(scene.sim.replicas("scaler"), Some(0));
    assert_stopped_status(&scene, phase, &job);
    let message = scene.sim.status_value()["message"].to_string();
    assert!(message.contains("to stop"), "{message}");
    // the stop is audited as a block of the step, after its start
    let entries = scene.history("users-v2");
    let outcomes = entries
        .iter()
        .map(|entry| entry.outcome)
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [StepState::Running, StepState::Blocked],
        "{entries:?}"
    );
    assert!(
        entries[1]
            .message
            .as_deref()
            .is_some_and(|message| message.contains(&job)),
        "{entries:?}"
    );
    // the hold looks again soon while the Job stops, and hourly once it is gone
    assert_eq!(secs(action), Some(STOPPING_SECS));
    scene.sim.reap(&job);
    assert_eq!(secs(scene.reconcile().await), Some(HOLD_SECS));
    assert_stopped_status(&scene, phase, &job);
    // no Job was created again and no component was scaled up
    assert_eq!(migrations(&scene).len(), 1);
    assert!(
        scene
            .sim
            .scaled()
            .iter()
            .all(|(_, replicas)| *replicas == 0),
        "{:?}",
        scene.sim.scaled()
    );
}

/// Clearing the target while a step's Job runs stops the Job and keeps the components paused
#[tokio::test]
async fn clearing_the_target_stops_the_running_job() {
    // a cleared target holds the cluster in UpgradeRequired
    a_hold_stops_the_running_job(None, "UpgradeRequired").await;
}

/// Setting the target to the deployment's revision while a step's Job runs stops the Job
#[tokio::test]
async fn setting_the_target_to_the_current_revision_stops_the_running_job() {
    // a target at the recorded revision holds the cluster in UpgradeRequired
    a_hold_stops_the_running_job(Some("2026-10-v01"), "UpgradeRequired").await;
}

/// Setting a target the operator can't use while a step's Job runs stops the Job
#[tokio::test]
async fn an_unusable_target_stops_the_running_job() {
    // a target this build doesn't know holds the cluster in Error
    a_hold_stops_the_running_job(Some("2026-10-v09"), "Error").await;
}

/// A stopped step resumes under the same approval when the target is set again: no second start
/// entry and no second pause, while the old Job still stops the step waits for it, and the new Job
/// keeps the run's name and finishes the step
#[tokio::test]
async fn a_stopped_step_resumes_when_the_target_is_set_again() {
    // a step whose Job was stopped by a cleared target
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    scene.sim.slow_delete();
    let job = running_job(&scene).await;
    let scaled = scene.sim.scaled().len();
    scene.retarget(None);
    scene.reconcile().await;
    // the target is set again while the old Job still stops: the step waits and starts nothing
    scene.retarget(Some("2026-10-v02"));
    scene.rounds(2).await;
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Running, "{step:?}");
    assert!(
        step.message
            .as_deref()
            .is_some_and(|message| message.contains("to stop")),
        "{step:?}"
    );
    assert_eq!(migrations(&scene).len(), 1);
    // once it is gone a Job of the same name resumes the run, without breaking a lease
    scene.sim.reap(&job);
    scene.rounds(2).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    assert_eq!(created[1]["metadata"]["name"], job.as_str());
    assert!(
        !Scene::args(&created[1]).contains(&"--force-break-lease".to_owned()),
        "{:?}",
        Scene::args(&created[1])
    );
    // the run kept its one start entry and its first counts, and nothing was paused again
    let running = scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Running)
        .collect::<Vec<_>>();
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].paused.get("api"), Some(&3));
    assert_eq!(scene.sim.scaled().len(), scaled);
    // the resumed Job finishes the step
    scene.sim.finish(
        &job,
        &Finish::Complete(
            json!({"outcome": "done", "resumed": true, "migration": "users-v2",
            "counts": {"read": 10, "current": 9, "migratable": 1, "written": 1}}),
        ),
    );
    scene.rounds(2).await;
    let entries = scene.history("users-v2");
    assert_eq!(
        entries.last().map(|entry| entry.outcome),
        Some(StepState::Done),
        "{entries:?}"
    );
}

/// A Job that completed before the hold is recorded by the hold with its approval and work, and
/// nothing is deleted; the next attempt plans past the step
#[tokio::test]
async fn a_job_that_finished_before_the_hold_is_recorded_by_the_hold() {
    // a Job that completes, and a target cleared before the next reconcile sees it
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let job = running_job(&scene).await;
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 3, &[])));
    scene.retarget(None);
    let action = scene.reconcile().await;
    // the hold records the run as done, with its Job and backup, and deletes nothing
    let entries = scene.history("users-v2");
    let done = entries.last().expect("an entry");
    assert_eq!(done.outcome, StepState::Done, "{entries:?}");
    assert_eq!(done.job.as_deref(), Some(job.as_str()));
    assert_eq!(done.backup.as_deref(), Some("snap-1"));
    assert!(deletes_of(&scene, &job).is_empty());
    assert_eq!(secs(action), Some(HOLD_SECS));
    // the components stay paused until the release is applied, and the status names them
    assert_eq!(scene.sim.replicas("api"), Some(0));
    let message = scene.sim.status_value()["message"].to_string();
    assert!(message.contains("api (3)"), "{message}");
    // the step is no longer planned once the target is set again
    scene.retarget(Some("2026-10-v02"));
    scene.rounds(2).await;
    assert_eq!(migrations(&scene).len(), 1);
    assert_eq!(
        scene.sim.state().expect("state").revision,
        rev("2026-10-v02")
    );
}

/// A Job that failed before the hold is recorded failed by the hold, so the step stays failed under
/// the same approval once its Job is deleted, as the Job's TTL does
#[tokio::test]
async fn a_job_that_failed_before_the_hold_stays_failed_after_its_job_is_deleted() {
    // a Job that fails over its issue limit, and a target cleared before the next reconcile
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let job = running_job(&scene).await;
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary("failed_issues", 0, &[("bad_type:x", 6)])),
            log_lines: 3,
        },
    );
    scene.retarget(None);
    scene.rounds(2).await;
    // the hold records the failure once, with the Job's name, and keeps the Job
    let failed = scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Failed)
        .collect::<Vec<_>>();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].job.as_deref(), Some(job.as_str()));
    assert!(deletes_of(&scene, &job).is_empty());
    // the Job goes, then the target is set again: the step stays failed and no Job is created
    scene.sim.reap(&job);
    scene.retarget(Some("2026-10-v02"));
    scene.rounds(3).await;
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Failed, "{step:?}");
    assert_eq!(migrations(&scene).len(), 1);
}

/// A Job that reuses an earlier failed run's name and fails before the hold is recorded failed by
/// the hold, so the earlier run's failure doesn't stand in for it and the step stays failed once
/// the Job is deleted
#[tokio::test]
async fn a_reused_name_job_that_failed_before_the_hold_is_recorded_by_the_hold() {
    // two failed runs whose Jobs are reaped, then the first approval again, which reuses the first
    // Job's name
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let failed = fail_two_runs_and_reap_them(&mut scene).await;
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-1"}]));
    scene.rounds(3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 3, "{created:#?}");
    let third = scene.sim.jobs("migration").remove(0);
    assert_eq!(created[0]["metadata"]["name"], third.as_str());
    // the third run's Job fails, and the target is cleared before the next reconcile sees it
    scene.sim.finish(&third, &failed);
    scene.retarget(None);
    scene.reconcile().await;
    // the hold records the third run's failure after its start, beside the first run's
    let entries = scene.history("users-v2");
    let start = entries
        .iter()
        .rposition(|entry| {
            entry.outcome == StepState::Running && entry.job.as_deref() == Some(third.as_str())
        })
        .expect("the third run's start");
    let own = entries[start..]
        .iter()
        .filter(|entry| {
            entry.outcome == StepState::Failed && entry.job.as_deref() == Some(third.as_str())
        })
        .count();
    assert_eq!(own, 1, "{entries:?}");
    // the Job goes, then the target is set again: the step stays failed and no Job is created
    scene.sim.reap(&third);
    scene.retarget(Some("2026-10-v02"));
    scene.rounds(3).await;
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Failed, "{step:?}");
    assert_eq!(migrations(&scene).len(), 3);
}

/// A hold with no open run lists no Jobs
#[tokio::test]
async fn a_plain_hold_lists_no_jobs() {
    // a cluster behind with no target and nothing started
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    scene.retarget(None);
    let action = scene.reconcile().await;
    // the hold waits hourly, reads no Job, and its message names no pause
    assert_eq!(secs(action), Some(HOLD_SECS));
    assert_eq!(job_lists(&scene), 0);
    let message = scene.sim.status_value()["message"].to_string();
    assert!(!message.contains("paused"), "{message}");
}

/// A hold between steps, with the first step done and its components still paused, names them and
/// the two ways forward without listing any Job
#[tokio::test]
async fn a_hold_between_steps_names_the_paused_components() {
    // the first step paused two components and finished; the next waits for its approval
    let mut scene = Scene::new(THREE, &approved(), &[("api", 3), ("scaler", 1)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 3, &[])));
    scene.settle_checks(&report, 3).await;
    assert_eq!(
        scene.history("users-v2").last().map(|entry| entry.outcome),
        Some(StepState::Done)
    );
    // the target is cleared between the steps
    scene.retarget(None);
    let lists = job_lists(&scene);
    let action = scene.reconcile().await;
    // the hold names the paused components, the step that paused them, and the ways forward
    assert_eq!(secs(action), Some(HOLD_SECS));
    let status = scene.sim.status_value();
    assert_eq!(status["phase"], "UpgradeRequired");
    let message = status["message"].as_str().unwrap_or_default();
    for part in [
        "api (3)",
        "scaler (1)",
        "users-v2",
        "set spec.upgrade.target_revision",
        "snap-1",
        "by hand",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
    // nothing ran, so no Job was listed or deleted
    assert_eq!(job_lists(&scene), lists);
    assert!(deletes_of(&scene, &job).is_empty());
}

/// Holds repeated while the Job stops delete it once and keep looking again soon, recording the stop
/// once
#[tokio::test]
async fn a_repeated_hold_deletes_a_stopping_job_once() {
    // a Job whose delete leaves it terminating
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    scene.sim.slow_delete();
    let job = running_job(&scene).await;
    scene.retarget(None);
    // three holds while it stops
    for _ in 0..3 {
        assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    }
    // one delete, and one audit entry of the stop
    assert_eq!(deletes_of(&scene, &job).len(), 1);
    let blocked = scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Blocked)
        .count();
    assert_eq!(blocked, 1);
}

/// A Job that changes between the hold's list and its delete is left for the next reconcile, which
/// sees it as it is: still running, it is deleted then
#[tokio::test]
async fn a_changed_job_is_left_for_the_next_reconcile() {
    // a Job that changes right before the first delete
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    scene.sim.slow_delete();
    let job = running_job(&scene).await;
    scene.sim.change_before_delete(&job);
    scene.retarget(None);
    // the pinned delete conflicts: no error, no stop recorded, and the hold looks again soon
    assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    assert_eq!(deletes_of(&scene, &job).len(), 1);
    assert_eq!(scene.sim.jobs("migration"), std::slice::from_ref(&job));
    assert!(
        scene
            .history("users-v2")
            .iter()
            .all(|entry| entry.outcome != StepState::Blocked)
    );
    // the next reconcile reads it again and deletes it
    assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    assert_eq!(deletes_of(&scene, &job).len(), 2);
    assert_eq!(
        scene.history("users-v2").last().map(|entry| entry.outcome),
        Some(StepState::Blocked)
    );
}

/// Count the `Blocked` entries of the fixture step that name a Job
///
/// # Arguments
///
/// * `scene` - The scene
/// * `job` - The Job's name
fn blocks_of(scene: &Scene, job: &str) -> usize {
    // the step's blocks naming the Job
    scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Blocked && entry.job.as_deref() == Some(job))
        .count()
}

/// An operator that restarted between deleting a running Job and recording its stop records the
/// stop on the next hold, once, without deleting the Job again
#[tokio::test]
async fn a_stop_lost_to_a_restart_is_recorded_once() {
    // a running Job that a previous operator deleted before it could record the stop
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let job = running_job(&scene).await;
    scene.retarget(None);
    scene.sim.terminate(&job);
    // the next hold records the stop, deletes nothing, and looks again soon
    assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    assert!(deletes_of(&scene, &job).is_empty());
    assert_eq!(blocks_of(&scene, &job), 1);
    // a further hold while the Job still stops records nothing more
    assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    assert_eq!(blocks_of(&scene, &job), 1);
    // once the Job is gone the hold waits hourly and still holds one block
    scene.sim.reap(&job);
    assert_eq!(secs(scene.reconcile().await), Some(HOLD_SECS));
    assert_eq!(blocks_of(&scene, &job), 1);
}

/// A Job that completes between the hold's list and its delete is left for the next hold, which
/// records the step done instead of stopped
#[tokio::test]
async fn a_job_that_finishes_before_its_delete_is_recorded_done() {
    // a Job that changes right before the delete, as when it completes
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let job = running_job(&scene).await;
    scene.sim.change_before_delete(&job);
    scene.retarget(None);
    assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    // the Job completed in between, so the next hold records it done and stops nothing
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 3, &[])));
    assert_eq!(secs(scene.reconcile().await), Some(HOLD_SECS));
    let entries = scene.history("users-v2");
    assert_eq!(
        entries.last().map(|entry| entry.outcome),
        Some(StepState::Done),
        "{entries:?}"
    );
    assert_eq!(blocks_of(&scene, &job), 0);
    assert_eq!(deletes_of(&scene, &job).len(), 1);
}

/// Build a state at a revision whose history holds a step's open run
///
/// # Arguments
///
/// * `at` - The revision the state is at
/// * `revision` - The revision the step belongs to
/// * `step` - The step
fn open_run_state(at: &str, revision: &str, step: &str) -> UpgradeState {
    // a run that started and paused the API
    let mut state = UpgradeState::new(rev(at), OPERATOR_VERSION);
    state.record(AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome: StepState::Running,
        job: Some("migrate-1".to_owned()),
        paused: BTreeMap::from([("api".to_owned(), 2)]),
        ..AppliedStep::default()
    });
    state
}

/// The hold note of an open forward step says to set the target to its revision or a later one,
/// and that of an open reverse step its revision or an earlier one
#[test]
fn the_hold_note_points_a_reverse_step_down() {
    // an open run of the forward step and of its reverse
    let forward = open_run_state("2026-10-v02", "2026-10-v03", "users-settings-theme");
    let reverse = open_run_state("2026-10-v03", "2026-10-v02", "users-settings-theme-reverse");
    let note = |state: &UpgradeState| {
        pause::hold_note(state, migration::open_runs(state).first().copied()).expect("a note")
    };
    // the forward step goes on upward and the reverse step downward
    let up = note(&forward);
    assert!(
        up.contains("target_revision to 2026-10-v03 or a later revision again"),
        "{up}"
    );
    let down = note(&reverse);
    assert!(
        down.contains("target_revision to 2026-10-v02 or an earlier one again"),
        "{down}"
    );
    assert!(!down.contains("later"), "{down}");
}

/// Clearing the target while `images-to-tools` runs stops its Job and keeps all five components it
/// paused at zero, naming each with its count; the stop ends the attempt's drain, so setting the
/// target again drains again before the conversion resumes under its first Job name with no new
/// pause and one start entry
#[tokio::test]
// the stop and the resume are one story, which reads best in one place
#[allow(clippy::too_many_lines)]
async fn clearing_the_target_mid_conversion_stops_images_to_tools_and_keeps_every_pause() {
    // a cluster with every component the conversion pauses, and the conversion approved
    let components = [
        ("api", 3),
        ("scaler", 1),
        ("baremetal-scaler", 1),
        ("event-handler", 1),
        ("search-streamer", 1),
    ];
    let approvals = json!([{"step": "images-to-tools", "backup": "snap-1"}]);
    let mut scene = Scene::new(agentic_catalog(), &approvals, &components);
    scene.sim.slow_delete();
    // nothing is in flight, so the drain is skipped and the conversion starts its Job
    let drain = check_report("drain-reactions", 0, &[], None);
    let images = check_report("images-to-tools", 3, &[], None);
    settle_agentic(&scene, &drain, &images, 4).await;
    let jobs = scene.sim.active("migration");
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = jobs[0].clone();
    assert!(job.contains("images-to-tools"), "{job}");
    for (name, _) in components {
        assert_eq!(scene.sim.replicas(name), Some(0), "{name}");
    }
    let scaled = scene.sim.scaled().len();
    let drain_checks = scene.sim.created_of("migration-check").len();
    // the target is cleared while the conversion runs
    scene.retarget(None);
    assert_eq!(secs(scene.reconcile().await), Some(STOPPING_SECS));
    // the Job is deleted once in the foreground, and every component stays at zero
    let deletes = deletes_of(&scene, &job);
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].body["propagationPolicy"], "Foreground");
    for (name, _) in components {
        assert_eq!(scene.sim.replicas(name), Some(0), "{name}");
    }
    // the status names the step, its Job, all five components with their counts, and the ways on
    let status = scene.sim.status_value();
    assert_eq!(status["phase"], "UpgradeRequired");
    let message = status["message"].as_str().unwrap_or_default();
    for part in [
        "images-to-tools",
        job.as_str(),
        "api (3)",
        "scaler (1)",
        "baremetal-scaler (1)",
        "event-handler (1)",
        "search-streamer (1)",
        "snap-1",
        "set spec.upgrade.target_revision",
        "by hand",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
    // the stop ends the attempt's drain and is audited as a block of the conversion
    let state = scene.sim.state().expect("state");
    assert!(
        state
            .applied
            .iter()
            .any(|entry| entry.step == thorium::models::upgrades::UPGRADE_STOPPED_STEP),
        "{:?}",
        state.applied
    );
    assert_eq!(
        scene
            .history("images-to-tools")
            .last()
            .map(|entry| entry.outcome),
        Some(StepState::Blocked)
    );
    // once the Job is gone and the target is set again, the drain is checked again first
    scene.sim.reap(&job);
    scene.retarget(Some("2026-10-v02"));
    scene.reconcile().await;
    let checks = scene.sim.created_of("migration-check");
    assert!(
        checks[drain_checks..]
            .iter()
            .any(|check| Scene::args(check).get(3).map(String::as_str) == Some("drain-reactions")),
        "{checks:#?}"
    );
    assert_eq!(migrations(&scene).len(), 1);
    // with nothing in flight the conversion resumes under its first Job name, without a new pause
    settle_agentic(&scene, &drain, &images, 4).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    assert_eq!(created[1]["metadata"]["name"], job.as_str());
    assert_eq!(scene.sim.scaled().len(), scaled);
    let running = scene
        .history("images-to-tools")
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Running)
        .collect::<Vec<_>>();
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].paused.len(), components.len());
}

/// A downgrade below the recorded revision of a cluster whose conversion stopped partway is
/// refused before anything runs, naming the conversion, and the hold stops the conversion's Job
#[tokio::test]
async fn a_downgrade_below_a_stopped_conversion_is_refused_and_stops_its_job() {
    // a cluster recorded at the revision before the conversions, whose conversion run is open
    let mut state =
        thorium::models::upgrades::Catalog::PRODUCTION.reached(&rev("2026-10-v03"), "1.8.1");
    for (step, outcome) in [
        ("drain-reactions", StepState::Skipped),
        ("images-to-tools", StepState::Running),
    ] {
        state.record(AppliedStep {
            revision: "2026-10-v04".to_owned(),
            step: step.to_owned(),
            outcome,
            job: (outcome == StepState::Running)
                .then(|| "migration-images-to-tools-000000000000".to_owned()),
            backup: Some("snap-1".to_owned()),
            paused: if outcome == StepState::Running {
                BTreeMap::from([("api".to_owned(), 3)])
            } else {
                BTreeMap::new()
            },
            ..AppliedStep::default()
        });
    }
    // the admin targets the first Helm revision
    let cluster = chart_cluster(json!({"target_revision": "2026-10-v02"}));
    let fake = super::super::tests::fake_with_status(&cluster).route(
        "GET",
        super::super::tests::STATE_PATH,
        200,
        super::super::tests::state_config_map(&state),
    );
    let tracker = tracked(&cluster);
    let gated = gate(&fake.client(), &tracker).await.expect("gate");
    // the downgrade is refused naming the conversion, and the hold stops the conversion's Job
    assert!(matches!(gated, Gate::Stop { .. }));
    let status = tracker.current();
    assert_eq!(
        status.phase,
        Some(crate::k8s::crds::ClusterPhase::Error),
        "{status:?}"
    );
    let message = status.message.unwrap_or_default();
    for part in [
        "images-to-tools",
        "2026-10-v04",
        "can't be undone",
        "api (3)",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
}
