//! Tests of `thoradm schema print`, `hash`, and `apply` as the operator's schema-apply Job runs
//! them: the exit codes, the termination summary, the record the apply leaves, and the refusals
//! of changed data-shaping settings

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use scylla::client::session::Session;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};
use thorium::Conf;
use thorium::models::schema::{RevisionSnapshot, SchemaObject, render};
use thorium::models::upgrades::latest;
use thorium::utils::helpers::partition;

#[path = "common/base_namespace.rs"]
mod base_namespace;

/// The test config every run starts from; the environment overrides point it at the test cluster
const CONFIG: &str = "../api/tests/thorium-testing.yml";

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

/// A fresh file for the termination summary
///
/// # Arguments
///
/// * `name` - What makes the file unique
fn termination_log(name: &str) -> PathBuf {
    // a file under the temporary directory, removed if an earlier run left it
    let path = std::env::temp_dir().join(format!("thoradm-{name}-{}.log", std::process::id()));
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

/// Drop a test's keyspace with a session from the test config
///
/// # Arguments
///
/// * `keyspace` - The keyspace to drop
async fn drop_keyspace(keyspace: &str) {
    // connect with the same config the command used
    let conf = Conf::new(CONFIG).expect("the test config loads");
    let session = thorium::models::schema::connect(&conf)
        .await
        .expect("Scylla is reachable");
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await
        .expect("the keyspace drops");
}

/// The deployment row's fingerprint and apply key
///
/// # Arguments
///
/// * `keyspace` - The keyspace holding the record
async fn deployment_row(keyspace: &str) -> (Option<String>, Option<String>) {
    // read the row the apply wrote last
    let conf = Conf::new(CONFIG).expect("the test config loads");
    let session = thorium::models::schema::connect(&conf)
        .await
        .expect("Scylla is reachable");
    let rows = session
        .query_unpaged(
            format!(
                "SELECT fingerprint, apply_key FROM {keyspace}.schema_migrations \
                 WHERE kind = 'backend' AND id = 'deployment'"
            ),
            (),
        )
        .await
        .expect("the record reads")
        .into_rows_result()
        .expect("the record has rows");
    rows.single_row::<(Option<String>, Option<String>)>()
        .expect("one deployment row")
}

/// The schema-apply Job's command converges an empty keyspace, exits 0, records the trigger key
/// from the environment, and writes a termination summary naming the deployment fingerprint; a
/// second run does the same without changing the record
#[tokio::test]
async fn the_job_command_applies_and_reports_the_fingerprint() {
    // run the command the operator's Job runs, on a keyspace that doesn't exist
    let ns = namespace("cli_job");
    drop_keyspace(&ns).await;
    let log = termination_log("job");
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--wait-for-scylla", "60"])
        .env(
            "THORIUM_SCHEMA_APPLY_KEY",
            "image=thorium:test;revision=2026-10-v02",
        )
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // the summary is JSON naming the declared deployment fingerprint and the key
    let declared = RevisionSnapshot::declared(latest()).deployment;
    let text = std::fs::read_to_string(&log).expect("the summary was written");
    assert!(text.len() <= 4096, "{} bytes", text.len());
    let summary: Value = serde_json::from_str(&text).expect("the summary is JSON");
    assert_eq!(summary["outcome"], "done");
    assert_eq!(summary["fingerprint"], declared.as_str());
    assert_eq!(
        summary["trigger_key"],
        "image=thorium:test;revision=2026-10-v02"
    );
    // the record holds the same fingerprint and the key
    assert_eq!(
        deployment_row(&ns).await,
        (
            Some(declared.as_str().to_owned()),
            Some("image=thorium:test;revision=2026-10-v02".to_owned())
        )
    );
    // a second run succeeds and changes nothing
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--wait-for-scylla", "60"])
        .env(
            "THORIUM_SCHEMA_APPLY_KEY",
            "image=thorium:test;revision=2026-10-v02",
        )
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(0));
    let summary: Value =
        serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).expect("JSON");
    assert_eq!(summary["created"], 0);
    assert_eq!(summary["added"], 0);
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_file(&log);
}

/// The files password's hash in the trigger key reaches neither the termination summary, the
/// output, nor the record, while the rest of the key does
#[tokio::test]
async fn the_password_hash_stays_out_of_the_summary_and_the_record() {
    // apply with a key that carries the hash
    let ns = namespace("cli_secret");
    drop_keyspace(&ns).await;
    let log = termination_log("secret");
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--wait-for-scylla", "60"])
        .env(
            "THORIUM_SCHEMA_APPLY_KEY",
            "image=thorium:test;password=c0ffee5ecret;revision=2026-10-v02",
        )
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // neither the summary nor the output names the hash
    let text = std::fs::read_to_string(&log).expect("the summary was written");
    for (what, written) in [
        ("summary", text.clone()),
        (
            "stdout",
            String::from_utf8_lossy(&output.stdout).to_string(),
        ),
        (
            "stderr",
            String::from_utf8_lossy(&output.stderr).to_string(),
        ),
    ] {
        assert!(
            !written.contains("c0ffee5ecret"),
            "the {what} holds the hash"
        );
    }
    // the summary and the record keep the rest of the key
    let summary: Value = serde_json::from_str(&text).expect("the summary is JSON");
    let redacted = "image=thorium:test;revision=2026-10-v02";
    assert_eq!(summary["trigger_key"], redacted);
    assert_eq!(deployment_row(&ns).await.1, Some(redacted.to_owned()));
    drop_keyspace(&ns).await;
    let _ = std::fs::remove_file(&log);
}

/// A dry run prints the statements `schema print` prints, executes nothing, and still exits 0
#[tokio::test]
async fn a_dry_run_prints_the_statements() {
    // dry-run against a keyspace that doesn't exist
    let ns = namespace("cli_dry");
    drop_keyspace(&ns).await;
    let output = run(thoradm(&ns).args(["schema", "apply", "--dry-run"]));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // stdout holds every statement, each ending with a semicolon
    let mut conf = Conf::new(CONFIG).expect("the test config loads");
    conf.thorium.namespace.clone_from(&ns);
    let mut expected = render(&conf, &SchemaObject::declared()).join(";\n");
    expected.push_str(";\n");
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    // and `schema print` prints the same text
    let printed = run(thoradm(&ns).args(["schema", "print"]));
    assert_eq!(printed.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&printed.stdout), expected);
}

/// `schema hash` prints the deployment fingerprint `schema apply` records
#[test]
fn hash_prints_the_recorded_fingerprint() {
    // the first line is the deployment's
    let output = run(thoradm("unused").args(["schema", "hash"]));
    assert_eq!(output.status.code(), Some(0));
    let declared = RevisionSnapshot::declared(latest()).deployment;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(
        stdout.lines().next(),
        Some(format!("deployment {}", declared.as_str()).as_str())
    );
}

/// A Scylla that never accepts connections fails with the retryable exit code once the wait runs
/// out, with a termination summary saying so
#[test]
fn an_unreachable_scylla_exits_with_the_retryable_code() {
    // point the command at a port nothing listens on, waiting two seconds
    let log = termination_log("unreachable");
    let output = run(thoradm(&namespace("cli_down"))
        .args(["schema", "apply", "--wait-for-scylla", "2"])
        .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1")
        .env("THORIUM__SCYLLA__SETUP_TIME", "1")
        .env("THORIUM_TERMINATION_LOG", &log));
    assert_eq!(output.status.code(), Some(4));
    let summary: Value =
        serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).expect("JSON");
    assert_eq!(summary["outcome"], "failed_systematic");
    assert_eq!(summary["exit_code"], 4);
    assert_eq!(summary["retryable"], true);
    let _ = std::fs::remove_file(&log);
}

/// A config that can't be loaded exits 2 before connecting
#[test]
fn usage_errors_exit_before_connecting() {
    // a missing config is a usage error
    let output = run(Command::new(env!("CARGO_BIN_EXE_thoradm")).args([
        "--cluster-conf",
        "/nonexistent/thorium.yml",
        "schema",
        "apply",
    ]));
    assert_eq!(output.status.code(), Some(2));
}

/// Connect to Scylla with the test config
async fn session() -> Session {
    // connect with the same config the command uses
    let conf = Conf::new(CONFIG).expect("the test config loads");
    thorium::models::schema::connect(&conf)
        .await
        .expect("Scylla is reachable")
}

/// Run a CQL statement in the test cluster
///
/// # Arguments
///
/// * `cql` - The statement to run
async fn exec(cql: &str) {
    // run it, naming it if it fails
    session()
        .await
        .query_unpaged(cql, ())
        .await
        .unwrap_or_else(|err| panic!("{cql}: {err}"));
}

/// Insert a submission's key columns into `samples_list`, bucketed with a partition size
///
/// # Arguments
///
/// * `keyspace` - The keyspace to write to
/// * `days` - How many days into 2026 the submission was uploaded
/// * `size` - The partition size its bucket is computed with
async fn seed_sample(keyspace: &str, days: i64, size: u16) {
    // only the primary key, which is all the checks read
    let uploaded = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("the start of the year parses")
        .with_timezone(&Utc)
        + chrono::Duration::days(days)
        + chrono::Duration::seconds(days * 977);
    let bucket = partition(uploaded, uploaded.year(), size);
    exec(&format!(
        "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id) \
         VALUES ('g', {}, {bucket}, '{}', uuid())",
        uploaded.year(),
        uploaded.to_rfc3339()
    ))
    .await;
}

/// One recorded data-shaping value
///
/// # Arguments
///
/// * `keyspace` - The keyspace holding the record
/// * `id` - The setting's record id
async fn config_value(keyspace: &str, id: &str) -> Option<String> {
    // read the setting's config row
    let rows = session()
        .await
        .query_unpaged(
            format!(
                "SELECT value FROM {keyspace}.schema_migrations WHERE kind = 'config' AND id = ?"
            ),
            (id,),
        )
        .await
        .expect("the record reads")
        .into_rows_result()
        .expect("the record has rows");
    rows.maybe_first_row::<(Option<String>,)>()
        .expect("the row reads")
        .and_then(|(value,)| value)
}

/// Run `schema apply` as the Job does, with extra arguments and environment, returning the exit
/// code, the output, and the termination summary
///
/// # Arguments
///
/// * `ns` - The namespace to apply to
/// * `extra` - Arguments after `schema apply --wait-for-scylla 60`
/// * `env` - Environment overrides
fn apply_job(ns: &str, extra: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, Value) {
    // run with a termination log of its own
    let log = termination_log(&format!("{ns}-{}", extra.join("-")));
    let mut command = thoradm(ns);
    command
        .args(["schema", "apply", "--wait-for-scylla", "60"])
        .args(extra)
        .env("THORIUM_TERMINATION_LOG", &log);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = run(&mut command);
    let text = std::fs::read_to_string(&log).expect("the summary was written");
    let _ = std::fs::remove_file(&log);
    assert!(text.len() <= 4096, "{} bytes", text.len());
    let summary = serde_json::from_str(&text).expect("the summary is JSON");
    let printed = format!(
        "{}{}{text}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), printed, summary)
}

/// With stored files, a changed partition size exits 1 with a summary naming the refused setting,
/// which the operator plans from
#[tokio::test]
async fn a_changed_setting_exits_1_naming_it_in_the_summary() {
    // apply, then store a file at the recorded size
    let ns = namespace("cli_refused");
    drop_keyspace(&ns).await;
    let conf = Conf::new(CONFIG).expect("the test config loads");
    let (code, printed, _) = apply_job(&ns, &[], &[]);
    assert_eq!(code, Some(0), "{printed}");
    seed_sample(&ns, 30, conf.thorium.files.partition_size).await;
    // a changed partition size is refused
    let size = (conf.thorium.files.partition_size * 20).to_string();
    let (code, printed, summary) = apply_job(
        &ns,
        &[],
        &[("THORIUM__THORIUM__FILES__PARTITION_SIZE", &size)],
    );
    assert_eq!(code, Some(1), "{printed}");
    assert_eq!(summary["outcome"], "failed_systematic");
    assert_eq!(summary["exit_code"], 1);
    assert_eq!(summary["retryable"], false);
    assert_eq!(summary["refused"], json!(["thorium.files.partition_size"]));
    assert_eq!(summary["sampled"], json!([]));
    drop_keyspace(&ns).await;
}

/// With stored files, a changed files password exits 1 naming `thorium.files.password` and never
/// printing either password, and `--accept-partition-sizes` doesn't let it through
#[tokio::test]
async fn a_changed_files_password_is_refused_without_printing_it() {
    // apply with one password, then store a file
    let ns = namespace("cli_password");
    drop_keyspace(&ns).await;
    let first = "l4c-cli-first-password";
    let second = "l4c-cli-second-password";
    let (code, printed, _) = apply_job(&ns, &[], &[("THORIUM__THORIUM__FILES__PASSWORD", first)]);
    assert_eq!(code, Some(0), "{printed}");
    let conf = Conf::new(CONFIG).expect("the test config loads");
    seed_sample(&ns, 30, conf.thorium.files.partition_size).await;
    let identity = config_value(&ns, "files.password").await;
    // another password is refused, naming only the setting
    let env = [("THORIUM__THORIUM__FILES__PASSWORD", second)];
    let (code, printed, summary) = apply_job(&ns, &[], &env);
    assert_eq!(code, Some(1), "{printed}");
    assert_eq!(summary["refused"], json!(["thorium.files.password"]));
    assert!(printed.contains("thorium.files.password"), "{printed}");
    assert!(
        !printed.contains(first) && !printed.contains(second),
        "{printed}"
    );
    // the partition-size approval doesn't accept it, and the identity is unchanged
    let (code, printed, _) = apply_job(&ns, &["--accept-partition-sizes"], &env);
    assert_eq!(code, Some(1), "{printed}");
    assert_eq!(config_value(&ns, "files.password").await, identity);
    drop_keyspace(&ns).await;
}

/// Before the first data-shaping record, rows at another partition size exit 1 naming the data
/// type as sampled, and `--accept-partition-sizes` records the configured size
#[tokio::test]
async fn sampled_rows_at_another_size_need_accept_partition_sizes() {
    // a keyspace whose record has no data-shaping rows yet, with files written at 3600 seconds
    let ns = namespace("cli_sampled");
    drop_keyspace(&ns).await;
    let (code, printed, _) = apply_job(&ns, &[], &[]);
    assert_eq!(code, Some(0), "{printed}");
    exec(&format!(
        "DELETE FROM {ns}.schema_migrations WHERE kind = 'config'"
    ))
    .await;
    let conf = Conf::new(CONFIG).expect("the test config loads");
    assert_ne!(conf.thorium.files.partition_size, 3600);
    for day in [40, 60, 80] {
        seed_sample(&ns, day, 3600).await;
    }
    // the mismatch is refused, naming the data type, and nothing is recorded
    let (code, printed, summary) = apply_job(&ns, &[], &[]);
    assert_eq!(code, Some(1), "{printed}");
    assert_eq!(summary["sampled"], json!(["files"]));
    assert_eq!(summary["refused"], json!([]));
    assert_eq!(config_value(&ns, "partition_size.files").await, None);
    // once approved, the configured size is recorded
    let (code, printed, _) = apply_job(&ns, &["--accept-partition-sizes"], &[]);
    assert_eq!(code, Some(0), "{printed}");
    assert_eq!(
        config_value(&ns, "partition_size.files").await,
        Some(conf.thorium.files.partition_size.to_string())
    );
    drop_keyspace(&ns).await;
}

/// The warning an apply gives when it records a raised `earliest` value
///
/// # Arguments
///
/// * `data_type` - The data type, such as `files`
/// * `at` - The raised value, in seconds since the epoch
fn raised_warning(data_type: &str, at: i64) -> String {
    // the date the raised value names, to the second
    let date = DateTime::from_timestamp(at, 0)
        .expect("the earliest value is a date")
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    format!(
        "earliest.{data_type} raised: data older than {date} no longer appears in default \
         listings and searches. Nothing is deleted. Lower it again to see it."
    )
}

/// With stored files, a raised `earliest` exits 0 and is recorded, logging a warning and listing
/// it in the summary; a lowered one is recorded without a warning; the default files password's
/// warning is logged too
#[tokio::test]
async fn an_earliest_change_is_recorded_with_a_warning() {
    // apply with the test config's default password, logging its warning
    let ns = namespace("cli_earliest");
    drop_keyspace(&ns).await;
    let conf = Conf::new(CONFIG).expect("the test config loads");
    let output = run(thoradm(&ns).args(["schema", "apply", "--wait-for-scylla", "60"]));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("thorium.files.password is the public default"),
        "{stderr}"
    );
    seed_sample(&ns, 30, conf.thorium.files.partition_size).await;
    // a raised value is recorded, its warning logged and in the summary
    let raised = conf.thorium.files.earliest + 86_400;
    let warning = raised_warning("files", raised);
    let log = termination_log("cli-earliest-raised");
    let output = run(thoradm(&ns)
        .args(["schema", "apply", "--wait-for-scylla", "60"])
        .env("THORIUM__THORIUM__FILES__EARLIEST", raised.to_string())
        .env("THORIUM_TERMINATION_LOG", &log));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains(&warning), "{stderr}");
    let summary: Value =
        serde_json::from_str(&std::fs::read_to_string(&log).expect("the summary was written"))
            .expect("the summary is JSON");
    let _ = std::fs::remove_file(&log);
    assert_eq!(summary["outcome"], "done");
    assert!(
        summary["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings.contains(&json!(warning))),
        "{summary}"
    );
    assert_eq!(
        config_value(&ns, "earliest.files").await,
        Some(raised.to_string())
    );
    // a lowered value is recorded without a warning
    let lowered = (conf.thorium.files.earliest - 86_400).to_string();
    let (code, printed, summary) =
        apply_job(&ns, &[], &[("THORIUM__THORIUM__FILES__EARLIEST", &lowered)]);
    assert_eq!(code, Some(0), "{printed}");
    assert_eq!(summary["outcome"], "done");
    assert!(!printed.contains("raised:"), "{printed}");
    assert_eq!(config_value(&ns, "earliest.files").await, Some(lowered));
    drop_keyspace(&ns).await;
}

/// `--accept-config` is an unknown argument, whatever it names, refused before connecting; the
/// apply's help offers no flag that overrides the files password
#[test]
fn accept_config_is_an_unknown_argument() {
    // every spelling it once took, and the password, against a Scylla nothing listens on
    for setting in ["earliest.files", "thorium.files.earliest", "files.password"] {
        let output = run(thoradm("unused")
            .args(["schema", "apply", "--accept-config", setting])
            .env("THORIUM__SCYLLA_OVERRIDE", "127.0.0.1:1")
            .env("THORIUM__SCYLLA__SETUP_TIME", "1"));
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert_ne!(output.status.code(), Some(0), "{stderr}");
        assert_ne!(output.status.code(), Some(4), "{stderr}");
        assert!(
            stderr.contains("unexpected argument '--accept-config'"),
            "{stderr}"
        );
    }
    // the help names the partition-size approval and nothing about the password
    let output = run(thoradm("unused").args(["schema", "apply", "--help"]));
    let help = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(output.status.code(), Some(0), "{help}");
    assert!(help.contains("--accept-partition-sizes"), "{help}");
    assert!(!help.contains("--accept-config"), "{help}");
    assert!(!help.to_lowercase().contains("password"), "{help}");
}

/// Read the names of a table's columns in the test cluster
///
/// # Arguments
///
/// * `keyspace` - The keyspace the table is in
/// * `table` - The table
async fn column_names(keyspace: &str, table: &str) -> Vec<String> {
    // the columns the schema tables list
    let rows = session()
        .await
        .query_unpaged(
            "SELECT column_name FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, table),
        )
        .await
        .expect("the columns read")
        .into_rows_result()
        .expect("rows");
    let mut names = rows
        .rows::<(String,)>()
        .expect("typed rows")
        .map(|row| row.expect("a row").0)
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// `schema apply --to-revision` converges to an older revision's snapshot: it re-adds the column a
/// later revision dropped, leaves the column a newer revision added in place, and exits 0
#[tokio::test]
async fn converging_to_an_older_revision_re_adds_a_dropped_column_and_keeps_newer_ones() {
    // a keyspace at the declared schema, which then loses a column and gains another
    let ns = namespace("cli_down");
    drop_keyspace(&ns).await;
    let (code, printed, _) = apply_job(&ns, &[], &[]);
    assert_eq!(code, Some(0), "{printed}");
    exec(&format!("ALTER TABLE {ns}.nodes DROP health")).await;
    exec(&format!("ALTER TABLE {ns}.nodes ADD newer_column text")).await;
    assert!(
        !column_names(&ns, "nodes")
            .await
            .contains(&"health".to_owned())
    );
    // converging to the older revision's snapshot adds the one column back
    let (code, printed, summary) = apply_job(&ns, &["--to-revision", "2026-10-v02"], &[]);
    assert_eq!(code, Some(0), "{printed}");
    assert_eq!(summary["added"], 1, "{summary}");
    // nothing the newer schema added was dropped
    let columns = column_names(&ns, "nodes").await;
    assert!(columns.contains(&"health".to_owned()), "{columns:?}");
    assert!(columns.contains(&"newer_column".to_owned()), "{columns:?}");
    // a dry run of a revision with no snapshot is unavailable, and a flag naming none is a usage error
    let (code, printed, _) = apply_job(&ns, &["--to-revision", "2999-01-v01"], &[]);
    assert_eq!(code, Some(3), "{printed}");
    drop_keyspace(&ns).await;
}
