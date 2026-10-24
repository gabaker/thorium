//! Tests of what a migration Job leaves in the deployment's history and what the status Job returns
//! for a check Job: the `Running` entry written first, the step's own outcome entry, and the check
//! report behind a Job's result row

mod common;

use common::migrate::World;
use serde_json::{Value, json};
use std::process::Output;
use thorium::models::schema::{CheckReport, RESULT_PREFIX, StatusReport};
use thorium::models::upgrades::{AppliedStep, StepState};

/// The revision of the fixture step these tests run
const REVISION: &str = "2026-10-v02";

/// The step the operator names in the `Running` entry it passes
const STEP: &str = "fixture-step";

/// Build the `Running` entry the operator passes with `--running-entry`
///
/// # Arguments
///
/// * `job` - The migration Job's name
fn running_entry(job: &str) -> String {
    // the entry the operator records before it creates the Job, with the counts it paused
    json!({
        "revision": REVISION,
        "step": STEP,
        "outcome": "Running",
        "message": "running",
        "at": "2026-10-22T10:00:00Z",
        "by": "thorium-operator 1.8.1",
        "backup": "snap-1",
        "job": job,
        "paused": {"api": 2, "scaler": 1},
    })
    .to_string()
}

/// The arguments of a run of a fixture migration as a Job runs it
///
/// # Arguments
///
/// * `id` - The migration
/// * `extra` - More arguments
fn job_args(id: &str, extra: &[&str]) -> Vec<String> {
    // one slice makes the counts deterministic
    [
        "migrate",
        "run",
        "--id",
        id,
        "--slices",
        "1",
        "--batch-rows",
        "50",
    ]
    .iter()
    .chain(extra)
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// Read the history entries of the step these tests run, oldest first by id
///
/// # Arguments
///
/// * `world` - The world to read
async fn history(world: &World) -> Vec<AppliedStep> {
    // the history rows hold their entry as JSON text
    let result = world
        .session()
        .query_unpaged(
            format!(
                "SELECT id, entry FROM {}.schema_migrations WHERE kind = 'history'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the history reads")
        .into_rows_result()
        .expect("rows");
    let mut rows = result
        .rows::<(String, Option<String>)>()
        .expect("typed rows")
        .map(|row| row.expect("a row"))
        .collect::<Vec<_>>();
    rows.sort();
    rows.into_iter()
        .filter_map(|(_, entry)| entry)
        .map(|entry| serde_json::from_str::<AppliedStep>(&entry).expect("an entry parses"))
        .filter(|entry| entry.step == STEP)
        .collect()
}

/// The outcomes of the history entries of one step, oldest first
///
/// # Arguments
///
/// * `entries` - The history
fn outcomes(entries: &[AppliedStep]) -> Vec<StepState> {
    // keep the order the ids sort in
    entries
        .iter()
        .filter(|entry| entry.step == STEP)
        .map(|entry| entry.outcome)
        .collect()
}

/// Run `thoradm` and describe a failure
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // the log lines say why it failed
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A run given the operator's `Running` entry writes that entry and nothing else: the operator
/// records the Job's outcome from the termination summary, so the history holds the step's
/// outcome once
#[tokio::test]
async fn an_operator_run_leaves_the_steps_outcome_to_the_operator() {
    // layout 1 rows to convert and two unreadable ones, whose group is within the step's limit
    let world = World::new("job01").await;
    world.seed_valid("a", 30).await;
    world
        .seed(&[
            ("bad-1".to_owned(), "not json".to_owned()),
            ("bad-2".to_owned(), "also not json".to_owned()),
        ])
        .await;
    let output = World::run(
        world
            .thoradm()
            .env("THORIUM_JOB_NAME", "migration-fixture-1")
            .args(job_args(
                "fixture-theme",
                &[
                    "--backup",
                    "snap-1",
                    "--running-entry",
                    &running_entry("migration-fixture-1"),
                ],
            )),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the history holds the `Running` entry only
    let entries = history(&world).await;
    assert_eq!(outcomes(&entries), vec![StepState::Running], "{entries:#?}");
    let running = &entries[0];
    assert_eq!(running.paused.get("api"), Some(&2));
    assert_eq!(running.paused.get("scaler"), Some(&1));
    world.drop_keyspace().await;
}

/// A run whose first pass finds every object current writes nothing, and an operator-run step
/// leaves its `Skipped` outcome to the operator like any other
#[tokio::test]
async fn a_run_with_nothing_to_change_leaves_the_outcome_to_the_operator() {
    // rows already in layout 2
    let world = World::new("job02").await;
    world
        .seed(&[(
            "a".to_owned(),
            "{\"name\":\"a\",\"theme\":\"Dark\"}".to_owned(),
        )])
        .await;
    let output = World::run(
        world
            .thoradm()
            .env("THORIUM_JOB_NAME", "migration-fixture-2")
            .args(job_args(
                "fixture-theme",
                &["--running-entry", &running_entry("migration-fixture-2")],
            )),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let entries = history(&world).await;
    assert_eq!(outcomes(&entries), vec![StepState::Running], "{entries:#?}");
    world.drop_keyspace().await;
}

/// A run by hand given a `Running` entry, as `migrate revert` gives its own, is no Job of the
/// operator's, so it writes the step's outcome itself
#[tokio::test]
async fn a_run_by_hand_with_a_running_entry_records_its_own_outcome() {
    // rows in layout 1 to convert, run outside any Job
    let world = World::new("job07").await;
    world.seed_valid("a", 5).await;
    let output = World::run(world.thoradm().args(job_args(
        "fixture-theme",
        &["--running-entry", &running_entry("by-hand")],
    )));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the history holds the `Running` entry and the run's own outcome, written by thoradm
    let entries = history(&world).await;
    assert_eq!(
        outcomes(&entries),
        vec![StepState::Running, StepState::Done],
        "{entries:#?}"
    );
    assert!(
        entries[1]
            .by
            .as_deref()
            .is_some_and(|by| by.starts_with("thoradm ")),
        "{:?}",
        entries[1].by
    );
    world.drop_keyspace().await;
}

/// A run that fails on a group over the step's limit leaves its `Running` entry and no outcome,
/// so the step stays unfinished
#[tokio::test]
async fn a_failed_run_leaves_no_outcome() {
    // the strict migration allows no issues at all
    let world = World::new("job03").await;
    world.seed_valid("a", 5).await;
    world
        .seed(&[("bad".to_owned(), "not json".to_owned())])
        .await;
    let output = World::run(world.thoradm().args(job_args(
        "fixture-strict",
        &["--running-entry", &running_entry("migration-fixture-3")],
    )));
    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
    let entries = history(&world).await;
    assert_eq!(outcomes(&entries), vec![StepState::Running], "{entries:#?}");
    world.drop_keyspace().await;
}

/// A run that only re-processes named objects doesn't end its pass, so it records no outcome, and
/// an entry that isn't a `Running` entry for a step is refused before anything is written
#[tokio::test]
async fn objects_runs_and_bad_entries_record_nothing() {
    let world = World::new("job04").await;
    world.seed_valid("a", 3).await;
    // a pass to fix objects of, run without the operator's entry
    let output = World::run(world.thoradm().args(job_args("fixture-theme", &[])));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(history(&world).await.len(), 0);
    // `--objects` leaves the pass open
    let output = World::run(world.thoradm().args(job_args(
        "fixture-theme",
        &[
            "--objects",
            "a-0",
            "--running-entry",
            &running_entry("migration-fixture-4"),
        ],
    )));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let entries = history(&world).await;
    assert_eq!(outcomes(&entries), vec![StepState::Running], "{entries:#?}");
    // an entry that isn't a `Running` one is a usage error that writes nothing
    let done = running_entry("x").replace("Running", "Done");
    let output = World::run(
        world
            .thoradm()
            .args(job_args("fixture-theme", &["--running-entry", &done])),
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    // so is one that isn't JSON
    let output = World::run(
        world
            .thoradm()
            .args(job_args("fixture-theme", &["--running-entry", "nonsense"])),
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(history(&world).await.len(), 1);
    world.drop_keyspace().await;
}

/// The status Job asked for a check Job's result returns its check report, and the report a
/// check that waits carries is whole
#[tokio::test]
async fn the_status_job_returns_a_check_jobs_report() {
    // a fixture migration whose check reports a wait, and a Job UID to key the result row by
    let world = World::new("job05").await;
    world.seed_valid("a", 4).await;
    let output = World::run(world.thoradm().env("THORIUM_JOB_UID", "uid-check-1").args([
        "migrate",
        "check",
        "--id",
        "fixture-wait",
        "--slices",
        "1",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the status Job reads the row by the UID, without probing the backends
    let output = World::run(world.thoradm().args([
        "schema",
        "status",
        "--no-probe",
        "--result",
        "uid-check-1",
        "--wait-for-scylla",
        "5",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(RESULT_PREFIX))
        .expect("a result line");
    let report: StatusReport = serde_json::from_str(line).expect("a status report");
    // the check report is there, and the record result isn't
    let check: &CheckReport = report.check.as_ref().expect("the check report");
    assert_eq!(check.migration, "fixture-wait");
    let wait = check.wait.as_ref().expect("the check asks to wait");
    assert!(wait.retry_after_secs > 0);
    assert!(report.job_result.is_none());
    // an unknown UID returns neither
    let output = World::run(world.thoradm().args([
        "schema",
        "status",
        "--no-probe",
        "--result",
        "uid-nothing",
        "--wait-for-scylla",
        "5",
    ]));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(RESULT_PREFIX))
        .expect("a result line");
    let value: Value = serde_json::from_str(line).expect("json");
    assert!(value.get("check").is_none(), "{value}");
    world.drop_keyspace().await;
}

/// A `--reason` run that ends a failed pass records the step's outcome once, and the pass row's
/// counts are the work the entry names
#[tokio::test]
async fn a_reason_run_that_ends_the_pass_records_the_step_once() {
    // a pass that failed on a group over the strict limit
    let world = World::new("job06").await;
    world.seed_valid("a", 4).await;
    world
        .seed(&[("p1".to_owned(), "{\"nme\":1}".to_owned())])
        .await;
    let entry = running_entry("migration-fixture-6");
    let output = World::run(
        world
            .thoradm()
            .args(job_args("fixture-strict", &["--running-entry", &entry])),
    );
    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
    assert_eq!(outcomes(&history(&world).await), vec![StepState::Running]);
    // an admin fixes the group's object and re-runs it
    world
        .session()
        .query_unpaged(
            format!(
                "UPDATE {}.mig_fixture SET body = '{{\"name\":\"ok\"}}' WHERE id = 'p1'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the row updates");
    let output = World::run(world.thoradm().args(job_args(
        "fixture-strict",
        &[
            "--reason",
            "missing_field:body.name",
            "--running-entry",
            &entry,
        ],
    )));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the pass ended `Done`, and the step's outcome is recorded once, carrying the pass's counts
    let entries = history(&world).await;
    assert_eq!(
        outcomes(&entries),
        vec![StepState::Running, StepState::Done],
        "{entries:#?}"
    );
    let row = world
        .store
        .pass_row("fixture")
        .await
        .expect("a row")
        .expect("a pass");
    let counts = row.counts.expect("the pass counts");
    assert_eq!(
        entries[1].work.as_deref(),
        Some(counts.work_text().as_str())
    );
    world.drop_keyspace().await;
}
