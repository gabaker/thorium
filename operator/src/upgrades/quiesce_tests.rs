//! Tests of a step that cancels in-flight work on approval: the confirmation code it shows while it
//! waits, the quiesce it records before stopping the scalers, the grace, the run that follows with
//! the whole pause, and the withdrawal that restores the scalers, all on the scene of the migration
//! tests with the production drain

use thorium::models::upgrades::{BlockReason, ConfirmRefusal, QUIESCE_STEP, drain_code};

use super::*;

/// The drain's step id
const DRAIN: &str = "drain-reactions";

/// The Deployments of a cluster running every component the conversions pause, with their replicas
const COMPONENTS: [(&str, i32); 5] = [
    ("api", 3),
    ("scaler", 2),
    ("baremetal-scaler", 1),
    ("event-handler", 1),
    ("search-streamer", 1),
];

/// The time the tests start at
fn start() -> DateTime<Utc> {
    // a fixed moment, so grace periods are counted from a known time
    DateTime::parse_from_rfc3339("2026-11-05T12:00:00Z")
        .expect("a time")
        .with_timezone(&Utc)
}

/// Build a scene upgrading through the production drain with every component running and a history
/// that already holds an entry, as a deployed cluster's does
///
/// # Arguments
///
/// * `approvals` - The approvals the cluster starts with
fn drain_scene(approvals: &Value) -> Scene {
    // the scene at the tests' start time
    set_now(start());
    let scene = Scene::new(agentic_catalog(), approvals, &COMPONENTS);
    // an earlier entry, so the code depends on when the history last changed
    let mut state = scene.sim.state().expect("a state");
    state.record(AppliedStep {
        revision: "2026-10-v01".to_owned(),
        step: "baseline-fixture".to_owned(),
        outcome: StepState::Done,
        at: Some("2026-11-01T00:00:00+00:00".to_owned()),
        ..AppliedStep::default()
    });
    scene.sim.set_state(&state);
    scene
}

/// The report of a drain check that waits on reactions still in flight
fn waiting() -> Value {
    // the drain's check asks to wait and names what holds it
    let mut report = check_report(DRAIN, 0, &[], Some(15));
    report["wait"]["reason"] = json!("2 reaction(s) are still Created or Started");
    report
}

/// The report of an image conversion check that finds nothing to convert
fn images() -> Value {
    // nothing to change
    check_report("images-to-tools", 0, &[], None)
}

/// Run reconciles with the drain's check waiting
///
/// # Arguments
///
/// * `scene` - The scene
/// * `rounds` - How many reconciles to run
async fn settle(scene: &Scene, rounds: usize) {
    // every check the reconciles start ends with the reports above
    settle_agentic(scene, &waiting(), &images(), rounds).await;
}

/// Build the approval that cancels the drain's work with a confirmation code
///
/// # Arguments
///
/// * `code` - The code the approval repeats
fn cancel(code: &str) -> Value {
    // a backup, the opt-in, and the code
    json!([{"step": DRAIN, "backup": "snap-1", "cancel_in_flight": true, "confirm": code}])
}

/// Get the confirmation code the drain's status shows
///
/// # Arguments
///
/// * `scene` - The scene
fn shown_code(scene: &Scene) -> String {
    // the code is in the drain's block reason, waiting or refused
    match scene.step(DRAIN).blocked {
        Some(
            BlockReason::CancelApproval { confirm }
            | BlockReason::ConfirmationRefused { confirm, .. },
        ) => confirm,
        other => panic!("the drain shows no code: {other:?}"),
    }
}

/// Get the code the drain should show for the history the scene holds now
///
/// # Arguments
///
/// * `scene` - The scene
fn current_code(scene: &Scene) -> String {
    // the code over the recorded and target revisions and the history
    let state = scene.sim.state().expect("a state");
    drain_code(DRAIN, "2026-10-v01", "2026-10-v02", &state.applied)
}

/// Wait at the drain, approve the cancel with the code it shows, and run until the quiesce is
/// recorded, returning the code and the quiesce entry
///
/// # Arguments
///
/// * `scene` - The scene
async fn quiesce(scene: &mut Scene) -> (String, AppliedStep) {
    // the drain waits and shows its code
    settle(scene, 3).await;
    let code = shown_code(scene);
    // the approval repeats it
    scene.approve(&cancel(&code));
    settle(scene, 2).await;
    let entries = scene.history(QUIESCE_STEP);
    assert_eq!(entries.len(), 1, "{entries:?}");
    (code, entries[0].clone())
}

/// Move past the default grace of a quiesce and run until the drain's Job is created
///
/// # Arguments
///
/// * `scene` - The scene
async fn pass_grace(scene: &Scene) {
    // a minute and a second after the quiesce
    set_now(start() + Duration::seconds(61));
    settle(scene, 3).await;
}

/// Get the drain's `Running` entry
///
/// # Arguments
///
/// * `scene` - The scene
fn running(scene: &Scene) -> AppliedStep {
    // the entry its start recorded
    scene
        .history(DRAIN)
        .into_iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("the drain started")
}

/// An approval that opts in to cancelling but repeats no code keeps the drain waiting, says the code
/// is missing and names the current one, lists the approval as not matching, and pauses nothing
#[tokio::test]
async fn an_approval_without_the_code_keeps_waiting() {
    // the opt-in with a backup and no code
    let scene = drain_scene(&json!([
        {"step": DRAIN, "backup": "snap-1", "cancel_in_flight": true}
    ]));
    settle(&scene, 4).await;
    let code = current_code(&scene);
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(
        step.blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: code.clone(),
            refusal: ConfirmRefusal::Missing,
        })
    );
    let message = step.message.expect("a message");
    assert!(
        message.contains("the approval repeats no confirmation code"),
        "{message}"
    );
    assert!(message.contains(&code), "{message}");
    // the status lists the approval as not matching, with the current code
    let stale = scene.sim.status().upgrade.expect("upgrade").stale_approvals;
    assert!(
        stale
            .iter()
            .any(|line| line.starts_with(DRAIN) && line.contains(&code)),
        "{stale:?}"
    );
    // nothing is quiesced, scaled, or run
    assert_eq!(scene.history(QUIESCE_STEP), Vec::<AppliedStep>::new());
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
    assert_eq!(migrations(&scene).len(), 0);
}

/// A code worked out before the drain was reached, over an older history, doesn't match the code
/// the drain shows, so the drain keeps waiting with nothing paused
#[tokio::test]
async fn a_code_staged_before_the_drain_does_not_match() {
    // the code of the history before its newest entry, as values staged earlier would carry
    let mut scene = drain_scene(&json!([]));
    let staged = drain_code(DRAIN, "2026-10-v01", "2026-10-v02", &[]);
    scene.approve(&cancel(&staged));
    settle(&scene, 4).await;
    // the drain waits and refuses the code
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_ne!(staged, current_code(&scene));
    assert_eq!(
        step.blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: current_code(&scene),
            refusal: ConfirmRefusal::Mismatch,
        })
    );
    assert_eq!(scene.history(QUIESCE_STEP), Vec::<AppliedStep>::new());
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
}

/// A code an earlier quiesce recorded is consumed: an approval repeating it is refused as consumed
/// and the drain keeps waiting with nothing paused
#[tokio::test]
async fn a_consumed_code_never_matches_again() {
    // an earlier quiesce that recorded a code and whose scalers were restored
    let mut scene = drain_scene(&json!([]));
    let mut state = scene.sim.state().expect("a state");
    let old = "OLDCODE1";
    state.record(AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: QUIESCE_STEP.to_owned(),
        outcome: StepState::Running,
        backup: Some("snap-0".to_owned()),
        confirm: Some(old.to_owned()),
        at: Some("2026-11-02T00:00:00+00:00".to_owned()),
        chain: vec![DRAIN.to_owned()],
        paused: BTreeMap::from([("scaler".to_owned(), 2), ("baremetal-scaler".to_owned(), 1)]),
        ..AppliedStep::default()
    });
    for (name, count) in [("scaler", 2), ("baremetal-scaler", 1)] {
        state.record(AppliedStep {
            revision: "2026-10-v01".to_owned(),
            step: RESTORE_STEP.to_owned(),
            outcome: StepState::Done,
            at: Some("2026-11-02T00:01:00+00:00".to_owned()),
            paused: BTreeMap::from([(name.to_owned(), count)]),
            ..AppliedStep::default()
        });
    }
    scene.sim.set_state(&state);
    // an approval repeating the consumed code
    scene.approve(&cancel(old));
    settle(&scene, 4).await;
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(
        step.blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: current_code(&scene),
            refusal: ConfirmRefusal::Consumed,
        })
    );
    assert_eq!(scene.history(QUIESCE_STEP).len(), 1);
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
}

/// While the drain waits it shows its current code in a `CancelApproval` block reason, and its
/// message keeps the check's reason and says how to cancel instead, in the Helm and the
/// `ThoriumCluster` spelling, repeating the code as `confirm`
#[tokio::test]
async fn the_waiting_status_shows_the_code() {
    // no approval at all
    let scene = drain_scene(&json!([]));
    settle(&scene, 4).await;
    let code = current_code(&scene);
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(
        step.blocked,
        Some(BlockReason::CancelApproval {
            confirm: code.clone()
        })
    );
    let message = step.message.expect("a message");
    assert!(
        message.starts_with("2 reaction(s) are still Created or Started"),
        "{message}"
    );
    assert!(message.contains("cancelInFlight: true"), "{message}");
    assert!(
        message.contains("cancel_in_flight: true in the ThoriumCluster"),
        "{message}"
    );
    assert!(message.contains(&code), "{message}");
    // the code stays the same while nothing is added to the history
    settle(&scene, 2).await;
    assert_eq!(shown_code(&scene), code);
}

/// The quiesce entry records the code the approval repeated, the approval's backup, the step's
/// revision, the scalers' counts, and the step it quiesced for
#[tokio::test]
async fn the_quiesce_entry_records_the_code() {
    // an approved cancel
    let mut scene = drain_scene(&json!([]));
    let (code, entry) = quiesce(&mut scene).await;
    assert_eq!(entry.outcome, StepState::Running);
    assert_eq!(entry.confirm.as_deref(), Some(code.as_str()));
    assert_eq!(entry.backup.as_deref(), Some("snap-1"));
    assert_eq!(entry.revision, "2026-10-v02");
    assert_eq!(entry.chain, [DRAIN]);
    assert_eq!(
        entry.paused,
        BTreeMap::from([("baremetal-scaler".to_owned(), 1), ("scaler".to_owned(), 2)])
    );
    assert_eq!(entry.at, Some(start().to_rfc3339()));
}

/// An approval with a backup but without the opt-in keeps the drain waiting on its check, with
/// nothing quiesced, scaled, or run
#[tokio::test]
async fn an_approval_without_opt_in_keeps_waiting() {
    // the approval every other gated step takes
    let scene = drain_scene(&json!([{"step": DRAIN, "backup": "snap-1"}]));
    settle(&scene, 4).await;
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert!(
        step.message
            .as_deref()
            .is_some_and(|message| message.starts_with("2 reaction(s) are still")),
        "{step:?}"
    );
    assert_eq!(scene.history(QUIESCE_STEP), Vec::<AppliedStep>::new());
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
    assert_eq!(migrations(&scene).len(), 0);
}

/// An opted-in approval with the code records the quiesce, with the scalers' counts, in the state
/// before either scaler is scaled down, and only the scalers stop
#[tokio::test]
async fn an_opted_in_approval_records_the_quiesce_before_scaling() {
    // an approved cancel
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    // the state holding the quiesce was written before the first scale of a scaler
    let writes = scene.sim.fake().writes();
    let saved = writes
        .iter()
        .position(|request| {
            request.method == "PUT" && request.body.to_string().contains(QUIESCE_STEP)
        })
        .expect("the quiesce was saved");
    let scaled = writes
        .iter()
        .position(|request| {
            request.method == "PATCH"
                && (request.path.ends_with("/deployments/scaler")
                    || request.path.ends_with("/deployments/baremetal-scaler"))
        })
        .expect("a scaler was scaled");
    assert!(saved < scaled, "saved at {saved}, scaled at {scaled}");
    // the scalers are at zero and everything else keeps running
    assert_eq!(scene.sim.replicas("scaler"), Some(0));
    assert_eq!(scene.sim.replicas("baremetal-scaler"), Some(0));
    for name in ["api", "event-handler", "search-streamer"] {
        assert_ne!(scene.sim.replicas(name), Some(0), "{name}");
    }
    assert_eq!(migrations(&scene).len(), 0);
}

/// During the grace the scalers are held paused, so a Deployment apply keeps them at zero
#[tokio::test]
async fn a_deployment_apply_during_the_grace_keeps_the_scalers_at_zero() {
    // a quiesce within its grace
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    // the pauses an apply reads hold both scalers at their counts, and nothing else
    let client = scene.sim.fake().client();
    let mut cluster = scene.cluster.clone();
    cluster.status = Some(scene.sim.status());
    let meta = meta_for(cluster, &client);
    assert_eq!(
        crate::upgrades::paused_components(&meta).await,
        BTreeMap::from([("baremetal-scaler".to_owned(), 1), ("scaler".to_owned(), 2)])
    );
}

/// The drain waits out the grace with no Job, then starts its run with the conversion pause,
/// passing the `at` of its `Running` entry as the step attempt
#[tokio::test]
async fn the_cancel_runs_after_the_grace_with_the_conversion_pause() {
    // a quiesce within its grace
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    set_now(start() + Duration::seconds(30));
    settle(&scene, 2).await;
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert!(
        step.message
            .as_deref()
            .is_some_and(|message| message.contains("grace")),
        "{step:?}"
    );
    assert_eq!(migrations(&scene).len(), 0);
    // once the grace ends the run starts with every conversion component paused
    pass_grace(&scene).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 1, "{created:#?}");
    assert_eq!(
        &Scene::args(&created[0])[..4],
        ["migrate", "run", "--id", DRAIN]
    );
    assert!(Scene::flag(&created[0], "--confirm").is_none());
    let entry = running(&scene);
    assert_eq!(Scene::flag(&created[0], "--step-attempt"), entry.at);
    for (name, _) in COMPONENTS {
        assert_eq!(scene.sim.replicas(name), Some(0), "{name}");
    }
    assert_eq!(
        entry.paused,
        COMPONENTS
            .iter()
            .map(|(name, count)| ((*name).to_owned(), *count))
            .collect::<BTreeMap<_, _>>()
    );
}

/// A start held while the paused pods stop gives the Job it creates later the `Running` entry it
/// recorded first, so the entry the run writes and its step attempt name the same start
#[tokio::test]
async fn a_held_start_gives_its_job_the_recorded_entry() {
    // a quiesce past its grace whose API pods don't stop at once
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    scene.sim.stick("api");
    pass_grace(&scene).await;
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(migrations(&scene).len(), 0);
    let recorded = running(&scene);
    // the pods stop later and the run starts
    set_now(start() + Duration::seconds(90));
    scene.sim.unstick("api");
    settle(&scene, 3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 1, "{created:#?}");
    // one start, which the Job's entry and its step attempt both name
    let starts = scene
        .history(DRAIN)
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Running)
        .collect::<Vec<_>>();
    assert_eq!(starts, vec![recorded.clone()]);
    let given: AppliedStep = serde_json::from_str(
        &Scene::flag(&created[0], "--running-entry").expect("a running entry"),
    )
    .expect("an entry");
    assert_eq!(given.at, recorded.at);
    assert_eq!(Scene::flag(&created[0], "--step-attempt"), recorded.at);
}

/// A cancel run whose Job failed is retried under a new backup with the step attempt of the run
/// that failed, though the retry records its own `Running` entry, so the requests both runs record
/// share one attempt
#[tokio::test]
async fn a_retried_cancel_keeps_the_failed_runs_step_attempt() {
    // the approved cancel's first Job, which fails
    let mut scene = drain_scene(&json!([]));
    let (code, _) = quiesce(&mut scene).await;
    pass_grace(&scene).await;
    let first = running(&scene);
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "BackoffLimitExceeded".to_owned(),
            summary: None,
            log_lines: 3,
        },
    );
    settle(&scene, 2).await;
    assert_eq!(scene.step(DRAIN).state, StepState::Failed);
    // the approval names a newer backup with the same code, and the retry starts
    scene.approve(&json!([{
        "step": DRAIN, "backup": "snap-2", "cancel_in_flight": true, "confirm": code
    }]));
    set_now(start() + Duration::seconds(120));
    settle(&scene, 3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    let starts = scene
        .history(DRAIN)
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Running)
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_ne!(starts[1].at, first.at);
    // both Jobs record under the first run's start
    for job in &created {
        assert_eq!(Scene::flag(job, "--step-attempt"), first.at);
    }
    assert_eq!(
        migration::attempt_at(&scene.sim.state().expect("a state"), DRAIN),
        first.at
    );
}

/// An approved drain whose run found nothing in flight is recorded done, never skipped
#[tokio::test]
async fn the_approved_drain_never_skips() {
    // an approved cancel past its grace
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    pass_grace(&scene).await;
    // the run changes nothing
    let job = scene.sim.jobs("migration").remove(0);
    let summary = json!({
        "outcome": "done",
        "migration": DRAIN,
        "counts": PassCounts::default(),
        "groups": [],
    });
    scene.sim.finish(&job, &Finish::Complete(summary));
    settle(&scene, 2).await;
    let entries = scene.history(DRAIN);
    let last = entries.last().expect("an entry");
    assert_eq!(last.outcome, StepState::Done, "{entries:?}");
    assert_eq!(last.job.as_deref(), Some(job.as_str()));
}

/// Removing the approval during the grace scales each scaler back to its recorded count, records
/// the restore, and leaves the drain waiting with a new code
#[tokio::test]
async fn withdrawing_the_approval_during_the_grace_restores_the_scalers() {
    // a quiesce within its grace
    let mut scene = drain_scene(&json!([]));
    let (code, _) = quiesce(&mut scene).await;
    // the approval is removed
    scene.approve(&json!([]));
    settle(&scene, 2).await;
    assert_eq!(scene.sim.replicas("scaler"), Some(2));
    assert_eq!(scene.sim.replicas("baremetal-scaler"), Some(1));
    let state = scene.sim.state().expect("a state");
    assert_eq!(pause::paused(&state), BTreeMap::new());
    let restored = scene
        .history(RESTORE_STEP)
        .into_iter()
        .flat_map(|entry| entry.paused.into_iter())
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        restored,
        BTreeMap::from([("baremetal-scaler".to_owned(), 1), ("scaler".to_owned(), 2)])
    );
    // the drain waits again under a new code, with no run
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_ne!(shown_code(&scene), code);
    assert_eq!(shown_code(&scene), current_code(&scene));
    assert_eq!(migrations(&scene).len(), 0);
}

/// The run's start keeps the counts the quiesce recorded for the scalers, though they are at zero
/// by then, and adds no note about lost counts
#[tokio::test]
async fn start_takes_over_the_quiesced_counts_without_a_lost_note() {
    // an approved cancel past its grace
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    pass_grace(&scene).await;
    let entry = running(&scene);
    assert_eq!(entry.paused.get("scaler"), Some(&2));
    assert_eq!(entry.paused.get("baremetal-scaler"), Some(&1));
    let message = entry.message.expect("a message");
    assert!(!message.contains("lost"), "{message}");
    let shown = scene.step(DRAIN).message.unwrap_or_default();
    assert!(!shown.contains("lost"), "{shown}");
}

/// Once the drain is done, the rollout's applies bring every component back to the count it had
/// before the quiesce
#[tokio::test]
async fn the_rollout_restores_the_scalers_exact_counts() {
    // a finished approved drain that recorded no request, so no resubmission holds the pause
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    pass_grace(&scene).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 0, &[])));
    settle(&scene, 3).await;
    // every component is still at zero until its Deployment is applied
    for (name, _) in COMPONENTS {
        assert_eq!(scene.sim.replicas(name), Some(0), "{name}");
    }
    // each apply restores the exact count
    let client = scene.sim.fake().client();
    let mut cluster = scene.cluster.clone();
    cluster.status = Some(scene.sim.status());
    let meta = meta_for(cluster, &client);
    for (name, count) in COMPONENTS {
        apply_deployment(&meta, name).await;
        assert_eq!(scene.sim.replicas(name), Some(count), "{name}");
    }
}

/// A code a quiesce consumed stays spent after the attempt stops: clearing the target restores the
/// scalers, and setting it again leaves the same approval refused as consumed, with no new quiesce
#[tokio::test]
async fn an_approval_used_before_upgrade_stopped_is_spent() {
    // a quiesce within its grace
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    // the target is cleared, which holds the cluster and ends the quiesce
    scene.retarget(None);
    scene.reconcile().await;
    assert_eq!(scene.sim.replicas("scaler"), Some(2));
    assert_eq!(scene.sim.replicas("baremetal-scaler"), Some(1));
    // the target is set again with the same approval
    scene.retarget(Some("2026-10-v02"));
    settle(&scene, 3).await;
    let step = scene.step(DRAIN);
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(
        step.blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: current_code(&scene),
            refusal: ConfirmRefusal::Consumed,
        })
    );
    assert_eq!(scene.history(QUIESCE_STEP).len(), 1);
    assert_eq!(scene.sim.replicas("scaler"), Some(2));
    assert_eq!(migrations(&scene).len(), 0);
}

/// An operator restart during the grace reads the grace's start and the scalers' counts from the
/// history: reconciles from a fresh start keep the one quiesce entry, start no run before the grace
/// ends, and the run records the counts from before the quiesce
#[tokio::test]
async fn an_operator_restart_during_the_grace_keeps_its_start_and_counts() {
    // a quiesce within its grace
    let mut scene = drain_scene(&json!([]));
    let (_, entry) = quiesce(&mut scene).await;
    // reconciles of a restarted operator, which holds nothing in memory, within the grace
    for seconds in [10, 30, 59] {
        set_now(start() + Duration::seconds(seconds));
        settle(&scene, 1).await;
        assert_eq!(scene.history(QUIESCE_STEP), std::slice::from_ref(&entry));
        assert_eq!(migrations(&scene).len(), 0, "at {seconds} seconds");
        assert_eq!(scene.sim.replicas("scaler"), Some(0));
    }
    // the grace ends a minute after the recorded start, and the run keeps the recorded counts
    pass_grace(&scene).await;
    assert_eq!(migrations(&scene).len(), 1);
    let running = running(&scene);
    assert_eq!(running.paused.get("scaler"), Some(&2));
    assert_eq!(running.paused.get("baremetal-scaler"), Some(&1));
}

/// Tests of deleting the worker pods the drain's run named, which run on this file's scene
#[path = "cancel_tests.rs"]
mod cancel_tests;
