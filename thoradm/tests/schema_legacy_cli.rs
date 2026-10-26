//! Tests of `thoradm schema legacy` and `thoradm schema drop-legacy` as an admin runs them: what the
//! listing and the dry run print, what a confirmed drop does and records, the refusals, and the
//! exit codes

use scylla::client::session::Session;
use serde_json::Value;
use std::process::{Command, Output};
use thorium::Conf;
use thorium::models::schema::support_window;
use thorium::models::upgrades::Catalog;

#[path = "common/base_namespace.rs"]
mod base_namespace;

/// The test config every run starts from; the environment overrides point it at the test cluster
const CONFIG: &str = "../api/tests/thorium-testing.yml";

/// The `events` table an older release's setup created
const LEGACY_EVENTS: &str = "CREATE TABLE IF NOT EXISTS {ns}.events (event_type TEXT, year INT, \
    bucket INT, timestamp TIMESTAMP, id UUID, parent UUID, user TEXT, trigger_depth SMALLINT, \
    data TEXT, PRIMARY KEY ((event_type, year, bucket), timestamp, id)) \
    WITH default_time_to_live = 2592000";

/// The `entities_by_name` view an older release's setup created
const LEGACY_ENTITIES_BY_NAME: &str = "CREATE MATERIALIZED VIEW IF NOT EXISTS \
    {ns}.entities_by_name AS SELECT name, group, kind, created, year, bucket, id, submitter, \
    kind_data, description, image FROM {ns}.entities WHERE name IS NOT NULL AND group IS NOT NULL \
    AND kind IS NOT NULL AND created IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL \
    AND id IS NOT NULL PRIMARY KEY (name, group, kind, created, year, bucket, id)";

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

/// Connect to Scylla with the test config
async fn session() -> Session {
    // the same config the command uses
    let conf = Conf::new(CONFIG).expect("the test config loads");
    thorium::models::schema::connect(&conf)
        .await
        .expect("Scylla is reachable")
}

/// Run a statement, failing the test on error
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The statement
async fn exec(session: &Session, cql: &str) {
    // run it and name the statement on failure
    session
        .query_unpaged(cql, ())
        .await
        .unwrap_or_else(|err| panic!("{cql}: {err}"));
}

/// Count the rows of a table or view
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace
/// * `name` - The table or view
async fn count(session: &Session, keyspace: &str, name: &str) -> i64 {
    // a full count of the object
    session
        .query_unpaged(format!("SELECT COUNT(*) FROM {keyspace}.{name}"), ())
        .await
        .expect("the count runs")
        .into_rows_result()
        .expect("rows")
        .single_row::<(i64,)>()
        .expect("one row")
        .0
}

/// Whether a table or view exists in a keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace
/// * `name` - The table or view
async fn exists(session: &Session, keyspace: &str, name: &str) -> bool {
    // look for it among the tables and then the views
    let tables = session
        .query_unpaged(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, name),
        )
        .await
        .expect("the read runs")
        .into_rows_result()
        .expect("rows")
        .rows_num();
    let views = session
        .query_unpaged(
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?",
            (keyspace, name),
        )
        .await
        .expect("the read runs")
        .into_rows_result()
        .expect("rows")
        .rows_num();
    tables + views > 0
}

/// Build the keyspace of an older release: the declared schema, recorded by `schema apply`, plus
/// both legacy objects holding rows
///
/// # Arguments
///
/// * `ns` - The namespace, and so the keyspace
/// * `events` - How many rows to put in `events`
/// * `entities` - How many rows to put in `entities`
async fn older_release(ns: &str, events: u32, entities: u32) -> Session {
    // start from nothing, apply the declared schema, and add the older release's objects
    let session = session().await;
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    let output = run(thoradm(ns).args(["schema", "apply", "--wait-for-scylla", "60"]));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for cql in [LEGACY_EVENTS, LEGACY_ENTITIES_BY_NAME] {
        exec(&session, &cql.replace("{ns}", ns)).await;
    }
    // rows in the table and in the view's base table
    for _ in 0..events {
        exec(
            &session,
            &format!(
                "INSERT INTO {ns}.events (event_type, year, bucket, timestamp, id, parent, user, \
                 trigger_depth, data) VALUES ('t', 2026, 1, toTimestamp(now()), uuid(), uuid(), \
                 'u', 0, 'd')"
            ),
        )
        .await;
    }
    for index in 0..entities {
        exec(
            &session,
            &format!(
                "INSERT INTO {ns}.entities (kind, group, year, bucket, created, id, name) VALUES \
                 ('device', 'g', 2026, 1, toTimestamp(now()), uuid(), 'entity-{index}')"
            ),
        )
        .await;
    }
    // wait until the view holds a row for each entity
    for _ in 0..240 {
        if count(&session, ns, "entities_by_name").await == i64::from(entities) {
            return session;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("entities_by_name never caught up");
}

/// The text of a command's standard output
///
/// # Arguments
///
/// * `output` - The command's output
fn stdout(output: &Output) -> String {
    // lossy is fine for text the command prints
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The text of a command's standard error
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // lossy is fine for text the command prints
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `schema legacy` on an older release's keyspace lists both objects with rows and the downgrade
/// lines in text, and the same facts as JSON, exiting 0
#[tokio::test]
async fn the_listing_shows_both_objects_as_text_and_json() {
    // two events and three entities
    let ns = namespace("lgc_list");
    let session = older_release(&ns, 2, 3).await;
    let output = run(thoradm(&ns).args(["schema", "legacy"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.starts_with("Legacy objects\n"), "{text}");
    assert!(
        text.contains(
            "  scylla:events  (table, class Runtime)\n    present:     yes\n    rows:        2\n"
        ),
        "{text}"
    );
    assert!(text.contains("  scylla:entities_by_name  (view, class Derived)\n    present:     yes\n    rows:        3\n"), "{text}");
    assert!(
        text.contains("    downgrade to 2026-10-v01: recreated (it does not use this object)\n"),
        "{text}"
    );
    assert!(text.contains("    droppable:   yes\n"), "{text}");
    assert!(
        text.contains("Not registered (cannot be dropped by this command)\n  none\n"),
        "{text}"
    );
    assert!(text.ends_with("no unmanaged extra objects\n"), "{text}");
    // the same facts as JSON
    let output = run(thoradm(&ns).args(["schema", "legacy", "--json"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(json["objects"][0]["id"], "scylla:events");
    assert_eq!(json["objects"][0]["rows"], 2);
    assert_eq!(json["objects"][1]["id"], "scylla:entities_by_name");
    assert_eq!(json["objects"][1]["rows"], 3);
    // one downgrade line for each revision of the production catalog's support window
    let window = support_window(&Catalog::PRODUCTION);
    assert_eq!(
        json["objects"][0]["downgrades"].as_array().map(Vec::len),
        Some(window.len())
    );
    // a keyspace this release created lists both as not present
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ns}")).await;
    let output = run(thoradm(&ns).args(["schema", "apply", "--wait-for-scylla", "60"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = run(thoradm(&ns).args(["schema", "legacy"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("    present:     no\n    rows:        0\n"),
        "{}",
        stdout(&output)
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// Without `--execute` the command prints the plan and each downgrade consequence, drops nothing,
/// writes no audit row, and exits 0
#[tokio::test]
async fn a_dry_run_prints_the_plan_and_drops_nothing() {
    // an older release's keyspace
    let ns = namespace("lgc_dry");
    let session = older_release(&ns, 2, 1).await;
    let recorded = count(&session, &ns, "schema_migrations").await;
    let output = run(thoradm(&ns).args(["schema", "drop-legacy", "--object", "scylla:events"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("Plan (dry run: nothing is dropped; pass --execute to drop)\n"),
        "{text}"
    );
    assert!(
        text.contains("  scylla:events  (table, 2 rows)\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "    statement: DROP TABLE IF EXISTS {}.events\n",
            ns.to_lowercase()
        )),
        "{text}"
    );
    assert!(
        text.contains("    downgrade to 2026-10-v02: recreated (it does not use this object)\n"),
        "{text}"
    );
    // nothing was dropped or recorded
    assert!(exists(&session, &ns.to_lowercase(), "events").await);
    assert_eq!(count(&session, &ns, "events").await, 2);
    assert_eq!(count(&session, &ns, "schema_migrations").await, recorded);
    assert_eq!(legacy_drops(&session, &ns).await, 0);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// Count the audit rows of legacy drops
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `ns` - The keyspace
async fn legacy_drops(session: &Session, ns: &str) -> usize {
    // the legacy-drop partition
    session
        .query_unpaged(
            format!("SELECT id FROM {ns}.schema_migrations WHERE kind = 'legacy_drop'"),
            (),
        )
        .await
        .expect("the read runs")
        .into_rows_result()
        .expect("rows")
        .rows_num()
}

/// Without a terminal, `--execute` needs `--confirm`: it refuses with exit 1 and drops nothing; with
/// `--confirm` it drops the named objects and records one audit row each with who and the rows
#[tokio::test]
async fn execute_needs_confirm_without_a_terminal_and_records_each_drop() {
    // an older release's keyspace with rows in both objects
    let ns = namespace("lgc_exec");
    let session = older_release(&ns, 3, 2).await;
    let both = [
        "schema",
        "drop-legacy",
        "--object",
        "scylla:events",
        "--object",
        "scylla:entities_by_name",
        "--execute",
    ];
    let output = run(thoradm(&ns).args(both).stdin(std::process::Stdio::null()));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("--confirm"), "{}", stderr(&output));
    assert!(exists(&session, &ns.to_lowercase(), "events").await);
    assert_eq!(legacy_drops(&session, &ns).await, 0);
    // with --confirm both are dropped and recorded
    let output = run(thoradm(&ns)
        .args(both)
        .arg("--confirm")
        .env("USER", "tester"));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("dropped scylla:events (3 rows)"),
        "{}",
        stdout(&output)
    );
    assert!(!exists(&session, &ns.to_lowercase(), "events").await);
    assert!(!exists(&session, &ns.to_lowercase(), "entities_by_name").await);
    // the declared base table of the view is untouched
    assert_eq!(count(&session, &ns, "entities").await, 2);
    // one audit row per object: its id after the time, the rows, and who
    let rows = session
        .query_unpaged(
            format!(
                "SELECT id, value, lease FROM {ns}.schema_migrations WHERE kind = 'legacy_drop'"
            ),
            (),
        )
        .await
        .expect("the read runs")
        .into_rows_result()
        .expect("rows");
    let found = rows
        .rows::<(String, Option<String>, Option<String>)>()
        .expect("typed rows")
        .map(|row| row.expect("a row"))
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 2, "{found:?}");
    let mut recorded = found
        .iter()
        .map(|(id, value, lease)| {
            let object = id
                .split_once('/')
                .expect("id is `<time>/<object>`")
                .1
                .to_owned();
            (
                object,
                value.clone().unwrap_or_default(),
                lease.clone().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    recorded.sort();
    assert_eq!(recorded[0].0, "scylla:entities_by_name");
    assert_eq!(recorded[0].1, "2");
    assert_eq!(recorded[1].0, "scylla:events");
    assert_eq!(recorded[1].1, "3");
    assert!(
        recorded.iter().all(|(.., who)| who.starts_with("tester@")),
        "{recorded:?}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// A declared object, an id nothing registers, and a list holding either with a registered object
/// each exit 1 with the message naming it, and nothing in the list is dropped
#[tokio::test]
async fn refusals_exit_1_and_drop_nothing() {
    // an older release's keyspace
    let ns = namespace("lgc_refuse");
    let session = older_release(&ns, 1, 1).await;
    let refused = |objects: &[&str]| {
        let mut command = thoradm(&ns);
        command.args(["schema", "drop-legacy", "--execute", "--confirm"]);
        for object in objects {
            command.args(["--object", object]);
        }
        run(&mut command)
    };
    // a declared object
    let output = refused(&["scylla:samples_list"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("scylla:samples_list is a declared object; refusing."),
        "{}",
        stderr(&output)
    );
    // an id nothing registers lists the registered ones
    let output = refused(&["scylla:tools"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(
            "scylla:tools is not a registered legacy object. Registered: scylla:events, scylla:entities_by_name."
        ),
        "{}",
        stderr(&output)
    );
    // a registered object beside a refused one is not dropped either
    let output = refused(&["scylla:events", "scylla:samples_list"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(exists(&session, &ns.to_lowercase(), "events").await);
    assert_eq!(count(&session, &ns, "events").await, 1);
    assert_eq!(legacy_drops(&session, &ns).await, 0);
    // naming no object is a usage error
    let output = run(thoradm(&ns).args(["schema", "drop-legacy"]));
    assert_ne!(output.status.code(), Some(0));
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ns}")).await;
}

/// A config that can't be loaded exits 2, an unreachable Scylla exits 3, for both commands
#[test]
fn config_and_availability_errors_exit_2_and_3() {
    // a missing config is a usage error
    for command in [
        vec!["schema", "legacy"],
        vec!["schema", "drop-legacy", "--object", "scylla:events"],
    ] {
        let output = run(Command::new(env!("CARGO_BIN_EXE_thoradm"))
            .args(["--cluster-conf", "/nonexistent/thorium.yml"])
            .args(&command));
        assert_eq!(
            output.status.code(),
            Some(2),
            "{command:?}: {}",
            stderr(&output)
        );
        // Scylla that never answers is unavailable
        let output = run(thoradm("unused")
            .args(&command)
            .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1")
            .env("THORIUM__SCYLLA__SETUP_TIME", "1"));
        assert_eq!(
            output.status.code(),
            Some(3),
            "{command:?}: {}",
            stderr(&output)
        );
    }
}

/// `--yes` is not a `drop-legacy` flag: it is rejected as unknown and nothing is dropped, and the
/// help names `--confirm`
#[test]
fn yes_is_rejected_and_the_help_names_confirm() {
    // an unknown flag fails before the config is read
    let output = run(thoradm("unused").args([
        "schema",
        "drop-legacy",
        "--object",
        "scylla:events",
        "--execute",
        "--yes",
    ]));
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("--yes"), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unexpected argument"),
        "{}",
        stderr(&output)
    );
    // the help lists --confirm and not --yes
    let output = run(thoradm("unused").args(["schema", "drop-legacy", "--help"]));
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(help.contains("--confirm"), "{help}");
    assert!(!help.contains("--yes"), "{help}");
}
