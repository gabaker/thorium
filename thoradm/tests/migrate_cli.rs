//! Tests of `thoradm migrate run`, `check`, `issues`, and `report` as a Job runs them: exit codes,
//! signals, the writer lease, logs without a terminal, and what a check leaves behind

mod common;

use common::kill::run_until;
use common::migrate::{World, run_signal};
use serde_json::Value;
use std::fmt::Write as _;
use std::process::Output;
use thorium::models::schema::{MigrationRunStatus, WriterLease};

/// The arguments of a run of the fixture migration with one worker and a checkpoint per batch
///
/// # Arguments
///
/// * `id` - The migration
/// * `batch_rows` - The rows of each batch
fn run_args(id: &str, batch_rows: &str) -> Vec<String> {
    // one slice makes the interruption points deterministic
    [
        "migrate",
        "run",
        "--id",
        id,
        "--slices",
        "1",
        "--batch-rows",
        batch_rows,
        "--checkpoint-ms",
        "0",
    ]
    .map(String::from)
    .to_vec()
}

/// Parse the one JSON line a run prints to stdout
///
/// # Arguments
///
/// * `output` - The command's output
fn printed(output: &Output) -> Value {
    // the last line of stdout is the summary
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.lines().last().unwrap_or("null")).expect("stdout ends in JSON")
}

/// After SIGTERM mid-run the process finishes its batch, writes its checkpoint, and exits
/// promptly with the retryable code; a new run resumes and finishes
#[tokio::test]
async fn sigterm_checkpoints_and_exits_then_a_new_run_resumes() {
    // a table that takes many batches
    let world = World::new("cli02").await;
    world.seed_valid("a", 4000).await;
    let mut command = world.thoradm();
    command.args(run_args("fixture-theme", "100"));
    let run = run_signal(command, "migration batch finished", libc::SIGTERM)
        .await
        .unwrap();
    assert!(run.sent, "the run ended before it logged a batch");
    // it exited with the retryable code, well inside the grace period
    assert_eq!(run.status.code(), Some(4), "{:?}", run.stderr.last());
    assert!(
        run.after.unwrap() < std::time::Duration::from_secs(30),
        "{:?}",
        run.after
    );
    // its pass is running with a checkpoint on a batch boundary
    let row = world
        .store
        .pass_row("fixture")
        .await
        .unwrap()
        .expect("a pass row");
    assert_eq!(row.status, MigrationRunStatus::Running);
    let (workers, _) = world.store.rows("fixture", &row.attempt).await.unwrap();
    let read = workers[0].checkpoint.counts.read;
    assert!(read > 0 && read % 100 == 0 && read < 4000, "{read}");
    assert!(!workers[0].checkpoint.done);
    // a new run resumes the same attempt and converts every row exactly once
    let output = World::run(world.thoradm().args(run_args("fixture-theme", "100")));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = printed(&output);
    assert_eq!(summary["resumed"], true);
    assert_eq!(summary["attempt"], row.attempt.as_str());
    assert_eq!(summary["counts"]["read"], 4000);
    assert!(world.rows().await.iter().all(|row| row.2 == 2));
    world.drop_keyspace().await;
}

/// After SIGKILL a new run resumes from the last committed checkpoint, re-classifying the batch
/// that was in flight and skipping what it finished
#[tokio::test]
async fn sigkill_resumes_from_the_last_checkpoint() {
    // kill the run once it has logged a batch
    let world = World::new("cli03").await;
    world.seed_valid("a", 3000).await;
    let mut command = world.thoradm();
    command.args(run_args("fixture-theme", "100"));
    let killed = run_until(command, "migration batch finished")
        .await
        .unwrap();
    assert!(killed.killed, "{:?}", killed.stderr);
    let row = world
        .store
        .pass_row("fixture")
        .await
        .unwrap()
        .expect("a pass row");
    assert_eq!(row.status, MigrationRunStatus::Running);
    let (before, _) = world.store.rows("fixture", &row.attempt).await.unwrap();
    assert!(before[0].checkpoint.counts.read >= 100);
    // the rerun resumes the same attempt and the rows end up converted once
    let output = World::run(
        world
            .thoradm()
            .args(run_args("fixture-theme", "100"))
            .arg("--force-break-lease"),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = printed(&output);
    assert_eq!(summary["resumed"], true);
    assert_eq!(summary["attempt"], row.attempt.as_str());
    let rows = world.rows().await;
    assert_eq!(rows.len(), 3000);
    assert!(
        rows.iter()
            .all(|row| row.2 == 2 && row.1.matches("Dark").count() == 1)
    );
    world.drop_keyspace().await;
}

/// Issue log lines name the migration, object key, class, and reason code, a secret seeded in an
/// object appears nowhere, and the lines are there with no terminal
#[tokio::test]
async fn issue_logs_carry_codes_and_never_contents() {
    // an unreadable object holding a secret, and one whose write is rejected
    let world = World::new("cli09").await;
    world.seed_valid("a", 5).await;
    world
        .seed(&[
            (
                "leaky".into(),
                "{\"name\":5,\"token\":\"S3CRET-TOKEN-VALUE\"}".into(),
            ),
            ("reject-it".into(), "{\"name\":\"r\"}".into()),
        ])
        .await;
    let log = World::termination_log("cli09");
    let output = World::run(
        world
            .thoradm()
            .args(run_args("fixture-theme", "100"))
            .env("THORIUM_TERMINATION_LOG", &log),
    );
    assert_eq!(output.status.code(), Some(0));
    // stderr isn't a terminal here, and still holds each issue with what identifies it
    let stderr = String::from_utf8_lossy(&output.stderr);
    let leaky = stderr
        .lines()
        .find(|line| line.contains("key=leaky"))
        .expect("the issue is logged");
    for part in [
        "migration=fixture-theme",
        "class=unrecognized",
        "reason=bad_type:body.name",
    ] {
        assert!(leaky.contains(part), "{leaky}");
    }
    let rejected = stderr
        .lines()
        .find(|line| line.contains("key=reject-it"))
        .expect("the rejected write is logged");
    assert!(
        rejected.contains("class=error") && rejected.contains("write_failed:rejected"),
        "{rejected}"
    );
    // the secret is in no output, no issue row, and no check report
    let check = World::run(world.thoradm().args([
        "migrate",
        "check",
        "--id",
        "fixture-theme",
        "--all",
        "--samples",
    ]));
    let rows = world
        .session()
        .query_unpaged(
            format!("SELECT * FROM {}.schema_migration_issues", world.ns),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let issue_rows = format!(
        "{:?}",
        rows.rows::<scylla::value::Row>()
            .unwrap()
            .map(|row| row.unwrap())
            .collect::<Vec<_>>()
    );
    let summary = std::fs::read_to_string(&log).unwrap_or_default();
    for text in [
        stderr.to_string(),
        String::from_utf8_lossy(&check.stdout).into_owned(),
        String::from_utf8_lossy(&check.stderr).into_owned(),
        issue_rows,
        summary,
    ] {
        assert!(!text.contains("S3CRET"), "{text}");
    }
    world.drop_keyspace().await;
}

/// A check prints the report, writes it to its own result row, and changes nothing else; with
/// `--all --samples` the groups and their samples go to stdout only
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "a scenario test reads top to bottom")]
async fn a_check_writes_only_its_result_row() {
    // seeded fixtures with two bad groups, and a state of the world to compare
    let world = World::new("cli21").await;
    world.seed_valid("a", 30).await;
    world
        .seed(&[
            ("b1".into(), "{\"name\":5}".into()),
            ("b2".into(), "{\"nme\":1}".into()),
        ])
        .await;
    let state = || async {
        let session = world.session();
        let mut text = String::new();
        for cql in [
            format!(
                "SELECT kind, id, fingerprint, revision, status, attempt FROM {}.schema_migrations",
                world.ns
            ),
            format!(
                "SELECT migration, transform, reason, worker, count FROM {}.schema_migration_issues",
                world.ns
            ),
            format!("SELECT id, body, layout FROM {}.mig_fixture", world.ns),
        ] {
            let rows = session
                .query_unpaged(cql, ())
                .await
                .unwrap()
                .into_rows_result()
                .unwrap();
            let mut lines = rows
                .rows::<scylla::value::Row>()
                .unwrap()
                .map(|row| format!("{:?}", row.unwrap()))
                .filter(|line| !line.contains("job_result"))
                .collect::<Vec<_>>();
            lines.sort();
            text.push_str(&lines.join("\n"));
        }
        let mut conn = world.backends.redis.get().await.unwrap();
        let keys: Vec<String> = common::harness::scan_keys(&mut *conn, &format!("{}:*", world.ns))
            .await
            .unwrap();
        write!(text, "{keys:?}").unwrap();
        text
    };
    let before = state().await;
    // the plain check prints a report without sample keys
    let output = World::run(world.thoradm().args([
        "migrate",
        "check",
        "--id",
        "fixture-theme",
        "--slices",
        "2",
    ]));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = printed(&output);
    assert_eq!(report["counts"]["migratable"], 30);
    assert_eq!(report["counts"]["unrecognized"], 2);
    assert_eq!(report["families"][0], "fixture");
    assert_eq!(report["reasons"].as_array().unwrap().len(), 2);
    assert!(report.get("groups").is_none());
    // its only write is the result row, which holds the same report
    assert_eq!(state().await, before);
    let result = world
        .session()
        .query_unpaged(
            format!(
                "SELECT result FROM {}.schema_migrations WHERE kind = 'job_result'",
                world.ns
            ),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let stored: Vec<(Option<String>,)> = result
        .rows::<(Option<String>,)>()
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(stored.len(), 1);
    let stored: Value = serde_json::from_str(stored[0].0.as_deref().unwrap()).unwrap();
    assert_eq!(stored, report);
    // with the flags the groups and their samples are printed, and still never stored
    let output = World::run(world.thoradm().args([
        "migrate",
        "check",
        "--id",
        "fixture-theme",
        "--all",
        "--samples",
    ]));
    let full = printed(&output);
    let groups = full["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .any(|group| group["samples"] == serde_json::json!(["b1"]))
    );
    assert_eq!(state().await, before);
    let result = world
        .session()
        .query_unpaged(
            format!(
                "SELECT result FROM {}.schema_migrations WHERE kind = 'job_result'",
                world.ns
            ),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    for row in result.rows::<(Option<String>,)>().unwrap() {
        assert!(!row.unwrap().0.unwrap().contains("samples"));
    }
    world.drop_keyspace().await;
}

/// A run that fails on its groups exits with its own code and summarizes the largest groups; one
/// that fails systematically exits with another
#[tokio::test]
async fn failures_have_their_own_exit_codes_and_summaries() {
    // six bad objects in one group are over the limit of five
    let world = World::new("cli05").await;
    world
        .seed(
            &(0..6)
                .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
                .collect::<Vec<_>>(),
        )
        .await;
    let log = World::termination_log("cli05");
    let output = World::run(
        world
            .thoradm()
            .args(run_args("fixture-theme", "100"))
            .env("THORIUM_TERMINATION_LOG", &log),
    );
    assert_eq!(output.status.code(), Some(5));
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
    assert_eq!(summary["outcome"], "failed_issues");
    assert_eq!(summary["groups"][0]["code"], "bad_type:body.name");
    assert_eq!(summary["groups"][0]["count"], 6);
    assert_eq!(
        world.status("fixture").await,
        Some(MigrationRunStatus::Failed)
    );
    // more than 100 distinct groups in one worker is systematic
    let world2 = World::new("cli05b").await;
    world2
        .seed(
            &(0..101)
                .map(|n| (format!("g{n}"), format!("poison:{n}")))
                .collect::<Vec<_>>(),
        )
        .await;
    let log = World::termination_log("cli05b");
    let mut command = world2.thoradm();
    command
        .args(run_args("fixture-theme", "200"))
        .args(["--accept-issues", "x"])
        .env("THORIUM_TERMINATION_LOG", &log);
    let output = World::run(&mut command);
    assert_eq!(
        output.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
    assert_eq!(summary["outcome"], "failed_systematic");
    world.drop_keyspace().await;
    world2.drop_keyspace().await;
}

/// `--reason` exits non-zero while the group still holds objects, printing its count before and
/// after, and exits 0 once they are fixed
#[tokio::test]
async fn a_reason_run_exits_nonzero_while_the_group_holds_objects() {
    // two groups, the pass run once
    let world = World::new("cli34").await;
    world
        .seed(&[
            ("p1".into(), "{\"nme\":1}".into()),
            ("q1".into(), "{\"name\":5}".into()),
        ])
        .await;
    let first = World::run(world.thoradm().args(run_args("fixture-theme", "100")));
    assert_eq!(first.status.code(), Some(0));
    let reason = ["--reason", "missing_field:body.name"];
    // unfixed: the group still holds its object
    let unfixed = World::run(
        world
            .thoradm()
            .args(run_args("fixture-theme", "100"))
            .args(reason),
    );
    assert_eq!(
        unfixed.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&unfixed.stderr)
    );
    let text = String::from_utf8_lossy(&unfixed.stdout);
    assert!(
        text.contains("missing_field:body.name: 1 objects before, 1 after"),
        "{text}"
    );
    // fixed: the group is empty and the run exits 0
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
        .unwrap();
    let fixed = World::run(
        world
            .thoradm()
            .args(run_args("fixture-theme", "100"))
            .args(reason),
    );
    assert_eq!(
        fixed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    assert!(String::from_utf8_lossy(&fixed.stdout).contains("1 objects before, 0 after"));
    // `issues` lists what is left
    let issues = World::run(
        world
            .thoradm()
            .args(["migrate", "issues", "--id", "fixture-theme"]),
    );
    let listed = String::from_utf8_lossy(&issues.stdout).into_owned();
    assert!(
        listed.contains("bad_type:body.name class=unrecognized count=1"),
        "{listed}"
    );
    let one = World::run(world.thoradm().args([
        "migrate",
        "issues",
        "--id",
        "fixture-theme",
        "--reason",
        "bad_type:body.name",
    ]));
    assert!(String::from_utf8_lossy(&one.stdout).contains("  q1"));
    world.drop_keyspace().await;
}

/// A running entry that isn't a history entry is a usage error, so no run silently ignores what
/// it was told
#[tokio::test]
async fn unbuilt_and_conflicting_flags_are_refused() {
    // the running entry must be an entry
    let world = World::new("cli05c").await;
    let output = World::run(world.thoradm().args([
        "migrate",
        "run",
        "--id",
        "fixture-theme",
        "--running-entry",
        "{}",
    ]));
    assert_eq!(output.status.code(), Some(2));
    // objects and a reason can't both be given
    let both = World::run(world.thoradm().args([
        "migrate",
        "run",
        "--id",
        "fixture-theme",
        "--objects",
        "x",
        "--reason",
        "bad_type:body.name",
    ]));
    assert_ne!(both.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&both.stderr).contains("cannot be used with"));
    world.drop_keyspace().await;
}

/// `--reason` with a code no group of the pass has is a usage error that scans nothing and leaves
/// no lease behind, and a check leaves a termination summary naming its result row
#[tokio::test]
async fn an_unknown_reason_is_refused_and_a_check_summarizes() {
    // a finished pass with one group
    let world = World::new("cli34u").await;
    world.seed(&[("q1".into(), "{\"name\":5}".into())]).await;
    assert_eq!(
        World::run(world.thoradm().args(run_args("fixture-theme", "100")))
            .status
            .code(),
        Some(0)
    );
    let refused = World::run(
        world
            .thoradm()
            .args(run_args("fixture-theme", "100"))
            .args(["--reason", "missing_field:nope"]),
    );
    assert_eq!(refused.status.code(), Some(2));
    // the lease was given back
    let free = WriterLease::acquire(&world.backends.redis, &world.ns, "next/x/1", false).await;
    assert!(free.is_ok());
    drop(free);
    // a check writes its summary with the id of its result row
    let log = World::termination_log("cli34u");
    let check = World::run(
        world
            .thoradm()
            .args(["migrate", "check", "--id", "fixture-theme"])
            .env("THORIUM_TERMINATION_LOG", &log)
            .env("THORIUM_JOB_UID", "uid-123"),
    );
    assert_eq!(check.status.code(), Some(0));
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
    assert_eq!(summary["outcome"], "done");
    assert_eq!(summary["result_id"], "uid-123");
    world.drop_keyspace().await;
}

/// A migration that can only be checked is checked normally and writes nothing but its result row,
/// while `run` and `verify` refuse it with the not-implemented exit code before a pass row or a
/// lease exists
#[tokio::test]
async fn a_check_only_migration_is_checked_but_never_run() {
    // rows in the old layout
    let world = World::new("cli21c").await;
    world.seed_valid("a", 10).await;
    let check = World::run(
        world
            .thoradm()
            .args(["migrate", "check", "--id", "fixture-checkonly"]),
    );
    assert_eq!(
        check.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert_eq!(printed(&check)["counts"]["migratable"], 10);
    // run and verify exit 3
    let run = World::run(world.thoradm().args(run_args("fixture-checkonly", "100")));
    assert_eq!(run.status.code(), Some(3));
    let verify =
        World::run(
            world
                .thoradm()
                .args(["migrate", "verify", "--id", "fixture-checkonly"]),
        );
    assert_eq!(verify.status.code(), Some(3));
    // nothing was written: no pass row, the rows are as seeded, and the lease is free
    assert!(
        world
            .store
            .pass_row("fixture-checkonly")
            .await
            .unwrap()
            .is_none()
    );
    assert!(world.rows().await.iter().all(|row| row.2 == 1));
    let free = WriterLease::acquire(&world.backends.redis, &world.ns, "next/x/1", false).await;
    assert!(free.is_ok());
    world.drop_keyspace().await;
}

/// A converted pre-Helm deployment holds only the `schema_migrations` table its seeded record
/// made until its 2026-10-v03 schema apply, as when an admin rebuilds Elasticsearch for the
/// 2026-10-v02 reindex step: a run creates the record table it keeps its pass state in and finishes
#[tokio::test]
async fn a_run_creates_the_record_table_a_converted_deployment_lacks() {
    // rows in the old layout, in a keyspace without schema_migration_issues
    let world = World::new("cli31r").await;
    world.seed_valid("a", 4).await;
    world
        .session()
        .query_unpaged(
            format!("DROP TABLE {}.schema_migration_issues", world.ns),
            (),
        )
        .await
        .expect("the issues table drops");
    // the run finishes and converts every row
    let run = World::run(world.thoradm().args(run_args("fixture-theme", "100")));
    assert_eq!(
        run.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        world.status("fixture").await,
        Some(MigrationRunStatus::Done)
    );
    assert!(world.rows().await.iter().all(|row| row.2 == 2));
    world.drop_keyspace().await;
}

/// A manual run is refused while a Job holds the writer lease, naming the holder, and with
/// `--force-break-lease` it proceeds and the holder's token goes stale
#[tokio::test]
async fn a_held_lease_refuses_a_manual_run_until_it_is_broken() {
    // a finished pass, so `--objects` has a pass to fix, and a lease held by a Job
    let world = World::new("cli40").await;
    world.seed(&[("x1".into(), "{\"name\":5}".into())]).await;
    assert_eq!(
        World::run(world.thoradm().args(run_args("fixture-theme", "100")))
            .status
            .code(),
        Some(0)
    );
    let held = world.lease("migrate-job-7").await;
    let args = ["migrate", "run", "--id", "fixture-theme", "--objects", "x1"];
    let refused = World::run(world.thoradm().args(args));
    assert_ne!(refused.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("migrate-job-7"), "{stderr}");
    // `migrate run` accepts the flag, so its refusal offers it
    assert!(stderr.contains("--force-break-lease"), "{stderr}");
    // breaking the lease lets it proceed, and the Job's token is no longer current
    let forced = World::run(world.thoradm().args(args).arg("--force-break-lease"));
    assert_eq!(
        forced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(!held.is_current().await.unwrap());
    // the forced run released its own lease on the way out
    let free = WriterLease::acquire(&world.backends.redis, &world.ns, "next", false).await;
    assert!(free.is_ok());
    world.drop_keyspace().await;
}

/// A bad object on a slice boundary is counted once, whichever worker's slice it ends
#[tokio::test]
async fn a_bad_row_on_a_slice_boundary_is_counted_once() {
    // many valid rows and one bad row whose token ends a slice
    let world = World::new("cli43").await;
    world.seed_valid("a", 200).await;
    world.seed(&[("edge".into(), "{\"name\":5}".into())]).await;
    let (token,): (i64,) = world
        .session()
        .query_unpaged(
            format!(
                "SELECT token(id) FROM {}.mig_fixture WHERE id = 'edge'",
                world.ns
            ),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .single_row()
        .unwrap();
    for slices in ["2", "3", "8"] {
        // cut the ring right after the bad row's token, so it ends a slice
        let mut command = world.thoradm();
        command
            .args([
                "migrate",
                "run",
                "--id",
                "fixture-theme",
                "--slices",
                slices,
                "--batch-rows",
                "50",
                "--checkpoint-ms",
                "0",
            ])
            .env("THORADM_FIXTURE_CUT", token.to_string());
        let output = World::run(&mut command);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let summary = printed(&output);
        // no row was read twice, and the bad row is in one group of one
        assert_eq!(summary["counts"]["read"], 201, "{slices} slices");
        assert_eq!(summary["groups"][0]["count"], 1, "{slices} slices");
        // put the rows back for the next slice count
        for n in 0..200 {
            world
                .session()
                .query_unpaged(
                    format!("UPDATE {}.mig_fixture SET body = '{{\"name\":\"x\"}}', layout = 1 WHERE id = 'a-{n}'", world.ns),
                    (),
                )
                .await
                .unwrap();
        }
    }
    world.drop_keyspace().await;
}

/// The applied report is stored with the pass and printed by `migrate report`
#[tokio::test]
async fn the_applied_report_is_stored_and_printed() {
    // a pass with a converted row, a skipped one, and counts
    let world = World::new("cliap").await;
    world.seed_valid("a", 4).await;
    world.seed(&[("bad".into(), "{\"name\":5}".into())]).await;
    assert_eq!(
        World::run(world.thoradm().args(run_args("fixture-theme", "100")))
            .status
            .code(),
        Some(0)
    );
    let output = World::run(
        world
            .thoradm()
            .args(["migrate", "report", "--id", "fixture-theme"]),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = printed(&output);
    assert_eq!(report["groups"]["fixture"]["changed"], 4);
    assert_eq!(report["groups"]["fixture"]["skipped"], 1);
    assert_eq!(report["skipped_total"], 1);
    assert_eq!(report["skipped"][0]["item"], "bad");
    assert_eq!(report["skipped"][0]["reason"], "bad_type:body.name");
    // the pass row's counts are stored too
    let row = world.store.pass_row("fixture").await.unwrap().unwrap();
    assert_eq!(row.counts.unwrap().written, 4);
    world.drop_keyspace().await;
}

/// `schema apply` is a writer too: it is refused while another writer holds the lease, naming the
/// holder, a dry run takes no lease, and with `--force-break-lease` it proceeds and gives the
/// lease up when it is done
#[tokio::test]
async fn schema_apply_takes_the_writer_lease() {
    // a lease held by a migration Job
    let world = World::new("cli40a").await;
    let held = world.lease("migrate-job-9").await;
    let refused = World::run(world.thoradm().args(["schema", "apply"]));
    assert_ne!(refused.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("migrate-job-9"), "{stderr}");
    // `schema apply` accepts the flag, so its refusal offers it
    assert!(stderr.contains("--force-break-lease"), "{stderr}");
    // a dry run changes nothing, so it needs no lease
    let dry = World::run(world.thoradm().args(["schema", "apply", "--dry-run"]));
    assert_eq!(
        dry.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    // breaking the lease lets the apply run, which leaves the lease free
    let forced = World::run(
        world
            .thoradm()
            .args(["schema", "apply", "--force-break-lease"]),
    );
    assert_eq!(
        forced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(!held.is_current().await.unwrap());
    let free = WriterLease::acquire(&world.backends.redis, &world.ns, "next", false).await;
    assert!(free.is_ok());
    world.drop_keyspace().await;
}

/// A Scylla node paused longer than the driver's request timeout makes reads and writes time out;
/// the run backs off, retries, and finishes with no object recorded as an issue
///
/// It needs a disposable Scylla container the test may pause, which a CI job can't do to its own
/// Scylla, so it is `#[ignore]`d and `THORADM_TEST_DOCKER_SCYLLA` must name the container. The
/// pause stops every test that shares that container, so run it alone. A stalled connection
/// through the fault proxy covers the runner's retries in CI.
#[tokio::test]
#[ignore = "pauses a Scylla container with docker for 40 s, which a CI job cannot do; run by hand against a disposable Scylla container: THORADM_TEST_DOCKER_SCYLLA=<container> cargo test -p thoradm --test migrate_cli -- --ignored a_paused_scylla_is_retried_through"]
async fn a_paused_scylla_is_retried_through() {
    // the environment must name the container to pause, since the test was asked for by hand
    let container = std::env::var("THORADM_TEST_DOCKER_SCYLLA")
        .expect("set THORADM_TEST_DOCKER_SCYLLA to the disposable Scylla container to pause");
    let world = World::new("cli04p").await;
    world.seed_valid("a", 8000).await;
    // pause the container once the run has logged a batch, and unpause it after the timeouts
    let mut command = world.thoradm();
    command
        .args(run_args("fixture-theme", "10"))
        .args(["--retries", "60", "--backoff-ms", "500"]);
    let pause = container.clone();
    let pauser = tokio::spawn(async move {
        // wait until the run has been going a moment
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let docker = |verb: &str| {
            let status = std::process::Command::new("docker")
                .args([verb, &pause])
                .status()
                .expect("docker runs");
            assert!(status.success(), "docker {verb}");
        };
        docker("pause");
        tokio::time::sleep(std::time::Duration::from_secs(40)).await;
        docker("unpause");
    });
    let output = tokio::task::spawn_blocking(move || World::run(&mut command))
        .await
        .unwrap();
    pauser.await.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // the pause was felt: some read or write was retried, and none of it became an issue
    assert!(stderr.contains("retrying"), "{stderr}");
    let summary = printed(&output);
    assert_eq!(summary["groups"].as_array().unwrap().len(), 0);
    assert_eq!(summary["counts"]["read"], 8000);
    assert!(world.rows().await.iter().all(|row| row.2 == 2));
    world.drop_keyspace().await;
}
