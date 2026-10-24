//! Tests of `thoradm schema apply --recreate-view` and `thoradm schema verify-views` from the
//! command line: which views they refuse, the recreate's confirmation, what they print, their exit
//! codes, and the recreate's termination summary

use scylla::client::session::Session;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use thorium::Conf;
use thorium::models::schema::{RevisionSnapshot, connect};
use thorium::models::upgrades::latest;
use uuid::Uuid;

#[path = "common/base_namespace.rs"]
mod base_namespace;

/// The test config every run starts from; the environment overrides point it at the test cluster
const CONFIG: &str = "../api/tests/thorium-testing.yml";

/// Build a thoradm command against the test config with a namespace of its own, with no terminal
/// attached to answer a prompt
///
/// # Arguments
///
/// * `namespace` - The namespace, and so the keyspace, the command uses
fn thoradm(namespace: &str) -> Command {
    // run the binary cargo built for these tests against the test config
    let mut command = Command::new(env!("CARGO_BIN_EXE_thoradm"));
    command
        .args(["--cluster-conf", CONFIG])
        .env("THORIUM__THORIUM__NAMESPACE", namespace)
        .stdin(Stdio::null());
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

/// A fresh file for the termination summary
///
/// # Arguments
///
/// * `name` - What makes the file unique
fn termination_log(name: &str) -> PathBuf {
    // a file under the temporary directory, removed if an earlier run left it
    let path =
        std::env::temp_dir().join(format!("thoradm-views-{name}-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
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

/// Read a termination summary
///
/// # Arguments
///
/// * `log` - The file it was written to
fn summary(log: &PathBuf) -> Value {
    // the summary is always one JSON object
    serde_json::from_str(&std::fs::read_to_string(log).expect("the summary was written"))
        .expect("the summary is JSON")
}

/// Open a session with the test config
async fn session() -> Session {
    // connect with the same config the commands use
    let conf = Conf::new(CONFIG).expect("the test config loads");
    connect(&conf).await.expect("Scylla is reachable")
}

/// Drop a keyspace, then converge it with `thoradm schema apply`
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create
async fn applied(session: &Session, keyspace: &str) {
    // start from nothing, then apply like the operator's Job
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await
        .expect("the keyspace drops");
    let output = run(thoradm(keyspace).args(["schema", "apply", "--wait-for-scylla", "60"]));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The id Scylla gives a view, which changes when the view is recreated
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the view
/// * `view` - The view
async fn view_id(session: &Session, keyspace: &str, view: &str) -> Option<Uuid> {
    // read the view's id from the schema tables
    session
        .query_unpaged(
            "SELECT id FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?",
            (keyspace, view),
        )
        .await
        .expect("the schema reads")
        .into_rows_result()
        .expect("rows")
        .maybe_first_row::<(Uuid,)>()
        .expect("an id")
        .map(|(id,)| id)
}

/// Drop a test's keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to drop
async fn drop_keyspace(session: &Session, keyspace: &str) {
    // nothing is left for the next run
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await
        .expect("the keyspace drops");
}

/// A name that isn't a declared view exits 1 with a refusal summary, and nothing is dropped: not
/// the extra view, and not a declared view named beside it
#[tokio::test]
async fn an_undeclared_view_exits_1_and_nothing_is_dropped() {
    // a converged keyspace with an extra view made by hand
    let session = session().await;
    let ns = namespace("vcli_undeclared");
    applied(&session, &ns).await;
    session
        .query_unpaged(
            format!(
                "CREATE MATERIALIZED VIEW {ns}.extra_by_name AS SELECT name, group, year, \
                 bucket, uploaded, id FROM {ns}.samples_list WHERE name IS NOT NULL AND group IS \
                 NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL \
                 AND id IS NOT NULL PRIMARY KEY ((name), group, year, bucket, uploaded, id)"
            ),
            (),
        )
        .await
        .expect("the extra view is created");
    let extra = view_id(&session, &ns, "extra_by_name").await;
    let samples = view_id(&session, &ns, "samples").await;
    // recreating it, alone or beside a declared view, is refused
    for names in [vec!["extra_by_name"], vec!["samples", "extra_by_name"]] {
        let log = termination_log("undeclared");
        let mut command = thoradm(&ns);
        command.args(["schema", "apply", "--yes"]);
        for name in &names {
            command.args(["--recreate-view", name]);
        }
        let output = run(command.env("THORIUM_TERMINATION_LOG", &log));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("extra_by_name"), "{stderr}");
        let summary = summary(&log);
        assert_eq!(summary["outcome"], "failed_systematic");
        assert_eq!(summary["exit_code"], 1);
        assert_eq!(summary["retryable"], false);
        let _ = std::fs::remove_file(&log);
    }
    // both views are untouched
    assert_eq!(view_id(&session, &ns, "extra_by_name").await, extra);
    assert_eq!(view_id(&session, &ns, "samples").await, samples);
    drop_keyspace(&session, &ns).await;
}

/// Without a terminal, `--recreate-view` without `--yes` exits 1 naming `--yes`, and the view is
/// untouched
#[tokio::test]
async fn without_a_terminal_yes_is_required() {
    // a converged keyspace
    let session = session().await;
    let ns = namespace("vcli_noyes");
    applied(&session, &ns).await;
    let before = view_id(&session, &ns, "samples").await;
    // recreate with no terminal and no --yes
    let log = termination_log("noyes");
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--recreate-view", "samples"])
        .env("THORIUM_TERMINATION_LOG", &log));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
    assert_eq!(summary(&log)["exit_code"], 1);
    assert_eq!(view_id(&session, &ns, "samples").await, before);
    let _ = std::fs::remove_file(&log);
    drop_keyspace(&session, &ns).await;
}

/// With `--yes`, each named declared view is dropped and created again: the run exits 0, says what
/// recreating costs, prints the statements it issued, and writes a summary naming the recorded
/// fingerprint and the views built
#[tokio::test]
async fn yes_recreates_each_named_view_and_reports_it() {
    // a converged keyspace
    let session = session().await;
    let ns = namespace("vcli_yes");
    applied(&session, &ns).await;
    let samples = view_id(&session, &ns, "samples").await;
    let tags = view_id(&session, &ns, "tags_by_item").await;
    let other = view_id(&session, &ns, "repos").await;
    // recreate two views, one named twice
    let log = termination_log("yes");
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--yes"])
        .args([
            "--recreate-view",
            "samples",
            "--recreate-view",
            "tags_by_item",
        ])
        .args(["--recreate-view", "samples"])
        .env("THORIUM_TERMINATION_LOG", &log));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("routes reading it fail while it is dropped, then see missing rows"),
        "{stderr}"
    );
    // the printed report lists the drop and create statements
    let report: Value = serde_json::from_slice(&output.stdout).expect("the report is JSON");
    let statements = report["statements"]
        .as_array()
        .expect("statements")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    for view in ["samples", "tags_by_item"] {
        let drop = format!("DROP MATERIALIZED VIEW IF EXISTS {ns}.{view}");
        assert_eq!(
            statements
                .iter()
                .filter(|statement| **statement == drop)
                .count(),
            1,
            "{statements:?}"
        );
        assert!(
            statements
                .iter()
                .any(|statement| statement.starts_with(&format!(
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.{view} "
                ))),
            "{statements:?}"
        );
    }
    // the summary has the shape every apply writes, with the views built
    let summary = summary(&log);
    assert_eq!(summary["outcome"], "done");
    assert_eq!(
        summary["fingerprint"],
        RevisionSnapshot::declared(latest()).deployment.as_str()
    );
    assert_eq!(
        summary["views_built"],
        serde_json::json!(["samples", "tags_by_item"])
    );
    // the two views were replaced, and no other
    let after = view_id(&session, &ns, "samples").await;
    assert!(after.is_some() && after != samples);
    let after = view_id(&session, &ns, "tags_by_item").await;
    assert!(after.is_some() && after != tags);
    assert_eq!(view_id(&session, &ns, "repos").await, other);
    let _ = std::fs::remove_file(&log);
    drop_keyspace(&session, &ns).await;
}

/// A dry run needs no confirmation, prints the statements a real run would issue, and drops
/// nothing
#[tokio::test]
async fn a_dry_run_prints_the_recreate_and_drops_nothing() {
    // a converged keyspace
    let session = session().await;
    let ns = namespace("vcli_dry");
    applied(&session, &ns).await;
    let before = view_id(&session, &ns, "samples").await;
    // dry-run a recreate with no terminal and no --yes
    let output =
        run(thoradm(&ns).args(["schema", "apply", "--dry-run", "--recreate-view", "samples"]));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("DROP MATERIALIZED VIEW IF EXISTS {ns}.samples;")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.samples "
        )),
        "{stdout}"
    );
    assert_eq!(view_id(&session, &ns, "samples").await, before);
    drop_keyspace(&session, &ns).await;
}

/// The `--recreate-view` help says what routes reading the view see until its build finishes
#[test]
fn the_recreate_help_says_what_readers_see() {
    // help prints to stdout and succeeds; whitespace is collapsed since help may wrap
    let output =
        run(Command::new(env!("CARGO_BIN_EXE_thoradm")).args(["schema", "apply", "--help"]));
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        help.contains(
            "routes reading it fail while it is dropped, then see missing rows until its build \
             finishes"
        ),
        "{help}"
    );
}

/// A config that can't be loaded exits 2 and a Scylla that can't be reached exits 4, as a plain
/// apply does
#[test]
fn config_and_connection_failures_exit_like_apply() {
    // a missing config is a usage error
    let output = run(Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args([
            "--cluster-conf",
            "/nonexistent/thorium.yml",
            "schema",
            "apply",
        ])
        .args(["--recreate-view", "samples", "--yes"]));
    assert_eq!(output.status.code(), Some(2));
    // an unreachable Scylla may come back, so the Job retries
    let log = termination_log("down");
    let output = run(thoradm(&namespace("vcli_down"))
        .args(["schema", "apply", "--wait-for-scylla", "1"])
        .args(["--recreate-view", "samples", "--yes"])
        .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1")
        .env("THORIUM__SCYLLA__SETUP_TIME", "1")
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(summary(&log)["retryable"], true);
    let _ = std::fs::remove_file(&log);
}

/// Write a few sample rows and wait until the `samples` view holds them
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
async fn seed_samples(session: &Session, keyspace: &str) {
    // write rows spread over several partitions
    for i in 0..40_i32 {
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, \
                     sha256) VALUES ('cli', 2026, ?, ?, ?, ?)"
                ),
                (
                    i % 8,
                    scylla::value::CqlTimestamp(1_700_000_000_000 + i64::from(i)),
                    Uuid::new_v4(),
                    format!("{i:064x}"),
                ),
            )
            .await
            .expect("a sample row is written");
    }
    // view updates land in the background
    for _ in 0..300 {
        let held = session
            .query_unpaged(format!("SELECT count(*) FROM {keyspace}.samples"), ())
            .await
            .expect("the view reads")
            .into_rows_result()
            .expect("rows")
            .single_row::<(i64,)>()
            .expect("a count")
            .0;
        if held == 40 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the samples view never caught up");
}

/// Every view in step exits 0 and prints a report naming each in step, in the full mode and for
/// one view in the sampled mode
#[tokio::test]
async fn verify_views_exits_0_when_every_view_is_in_step() {
    // a converged keyspace with sample rows
    let session = session().await;
    let ns = namespace("vcli_verify_ok");
    applied(&session, &ns).await;
    seed_samples(&session, &ns).await;
    // the full check reports every view in step
    let output = run(thoradm(&ns).args(["schema", "verify-views"]));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("the report is JSON");
    let views = report["views"].as_array().expect("views");
    assert_eq!(views.len(), 12);
    assert!(
        views
            .iter()
            .all(|view| view["state"]["status"] == "in_step"),
        "{report}"
    );
    // one view in the sampled mode checks only the ranges asked for
    let output = run(thoradm(&ns).args([
        "schema",
        "verify-views",
        "--view",
        "samples",
        "--sample",
        "4",
    ]));
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).expect("the report is JSON");
    assert_eq!(report["sampled"], true);
    assert_eq!(report["views"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        report["views"][0]["ranges"].as_array().map(Vec::len),
        Some(4)
    );
    drop_keyspace(&session, &ns).await;
}

/// A view out of step exits 1 and is listed; recreating it with `--recreate-view` brings the
/// check back to 0
#[tokio::test]
async fn verify_views_exits_1_listing_views_out_of_step() {
    // a converged keyspace with one declared view dropped by hand
    let session = session().await;
    let ns = namespace("vcli_verify_bad");
    applied(&session, &ns).await;
    session
        .query_unpaged(format!("DROP MATERIALIZED VIEW {ns}.comments_by_id"), ())
        .await
        .expect("the view drops");
    // the check exits 1 and names the view
    let output = run(thoradm(&ns).args(["schema", "verify-views"]));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("comments_by_id"), "{stderr}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("the report is JSON");
    let out = report["views"]
        .as_array()
        .expect("views")
        .iter()
        .filter(|view| view["state"]["status"] != "in_step")
        .map(|view| view["view"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(out, ["comments_by_id"]);
    // recreating it brings every view back in step
    let output = run(thoradm(&ns).args([
        "schema",
        "apply",
        "--recreate-view",
        "comments_by_id",
        "--yes",
    ]));
    assert_eq!(output.status.code(), Some(0));
    let output = run(thoradm(&ns).args(["schema", "verify-views"]));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop_keyspace(&session, &ns).await;
}

/// An undeclared view, a sample of no ranges, and a config that can't be loaded exit 2; a Scylla
/// that can't be reached exits 4
#[test]
fn verify_views_usage_and_connection_failures() {
    // usage errors are found before Scylla is tried
    for extra in [
        &["--view", "samples_list"][..],
        &["--sample", "0"][..],
        &["--sample", "257"][..],
    ] {
        let output = run(thoradm(&namespace("vcli_verify_usage"))
            .args(["schema", "verify-views"])
            .args(extra)
            .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1")
            .env("THORIUM__SCYLLA__SETUP_TIME", "1"));
        assert_eq!(
            output.status.code(),
            Some(2),
            "{extra:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = run(Command::new(env!("CARGO_BIN_EXE_thoradm")).args([
        "--cluster-conf",
        "/nonexistent/thorium.yml",
        "schema",
        "verify-views",
    ]));
    assert_eq!(output.status.code(), Some(2));
    // an unreachable Scylla may come back
    let output = run(thoradm(&namespace("vcli_verify_down"))
        .args(["schema", "verify-views"])
        .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1")
        .env("THORIUM__SCYLLA__SETUP_TIME", "1"));
    assert_eq!(output.status.code(), Some(4));
}
