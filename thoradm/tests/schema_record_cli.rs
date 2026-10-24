//! Tests of `thoradm schema status`, `schema apply --seed-revision`, and `migrate record` as the
//! operator's Jobs run them: the one result line, the Job's result row, the termination summary,
//! and the seed, copy, and revision rules

use chrono::{Duration, SecondsFormat, Utc};
use scylla::client::session::Session;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thorium::Conf;
use thorium::models::schema::{RecordStatus, StatusReport};
use thorium::models::upgrades::{AppliedStep, STATE_KEY, StepState, UpgradeState, latest};

#[path = "common/base_namespace.rs"]
mod base_namespace;

/// The test config every run starts from; the environment overrides point it at the test cluster
const CONFIG: &str = "../api/tests/thorium-testing.yml";

/// The prefix of the one line the status Job prints
const RESULT_PREFIX: &str = "THORIUM-RESULT ";

/// Build a thoradm command against the test config with a namespace of its own
///
/// # Arguments
///
/// * `namespace` - The namespace, and so the keyspace, the command uses
fn thoradm(namespace: &str) -> Command {
    // run the binary cargo built for these tests against the test config
    let mut command = Command::new(env!("CARGO_BIN_EXE_thoradm"));
    command
        .args(["--cluster-conf", CONFIG])
        .env("THORIUM__THORIUM__NAMESPACE", namespace);
    command
}

/// The namespace a test uses, beside the run's own
///
/// # Arguments
///
/// * `suffix` - What makes the test's namespace unique
fn namespace(suffix: &str) -> String {
    // the run's namespace comes from the environment, or is made up for a local run
    format!("{}_{suffix}", base_namespace::base_namespace())
}

/// A fresh path for a test's files
///
/// # Arguments
///
/// * `name` - What makes the path unique
fn scratch(name: &str) -> PathBuf {
    // a path under the temporary directory, removed if an earlier run left it
    let path = std::env::temp_dir().join(format!("thoradm-rec-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// Run a command, returning its output
///
/// # Arguments
///
/// * `command` - The command to run
fn run(command: &mut Command) -> Output {
    // run to completion, keeping stdout and stderr
    command.output().expect("thoradm runs")
}

/// Open a session with the test config
async fn session() -> Session {
    // connect with the same config the command uses
    let conf = Conf::new(CONFIG).expect("the test config loads");
    thorium::models::schema::connect(&conf)
        .await
        .expect("Scylla is reachable")
}

/// Run a CQL statement, failing the test on error
///
/// # Arguments
///
/// * `cql` - The statement
async fn exec(cql: &str) {
    // run it on a session of its own
    session()
        .await
        .query_unpaged(cql, ())
        .await
        .unwrap_or_else(|err| panic!("{cql}: {err}"));
}

/// Drop a test's keyspace
///
/// # Arguments
///
/// * `keyspace` - The keyspace to drop
async fn drop_keyspace(keyspace: &str) {
    // drop it whether or not it exists
    exec(&format!("DROP KEYSPACE IF EXISTS {keyspace}")).await;
}

/// Create a keyspace the way an older release did, from its setup statements
///
/// # Arguments
///
/// * `keyspace` - The keyspace to create
async fn create_legacy(keyspace: &str) {
    // the keyspace, then every statement of the older setup
    drop_keyspace(keyspace).await;
    exec(&format!(
        "CREATE KEYSPACE {keyspace} WITH replication = \
         {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
    ))
    .await;
    let legacy = include_str!("../../api/src/models/schema/goldens/legacy_ddl.cql")
        .replace("{ns}", keyspace)
        .replace("{ttl}", "2592000");
    for statement in legacy
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
    {
        exec(statement).await;
    }
}

/// Print a command's stderr, for failure messages
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // lossy text of the error stream
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Read the report a status run printed on its one result line
///
/// # Arguments
///
/// * `output` - The status command's output
fn report(output: &Output) -> StatusReport {
    // exactly one line carries the prefix, and everything after it is the report
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout
        .lines()
        .filter(|line| line.starts_with(RESULT_PREFIX))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{stdout}");
    serde_json::from_str(&lines[0][RESULT_PREFIX.len()..]).expect("the line is a report")
}

/// Run `schema status` and read its report
///
/// # Arguments
///
/// * `namespace` - The namespace to read
/// * `extra` - More arguments
fn status(namespace: &str, extra: &[&str]) -> (Output, StatusReport) {
    // the status Job's command, with a log file for its termination summary
    let output = run(thoradm(namespace)
        .args(["schema", "status", "--no-probe"])
        .args(extra));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = report(&output);
    (output, report)
}

/// Write the state `ConfigMap`'s file the record Job mounts
///
/// # Arguments
///
/// * `name` - What makes the directory unique
/// * `state` - The state to write
fn write_state(name: &str, state: &UpgradeState) -> PathBuf {
    // the mount is a directory holding the state under its key
    let dir = scratch(name);
    std::fs::create_dir_all(&dir).expect("a directory");
    std::fs::write(
        dir.join(STATE_KEY),
        serde_json::to_string(state).expect("the state serializes"),
    )
    .expect("the state is written");
    dir
}

/// Build a history entry
///
/// # Arguments
///
/// * `revision` - The revision the step belongs to
/// * `step` - The step's id
/// * `outcome` - How the step ended
/// * `seconds` - How many seconds after the test's start it ended
fn entry(revision: &str, step: &str, outcome: StepState, seconds: i64) -> AppliedStep {
    // the fields the record keeps
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .expect("a time")
        .with_timezone(&Utc)
        + Duration::seconds(seconds);
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome,
        message: Some(format!("{step} {outcome:?}")),
        at: Some(at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        by: Some("thorium-operator 1.8.1".to_owned()),
        ..AppliedStep::default()
    }
}

/// Build a state at a revision with entries
///
/// # Arguments
///
/// * `revision` - The revision the state records
/// * `entries` - The history
fn state_of(revision: &str, entries: Vec<AppliedStep>) -> UpgradeState {
    // a state is its revision and history
    let mut state = UpgradeState::new(revision.parse().expect("a revision"), "1.8.1");
    for applied in entries {
        state.record(applied);
    }
    state
}

/// A status run against a keyspace that doesn't exist prints one result line naming no keyspace
/// and thoradm's catalog, writes a short termination summary, and exits 0
#[tokio::test]
async fn status_of_a_missing_keyspace_prints_one_line() {
    // read the status of a namespace with no keyspace
    let ns = namespace("rcli_none");
    drop_keyspace(&ns).await;
    let log = scratch("none.log");
    let (_, report) = status(&ns, &[]);
    assert_eq!(report.status, RecordStatus::NoKeyspace);
    assert_eq!(report.catalog.latest, latest());
    assert_eq!(report.job_result, None);
    // the status Job writes no row, so a second run prints the same thing
    let (_, again) = status(&ns, &[]);
    assert_eq!(again, report);
    // the summary the Job leaves is short and names its outcome
    let output = run(thoradm(&ns)
        .args(["schema", "status", "--no-probe"])
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = std::fs::read_to_string(&log).expect("a summary");
    assert!(text.len() <= 4096, "{} bytes", text.len());
    let summary: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(summary["outcome"], "done");
    let _ = std::fs::remove_file(&log);
}

/// A status run that can't reach Scylla exits 0 with an `Unreachable` result naming no
/// credentials, so the operator retries and never takes the namespace for a fresh install
#[tokio::test]
async fn status_of_an_unreachable_scylla_reports_unreachable() {
    // point the command at a port nothing listens on
    let ns = namespace("rcli_down");
    let output = run(thoradm(&ns)
        .args(["schema", "status", "--no-probe"])
        .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1"));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = report(&output);
    assert!(
        matches!(&report.status, RecordStatus::Unreachable { message } if !message.is_empty()),
        "{:?}",
        report.status
    );
    // the catalog identity is thoradm's own, whatever the backends say
    assert_eq!(report.catalog.latest, latest());
}

/// A fresh install's apply records the latest revision, writes its result row under the Job's UID
/// and names the row in its summary; the status reads the record, and with the UID also the row
#[tokio::test]
async fn an_apply_leaves_a_record_and_a_result_row() {
    // apply the way the Job does, with the Job's UID in the environment
    let ns = namespace("rcli_apply");
    drop_keyspace(&ns).await;
    let log = scratch("apply.log");
    let uid = "5b0f4d4e-6a3f-4a0e-8f3e-2d5f0c1a7b11";
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--wait-for-scylla", "60"])
        .env("THORIUM_JOB_UID", uid)
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the summary names the row
    let text = std::fs::read_to_string(&log).expect("a summary");
    let summary: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(summary["result"], uid);
    // the status reads the record, and the row when asked
    let (_, plain) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &plain.status else {
        panic!("expected a record, read {:?}", plain.status);
    };
    assert_eq!(record.revision, latest());
    assert_eq!(plain.job_result, None);
    let (printed, with_row) = status(&ns, &["--result", uid]);
    // neither the record nor the Job's result row carries the files password's hash
    let line = String::from_utf8_lossy(&printed.stdout);
    assert!(!line.contains("$argon2"), "{line}");
    assert_eq!(with_row.status, plain.status);
    match with_row.job_result {
        Some(RecordStatus::Recorded {
            record: written, ..
        }) => {
            assert_eq!(written.revision, latest());
        }
        other => panic!("expected the Job's result, read {other:?}"),
    }
    // a UID with no row reads as none
    let (_, missing) = status(&ns, &["--result", "no-such-uid"]);
    assert_eq!(missing.job_result, None);
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_file(&log);
}

/// `schema apply --seed-revision` on a keyspace with data and no record seeds the revision and
/// names the source the operator passed; a keyspace whose object differs is refused with exit 1
/// and a summary that names the failure, and nothing is written
#[tokio::test]
async fn apply_seeds_a_revision_or_refuses_a_mismatch() {
    // a keyspace an older release created
    let ns = namespace("rcli_seed");
    create_legacy(&ns).await;
    let (_, before) = status(&ns, &[]);
    assert_eq!(before.status, RecordStatus::NoRecord { suggested: None });
    // seed it at the first revision, the way the operator's Job does for the starting revision
    let output = run(thoradm(&ns)
        .args([
            "schema",
            "apply",
            "--wait-for-scylla",
            "60",
            "--seed-revision",
            "2026-10-v01",
        ])
        .env("THORIUM_SEED_SOURCE", "spec.upgrade.starting_revision"));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (_, after) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &after.status else {
        panic!("expected a record, read {:?}", after.status);
    };
    assert_eq!(record.revision.as_str(), "2026-10-v01");
    assert!(
        record.history[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("spec.upgrade.starting_revision")),
        "{:?}",
        record.history
    );
    drop_keyspace(&ns).await;
    // a keyspace whose nodes' health column has another type is refused
    let ns = namespace("rcli_seedbad");
    drop_keyspace(&ns).await;
    exec(&format!(
        "CREATE KEYSPACE {ns} WITH replication = \
         {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
    ))
    .await;
    let legacy = include_str!("../../api/src/models/schema/goldens/legacy_ddl.cql")
        .replace("{ns}", &ns)
        .replace("{ttl}", "2592000")
        .replace("health TEXT", "health INT");
    for statement in legacy
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
    {
        exec(statement).await;
    }
    let log = scratch("seedbad.log");
    let output = run(thoradm(&ns)
        .args([
            "schema",
            "apply",
            "--wait-for-scylla",
            "60",
            "--seed-revision",
            "2026-10-v01",
        ])
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let summary: Value =
        serde_json::from_str(&std::fs::read_to_string(&log).expect("a summary")).expect("JSON");
    assert_eq!(summary["outcome"], "failed_systematic");
    assert!(
        summary["error"]
            .as_str()
            .is_some_and(|error| error.contains("nodes.health")),
        "{summary}"
    );
    let (_, after) = status(&ns, &[]);
    assert_eq!(after.status, RecordStatus::NoRecord { suggested: None });
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_file(&log);
}

/// `migrate record --seed` records the state's revision and its whole history, a second seed is
/// refused with exit 1, the apply that follows changes neither, and a record bigger than the
/// termination message reaches the status line intact while the summary stays short
#[tokio::test]
async fn record_seed_copies_the_state_once() {
    // a keyspace with no record table and a state with 220 entries
    let ns = namespace("rcli_state");
    create_legacy(&ns).await;
    let entries = (0..220)
        .map(|index| entry("2026-10-v02", "scylla-role", StepState::Blocked, index))
        .collect();
    let mut state = state_of("2026-10-v02", Vec::new());
    // the file holds all 220, as a state written by another build might, past the usual window
    state.applied = entries;
    let dir = write_state("state", &state);
    let log = scratch("seed.log");
    let uid = "7c1d2e3f-0000-4000-8000-000000000001";
    let output = run(thoradm(&ns)
        .args([
            "migrate",
            "record",
            "--seed",
            "--revision",
            "2026-10-v02",
            "--state",
        ])
        .arg(&dir)
        .env("THORIUM_JOB_UID", uid)
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = std::fs::read_to_string(&log).expect("a summary");
    assert!(text.len() <= 4096, "{} bytes", text.len());
    // the status line carries the newest 200 entries and the whole count, past 4 KiB
    let (output, report) = status(&ns, &["--result", uid]);
    assert!(output.stdout.len() > 4096, "{} bytes", output.stdout.len());
    let RecordStatus::Recorded { record, .. } = &report.status else {
        panic!("expected a record, read {:?}", report.status);
    };
    assert_eq!(record.revision.as_str(), "2026-10-v02");
    assert_eq!(record.history.len(), 200);
    assert_eq!(record.history_len, 220);
    assert!(matches!(
        &report.job_result,
        Some(RecordStatus::Recorded { .. })
    ));
    // a second seed is refused, and the apply that follows changes neither
    let output = run(thoradm(&ns)
        .args([
            "migrate",
            "record",
            "--seed",
            "--revision",
            "2026-10-v02",
            "--state",
        ])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let output = run(thoradm(&ns).args(["schema", "apply", "--wait-for-scylla", "60"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (_, after) = status(&ns, &[]);
    let RecordStatus::Recorded { record: after, .. } = &after.status else {
        panic!("expected a record, read {:?}", after.status);
    };
    assert_eq!(after.revision, record.revision);
    assert_eq!(after.history_len, record.history_len);
    assert_eq!(after.history, record.history);
    // a seed needs the state
    let output =
        run(thoradm(&ns).args(["migrate", "record", "--seed", "--revision", "2026-10-v02"]));
    assert_ne!(output.status.code(), Some(0));
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&log);
}

/// Seed a keyspace's record from a state at a revision
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `name` - What makes the state's directory unique
/// * `state` - The state to seed from
fn seed(ns: &str, name: &str, state: &UpgradeState) -> PathBuf {
    // the keyspace is seeded the way the operator's first Job does
    let dir = write_state(name, state);
    let output = run(thoradm(ns)
        .args([
            "migrate",
            "record",
            "--seed",
            "--revision",
            state.revision.as_str(),
            "--state",
        ])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    dir
}

/// Rewrite the state's file in a directory
///
/// # Arguments
///
/// * `dir` - The directory the state is mounted at
/// * `state` - The new state
fn rewrite_state(dir: &Path, state: &UpgradeState) {
    // replace the file the way the ConfigMap's mount changes
    std::fs::write(
        dir.join(STATE_KEY),
        serde_json::to_string(state).expect("the state serializes"),
    )
    .expect("the state is written");
}

/// `migrate record --revision` copies the operator's entries and moves the revision only once
/// every step of the path is recorded: a `Done` entry for a gated step placed in the state by hand
/// is never copied, so the revision stays; the operator's `Skipped` entry for it is, so the
/// revision moves, and the Job writes the finalize entry itself; a second run copies nothing
#[tokio::test]
async fn record_moves_the_revision_once_the_steps_are_recorded() {
    // a record at the baseline, and the state's two infra steps done and the reindex `Done` by hand
    let ns = namespace("rcli_move");
    create_legacy(&ns).await;
    let base = "2026-10-v01";
    let release = "2026-10-v02";
    let reindex = "elastic-reindex-keyword-mappings";
    let seeded = state_of(
        base,
        vec![entry(base, "convert-to-helm", StepState::Done, 0)],
    );
    let dir = seed(&ns, "move", &seeded);
    let infra = [
        entry(release, "scylla-role", StepState::Done, 1),
        entry(release, "elastic-identity", StepState::Done, 2),
    ];
    let mut by_hand = seeded.clone();
    for applied in infra
        .iter()
        .cloned()
        .chain([entry(release, reindex, StepState::Done, 3)])
    {
        by_hand.record(applied);
    }
    rewrite_state(&dir, &by_hand);
    // the revision doesn't move, since the hand-placed `Done` entry isn't copied
    let output = run(thoradm(&ns)
        .args(["migrate", "record", "--revision", release, "--state"])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains(reindex), "{}", stderr(&output));
    let (_, read) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &read.status else {
        panic!("expected a record, read {:?}", read.status);
    };
    assert_eq!(record.revision.as_str(), base);
    assert!(
        !record.history.iter().any(|applied| applied.step == reindex),
        "{:?}",
        record.history
    );
    // the operator finds nothing to reindex and records it `Skipped`, which moves the revision
    let mut skipped = seeded.clone();
    for applied in infra
        .iter()
        .cloned()
        .chain([entry(release, reindex, StepState::Skipped, 4)])
    {
        skipped.record(applied);
    }
    rewrite_state(&dir, &skipped);
    let output = run(thoradm(&ns)
        .args(["migrate", "record", "--revision", release, "--state"])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (_, read) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &read.status else {
        panic!("expected a record, read {:?}", read.status);
    };
    assert_eq!(record.revision.as_str(), release);
    let finalize = record
        .history
        .iter()
        .find(|applied| applied.step == "finalize")
        .expect("the Job records the finalize step");
    assert_eq!(finalize.outcome, StepState::Done);
    assert!(
        finalize
            .by
            .as_deref()
            .is_some_and(|by| by.starts_with("thoradm")),
        "{finalize:?}"
    );
    let rows = record.history_len;
    // a second run over the same state copies nothing and moves nothing
    let output = run(thoradm(&ns)
        .args(["migrate", "record", "--revision", release, "--state"])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (_, again) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &again.status else {
        panic!("expected a record, read {:?}", again.status);
    };
    assert_eq!(record.history_len, rows);
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// `migrate record --revision` takes the operator's `Done` of a gated step whose run it started,
/// which its Job's termination summary produced, so the revision moves, and the record holds
/// exactly one `Done` of the step even when the state later gains a second one
#[tokio::test]
async fn record_takes_the_operators_done_of_a_run_it_started_once() {
    // a record at the baseline, and a state whose gated step ran: `Running`, then the operator's
    // `Done`
    let ns = namespace("rcli_ran");
    create_legacy(&ns).await;
    let base = "2026-10-v01";
    let release = "2026-10-v02";
    let reindex = "elastic-reindex-keyword-mappings";
    let seeded = state_of(
        base,
        vec![entry(base, "convert-to-helm", StepState::Done, 0)],
    );
    let dir = seed(&ns, "ran", &seeded);
    let mut ran = seeded.clone();
    for applied in [
        entry(release, "scylla-role", StepState::Done, 1),
        entry(release, "elastic-identity", StepState::Done, 2),
        entry(release, reindex, StepState::Running, 3),
        entry(release, reindex, StepState::Done, 4),
    ] {
        ran.record(applied);
    }
    rewrite_state(&dir, &ran);
    let output = run(thoradm(&ns)
        .args(["migrate", "record", "--revision", release, "--state"])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (_, read) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &read.status else {
        panic!("expected a record, read {:?}", read.status);
    };
    assert_eq!(record.revision.as_str(), release);
    // a second `Done` of the step in the state is not copied beside the first
    let mut twice = ran.clone();
    twice.record(entry(release, reindex, StepState::Done, 5));
    rewrite_state(&dir, &twice);
    let output = run(thoradm(&ns)
        .args(["migrate", "record", "--revision", release, "--state"])
        .arg(&dir));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (_, again) = status(&ns, &[]);
    let RecordStatus::Recorded { record, .. } = &again.status else {
        panic!("expected a record, read {:?}", again.status);
    };
    let dones = record
        .history
        .iter()
        .filter(|applied| applied.step == reindex && applied.outcome == StepState::Done)
        .count();
    assert_eq!(dones, 1, "{:?}", record.history);
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_dir_all(&dir);
}
