//! Tests of `thoradm schema diff` and of the drift summary `thoradm schema status` reports: a
//! matching deployment, a table dropped by hand, acknowledged and unacknowledged extra objects, and
//! the capped summary of many

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};
use thorium::Conf;
use thorium::models::schema::RESULT_PREFIX;
use tokio::sync::Mutex;

#[path = "schema_diff_cli/manual.rs"]
mod manual;

#[path = "common/base_namespace.rs"]
mod base_namespace;

/// The test config every run starts from; the environment overrides point it at the test cluster
const CONFIG: &str = "../api/tests/thorium-testing.yml";

/// Serializes the tests, which share the Elastic indexes the config names
static ELASTIC: Mutex<()> = Mutex::const_new(());

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

/// Run a command, returning its output
///
/// # Arguments
///
/// * `command` - The command to run
fn run(command: &mut Command) -> Output {
    // run to completion, keeping stdout and stderr
    command.output().expect("thoradm runs")
}

/// The text a command wrote to stderr, for failure messages
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // lossy, since only a message reads it
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Connect to Scylla with the test config
async fn session() -> scylla::client::session::Session {
    // the same config the command uses
    let conf = Conf::new(CONFIG).expect("the test config loads");
    thorium::models::schema::connect(&conf)
        .await
        .expect("Scylla is reachable")
}

/// Run a statement against Scylla
///
/// # Arguments
///
/// * `cql` - The statement
async fn exec(cql: &str) {
    // run it unpaged, naming it in a failure
    session()
        .await
        .query_unpaged(cql, ())
        .await
        .unwrap_or_else(|err| panic!("{cql}: {err}"));
}

/// Apply the declared schema to a namespace's keyspace as the operator's Job does
///
/// # Arguments
///
/// * `ns` - The namespace to apply to
fn apply(ns: &str) {
    // the Job's command line
    let output = run(thoradm(ns).args(["schema", "apply", "--wait-for-scylla", "60"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

/// Run `schema diff --json`, returning its exit code and report
///
/// # Arguments
///
/// * `ns` - The namespace to compare
/// * `extra` - More arguments for the command
fn diff(ns: &str, extra: &[&str]) -> (Option<i32>, Value) {
    // the report is the whole of stdout
    let output = run(thoradm(ns).args(["schema", "diff", "--json"]).args(extra));
    let report = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("{err}: {}", stderr(&output)));
    (output.status.code(), report)
}

/// The drifted objects of a backend in a report, as `(object, kind)`
///
/// # Arguments
///
/// * `report` - The `schema diff --json` report
/// * `backend` - The backend's name
fn objects(report: &Value, backend: &str) -> Vec<(String, String)> {
    // each entry names its object and kind
    report["backends"][backend]["objects"]
        .as_array()
        .expect("the backend lists its objects")
        .iter()
        .map(|entry| {
            (
                entry["object"].as_str().unwrap().to_owned(),
                entry["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// A fresh file under the temporary directory
///
/// # Arguments
///
/// * `name` - What makes the file unique
fn temp_file(name: &str) -> PathBuf {
    // removed if an earlier run left it
    let path = std::env::temp_dir().join(format!("thoradm-{name}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Run `schema status` and parse the report on its `THORIUM-RESULT` line
///
/// # Arguments
///
/// * `ns` - The namespace to read
/// * `extra` - More arguments for the command
fn status(ns: &str, extra: &[&str]) -> Value {
    // the one line the operator picks out of the log
    let output = run(thoradm(ns).args(["schema", "status"]).args(extra));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(RESULT_PREFIX))
        .expect("the status prints a result line");
    serde_json::from_str(line).expect("the result line is JSON")
}

/// On a matching deployment `schema diff` exits 0 and reports every backend `ok`, and the observed
/// deployment fingerprint equals the recorded revision's
#[tokio::test]
#[ignore = "creates and deletes the Elasticsearch indexes the config names, which the CI user cannot do; run by hand against your own Elasticsearch: cargo test -p thoradm --test schema_diff_cli -- --ignored"]
async fn diff_reports_ok_on_a_matching_deployment() {
    // apply the declared schema to a fresh keyspace, beside indexes created as the streamer does
    let _held = ELASTIC.lock().await;
    let ns = namespace("diff_ok");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    manual::create_indexes().await;
    apply(&ns);
    // compare it with the recorded revision
    let (code, report) = diff(&ns, &[]);
    assert_eq!(code, Some(0), "{report}");
    for backend in ["scylla", "elastic", "redis", "s3"] {
        assert_eq!(report["backends"][backend]["status"], "ok", "{backend}");
    }
    assert_eq!(report["observed"], report["expected"]);
    // the text form says the same
    let output = run(thoradm(&ns).args(["schema", "diff"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("scylla"), "{text}");
    assert!(text.contains("ok"), "{text}");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// After a declared table is dropped by hand, `schema diff` reports it `missing` against the
/// recorded revision and exits 7, and the next `schema apply` recreates it and clears the drift
#[tokio::test]
#[ignore = "creates and deletes the Elasticsearch indexes the config names, which the CI user cannot do; run by hand against your own Elasticsearch: cargo test -p thoradm --test schema_diff_cli -- --ignored"]
async fn a_dropped_table_is_missing_until_the_next_apply() {
    // apply, then drop a table by hand
    let _held = ELASTIC.lock().await;
    let ns = namespace("diff_drop");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    manual::create_indexes().await;
    apply(&ns);
    exec(&format!("DROP TABLE {ns}.nodes")).await;
    // the diff names it, in text and in JSON
    let (code, report) = diff(&ns, &[]);
    assert_eq!(code, Some(7), "{report}");
    assert_eq!(report["backends"]["scylla"]["status"], "drift");
    assert_eq!(
        objects(&report, "scylla"),
        [("nodes".to_owned(), "missing".to_owned())]
    );
    let output = run(thoradm(&ns).args(["schema", "diff"]));
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("missing") && text.contains("nodes"), "{text}");
    // the next apply recreates it, and the drift is gone
    apply(&ns);
    let (code, report) = diff(&ns, &[]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["backends"]["scylla"]["status"], "ok");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// An extra table is `unexpected` and exits 7 until it is acknowledged with `--acknowledged`, which
/// clears the exit code and marks it, and an acknowledgement naming another fingerprint lapses
#[tokio::test]
#[ignore = "creates and deletes the Elasticsearch indexes the config names, which the CI user cannot do; run by hand against your own Elasticsearch: cargo test -p thoradm --test schema_diff_cli -- --ignored"]
async fn an_acknowledged_extra_table_exits_zero() {
    // apply, then add a table the revision doesn't know
    let _held = ELASTIC.lock().await;
    let ns = namespace("diff_ack");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    manual::create_indexes().await;
    apply(&ns);
    exec(&format!(
        "CREATE TABLE {ns}.extra (id uuid PRIMARY KEY, note text)"
    ))
    .await;
    // unacknowledged, it drifts, with its observed fingerprint to copy
    let (code, report) = diff(&ns, &[]);
    assert_eq!(code, Some(7), "{report}");
    assert_eq!(
        objects(&report, "scylla"),
        [("extra".to_owned(), "unexpected".to_owned())]
    );
    let fingerprint = report["backends"]["scylla"]["objects"][0]["fingerprint"]
        .as_str()
        .expect("an unexpected object has a fingerprint")
        .to_owned();
    // acknowledged with its fingerprint, it exits 0 and is marked
    let file = temp_file("ack");
    let ack =
        serde_json::json!([{"backend": "scylla", "object": "extra", "fingerprint": fingerprint}]);
    std::fs::write(&file, ack.to_string()).unwrap();
    let path = file.to_str().unwrap();
    let (code, report) = diff(&ns, &["--acknowledged", path]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["backends"]["scylla"]["status"], "ok");
    assert_eq!(
        report["backends"]["scylla"]["objects"][0]["acknowledged"],
        true
    );
    // after the table changes, the acknowledgement lapses
    exec(&format!("ALTER TABLE {ns}.extra ADD more text")).await;
    let (code, report) = diff(&ns, &["--acknowledged", path]);
    assert_eq!(code, Some(7), "{report}");
    assert_eq!(report["backends"]["scylla"]["objects"][0]["lapsed"], true);
    // an acknowledgement file that isn't acknowledgements is a usage error
    std::fs::write(&file, "{}").unwrap();
    let output = run(thoradm(&ns).args(["schema", "diff", "--acknowledged", path]));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let _ = std::fs::remove_file(&file);
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// A table the revision doesn't know survives a later `schema apply`, which converges only the
/// declared objects, and `schema diff` still reports it `unexpected` afterwards
#[tokio::test]
#[ignore = "creates and deletes the Elasticsearch indexes the config names, which the CI user cannot do; run by hand against your own Elasticsearch: cargo test -p thoradm --test schema_diff_cli -- --ignored"]
async fn an_apply_leaves_an_unexpected_table_in_place_and_reported() {
    // apply, then add a table the revision doesn't know
    let _held = ELASTIC.lock().await;
    let ns = namespace("diff_keep");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    manual::create_indexes().await;
    apply(&ns);
    exec(&format!(
        "CREATE TABLE {ns}.extra (id uuid PRIMARY KEY, note text)"
    ))
    .await;
    // another apply converges the declared objects and nothing else
    apply(&ns);
    let kept = session()
        .await
        .query_unpaged(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (ns.to_lowercase(), "extra"),
        )
        .await
        .expect("the tables list")
        .into_rows_result()
        .expect("rows")
        .rows_num();
    assert_eq!(kept, 1, "the apply dropped the table");
    // the diff still reports it
    let (code, report) = diff(&ns, &[]);
    assert_eq!(code, Some(7), "{report}");
    assert_eq!(
        objects(&report, "scylla"),
        [("extra".to_owned(), "unexpected".to_owned())]
    );
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// With 60 extra tables, 10 of them acknowledged (4 without a fingerprint), `schema status` reports
/// the four counts, `unfingerprinted: 4`, the first 20 unacknowledged objects with fingerprints and
/// `more: 30`, never an acknowledged object, and `schema diff` lists all 60
#[tokio::test]
#[ignore = "creates and deletes the Elasticsearch indexes the config names, which the CI user cannot do; run by hand against your own Elasticsearch: cargo test -p thoradm --test schema_diff_cli -- --ignored"]
async fn the_status_summary_is_capped_and_the_diff_lists_everything() {
    // apply, then add 60 tables the revision doesn't know
    let _held = ELASTIC.lock().await;
    let ns = namespace("diff_many");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    manual::create_indexes().await;
    apply(&ns);
    for index in 0..60 {
        exec(&format!(
            "CREATE TABLE {ns}.extra_{index:02} (id uuid PRIMARY KEY)"
        ))
        .await;
    }
    // the diff lists all 60
    let (code, report) = diff(&ns, &[]);
    assert_eq!(code, Some(7));
    let listed = objects(&report, "scylla");
    assert_eq!(listed.len(), 60);
    assert!(listed.iter().all(|(_, kind)| kind == "unexpected"));
    // acknowledge the last 10: four without a fingerprint, six with the observed one
    let acks = report["backends"]["scylla"]["objects"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| {
            entry["object"].as_str().unwrap()[6..]
                .parse::<u32>()
                .unwrap()
                >= 50
        })
        .map(|entry| {
            let at = entry["object"].as_str().unwrap()[6..]
                .parse::<u32>()
                .unwrap();
            let mut ack = serde_json::json!({"backend": "scylla", "object": entry["object"]});
            if at >= 54 {
                ack["fingerprint"] = entry["fingerprint"].clone();
            }
            ack
        })
        .collect::<Vec<_>>();
    assert_eq!(acks.len(), 10);
    let acks = serde_json::to_string(&acks).unwrap();
    // the status carries the capped summary
    let report = status(&ns, &["--acknowledged", &acks]);
    let drift = &report["status"]["drift"];
    assert_eq!(drift["unexpected"], 50, "{drift}");
    assert_eq!(drift["missing"], 0);
    assert_eq!(drift["changed"], 0);
    assert_eq!(drift["acknowledged"], 10);
    assert_eq!(drift["unfingerprinted"], 4);
    assert_eq!(drift["more"], 30);
    let listed = drift["objects"].as_array().unwrap();
    assert_eq!(listed.len(), 20);
    for entry in listed {
        let at = entry["object"].as_str().unwrap()[6..]
            .parse::<u32>()
            .unwrap();
        assert!(at < 50, "an acknowledged object is listed: {entry}");
        assert!(entry["fingerprint"].is_string());
    }
    assert_eq!(drift["backends"]["scylla"], "drift");
    assert_eq!(drift["backends"]["elastic"], "ok");
    // without the probe, the status carries no summary
    let report = status(&ns, &["--no-probe"]);
    assert!(report["status"]["drift"].is_null(), "{report}");
    exec(&format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}
