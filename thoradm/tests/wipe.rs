//! `backup restore --wipe <namespace>`: the namespace named and the API's namespace are checked
//! first, the wipe's guards refuse targets it can't safely delete from, the wipe is confirmed by
//! `--yes` or the typed namespace, and it replaces the target's backed-up data with the backup,
//! keeping excluded data and resuming after a failure partway
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use axum::http::Method;
use common::corrupt;
use common::cycle::{describe, wipe};
use common::fixture;
use common::harness::{Harness, bucket_for, empty_bucket, list_keys, write_cluster_conf};
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::names::Names;
use common::seed;
use common::state::{TargetState, target_state};
use futures::FutureExt;
use serde_json::Value;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use strum::IntoEnumIterator;
use thoradm::args::BackupComponents;
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, Operation};
use thoradm::backup::manifest::Manifest;
use thoradm::backup::target::Target;
use thoradm::backup::wipe;
use thorium::models::{FileDeleteOpts, SystemInfo};

/// Empties the target and initializes the system again, as a fresh install has it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn empty_target(harness: &Harness) {
    // wipe everything but the admin's login, then put back what a fresh install has
    wipe(harness).await;
    harness
        .client()
        .await
        .system
        .init()
        .await
        .expect("failed to initialize the system");
}

/// Empties the target, seeds the clean fixture, and backs everything up, returning the backup and
/// what was seeded; the target keeps the seeded data
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
async fn seeded_backup(harness: &Harness, name: &str) -> (PathBuf, fixture::Clean) {
    // one of every resource whose Redis state restore brings back
    empty_target(harness).await;
    let seeded = fixture::clean(harness).await;
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "4", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    (out, seeded)
}

/// Runs `backup restore` without a terminal
///
/// # Arguments
///
/// * `command` - The thoradm command to run, with its configs
/// * `backup` - The backup to restore
/// * `extra` - Extra `backup restore` arguments
fn restore_with(mut command: Command, backup: &Path, extra: &[&str]) -> Output {
    // stdin detached, so no prompt can be answered
    command
        .args(["--workers", "2", "backup", "restore", "-b"])
        .arg(backup)
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm")
}

/// Returns every record the restores of a backup wrote in their default report directories
///
/// # Arguments
///
/// * `backup` - The backup
fn records(backup: &Path) -> Vec<FailureRecord> {
    // every restore report under the backup
    common::cycle::failure_records(backup)
}

/// Returns the preflight refusal among a backup's restore records, by the check it names
///
/// # Arguments
///
/// * `backup` - The backup
/// * `check` - The preflight check
fn refusal(backup: &Path, check: &str) -> Option<FailureRecord> {
    // the restore's own preflight records name their check as their object
    records(backup).into_iter().find(|record| {
        record.operation == Operation::Preflight && record.object.as_deref() == Some(check)
    })
}

/// Returns every `wipe.json` the restores of a backup wrote, with its report directory
///
/// # Arguments
///
/// * `backup` - The backup
fn wipe_reports(backup: &Path) -> Vec<(PathBuf, Value)> {
    // each restore report directory may hold one
    let mut found = Vec::new();
    if let Ok(listing) = std::fs::read_dir(backup.join("reports")) {
        for entry in listing.filter_map(Result::ok) {
            let path = entry.path().join("wipe.json");
            if let Ok(bytes) = std::fs::read(&path) {
                found.push((entry.path(), serde_json::from_slice(&bytes).unwrap()));
            }
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

/// Returns whether any `wipe.json` of a backup records a deletion
///
/// # Arguments
///
/// * `backup` - The backup
fn any_deletion(backup: &Path) -> bool {
    // a truncated table or a deleted object
    wipe_reports(backup).iter().any(|(_, report)| {
        report["tables"]
            .as_array()
            .is_some_and(|tables| tables.iter().any(|table| table["truncated"] == true))
            || report["buckets"].as_array().is_some_and(|buckets| {
                buckets
                    .iter()
                    .any(|bucket| bucket["deleted_objects"].as_u64() > Some(0))
            })
    })
}

/// Runs `backup verify --cluster` against the namespace and returns its exit code and stderr
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to check the cluster against
fn verify_cluster(harness: &Harness, backup: &Path) -> Output {
    // the report goes beside the backup
    harness
        .thoradm()
        .args(["backup", "verify", "--cluster", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(
            backup
                .parent()
                .unwrap()
                .join(format!("cluster-{}", uuid::Uuid::new_v4())),
        )
        .output()
        .expect("failed to run thoradm")
}

/// Returns the sha256s the samples list holds
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn listed_samples(harness: &Harness) -> Vec<String> {
    // every row of the samples list
    let scylla = harness.scylla().await;
    let mut found: Vec<String> = scylla
        .query_unpaged(
            format!(
                "SELECT sha256 FROM {}.samples_list",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Counts the rows of an excluded table
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `table` - The table
async fn rows_of(harness: &Harness, table: &str) -> usize {
    // count them all; the test tables are small
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!("SELECT * FROM {}.{table}", harness.conf.thorium.namespace),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows_num()
}

/// A wipe restore over a target that gained a sample, lost one, and holds excluded data leaves the
/// backup's state exactly, keeps the excluded data, and records what it wiped
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_wipe_replaces_the_target_with_the_backup() {
    // a backup holding sample D
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let client = harness.client().await;
    let names = Names::new();
    let (backup, seeded) = seeded_backup(harness, "replace").await;
    let d = seeded.sample.clone();
    let ns = harness.conf.thorium.namespace.clone();
    // the files bucket as the backup holds it
    let files = bucket_for(&harness.conf, "files");
    let backed_up_files = list_keys(&harness.s3(), &files).await.unwrap();
    // after it, sample B is added and every submission of D deleted
    let b = seed::sample(&client, &seeded.group, "after.bin", names.suffix.as_bytes()).await;
    let sample = client.files.get(&d).await.unwrap();
    for submission in &sample.submissions {
        let opts = FileDeleteOpts::default().groups(submission.groups.clone());
        client
            .files
            .delete(&d, &submission.id, &opts)
            .await
            .unwrap();
    }
    let samples = listed_samples(harness).await;
    assert!(samples.contains(&b) && !samples.contains(&d), "{samples:?}");
    let scylla = harness.scylla().await;
    // excluded data: a stage log line, an event, and an ephemeral object, none of which expire
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.logs (reaction, stage, bucket, position, line) \
                 VALUES (?, 'kept', 0, 0, 'a line the wipe keeps') USING TTL 0"
            ),
            (uuid::Uuid::new_v4(),),
        )
        .await
        .unwrap();
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.events (event_type, year, bucket, timestamp, id, data) \
                 VALUES ('kept', 2026, 0, toTimestamp(now()), ?, '{{}}') USING TTL 0"
            ),
            (uuid::Uuid::new_v4(),),
        )
        .await
        .unwrap();
    let ephemeral = bucket_for(&harness.conf, "ephemeral");
    common::objects::put(
        &harness.s3(),
        &ephemeral,
        "kept/scratch.bin",
        &common::columns::Body::Bytes(b"scratch the wipe keeps".to_vec()),
    )
    .await;
    let (logs, events) = (
        rows_of(harness, "logs").await,
        rows_of(harness, "events").await,
    );
    // the wipe restore runs without a terminal under --yes
    let restored = restore_with(harness.thoradm(), &backup, &["--wipe", &ns, "--yes"]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr);
    // the plan never calls a table that holds rows empty, whether or not Scylla has estimated it
    let planned = stderr
        .lines()
        .find(|line| line.contains(&format!("table {ns}.samples_list: ")))
        .unwrap_or_else(|| panic!("the plan names no samples_list table:\n{stderr}"));
    assert!(
        !planned.contains("about 0 partitions") && !planned.contains("empty"),
        "{planned}"
    );
    // the cluster equals the backup: B is gone and D is back
    let verified = verify_cluster(harness, &backup);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
    let samples = listed_samples(harness).await;
    assert!(samples.contains(&d) && !samples.contains(&b), "{samples:?}");
    // the excluded data is untouched
    assert_eq!(rows_of(harness, "logs").await, logs);
    assert_eq!(rows_of(harness, "events").await, events);
    let kept = list_keys(&harness.s3(), &ephemeral).await.unwrap();
    // the files bucket holds what the backup holds: B's object is gone and D's is back
    assert_eq!(
        list_keys(&harness.s3(), &files).await.unwrap(),
        backed_up_files
    );
    assert!(kept.contains(&"kept/scratch.bin".to_owned()), "{kept:?}");
    // wipe.json and the summary say what was wiped, and each table and bucket was logged
    let reports = wipe_reports(&backup);
    let (_, report) = reports.last().expect("no wipe.json");
    assert_eq!(report["namespace"], ns.as_str(), "{report:#}");
    assert_eq!(report["api_namespace"], ns.as_str(), "{report:#}");
    assert!(report["finished"].is_string(), "{report:#}");
    let tables = report["tables"].as_array().unwrap();
    assert!(
        tables.iter().all(|table| table["truncated"] == true),
        "{report:#}"
    );
    let wiped_files = report["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bucket| bucket["bucket"] == files.as_str())
        .unwrap_or_else(|| panic!("the files bucket isn't in the plan: {report:#}"));
    assert!(
        wiped_files["deleted_objects"].as_u64() > Some(0),
        "{report:#}"
    );
    assert!(wiped_files["done"] == true, "{report:#}");
    assert!(
        report["kept"]
            .as_array()
            .unwrap()
            .iter()
            .any(|kept| kept == "logs"),
        "{report:#}"
    );
    for table in tables {
        let name = table["table"].as_str().unwrap();
        assert!(
            stderr
                .lines()
                .any(|line| line.contains("WARN")
                    && line.contains(&format!("Wiped table {ns}.{name}"))),
            "no WARN for {name}:\n{stderr}"
        );
    }
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("WARN") && line.contains(&format!("Wiped bucket {files}"))),
        "{stderr}"
    );
    assert!(stderr.contains("restore summary: wiped"), "{stderr}");
    let manifest = Manifest::read(&backup).await.unwrap().unwrap();
    assert!(manifest.tables.contains_key("samples_list"));
}

/// `--merge` and `--wipe` exclude each other and `--wipe` needs its namespace: each is a usage
/// error that exits 1 before thoradm reads a config
#[test]
fn wipe_flag_usage_errors_exit_1() {
    // a config path that doesn't exist, which thoradm would fail on if it got that far
    let missing = std::env::temp_dir().join("no-such-thoradm-config.yml");
    for args in [
        vec![
            "backup",
            "restore",
            "-b",
            "x",
            "--merge",
            "--wipe",
            "testing_x",
        ],
        vec!["backup", "restore", "-b", "x", "--wipe"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_thoradm"))
            .arg("--cluster-conf")
            .arg(&missing)
            .args(&args)
            .output()
            .expect("failed to run thoradm");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains("--wipe"), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("no-such-thoradm-config"),
            "{args:?}: {stderr}"
        );
    }
}

/// Runs a restore on a pseudo-terminal, typing a line into its prompt
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to restore
/// * `extra` - Extra `backup restore` arguments
/// * `typed` - The line typed at the prompt
fn restore_on_a_terminal(harness: &Harness, backup: &Path, extra: &[&str], typed: &str) -> Output {
    // script gives thoradm a terminal and forwards our stdin to it
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut line = vec![
        quote(env!("CARGO_BIN_EXE_thoradm")),
        "--cluster-conf".to_owned(),
        quote(&harness.cluster_conf.to_string_lossy()),
        "--ctl-conf".to_owned(),
        quote(&harness.ctl_conf.to_string_lossy()),
        "--workers".to_owned(),
        "2".to_owned(),
        "backup".to_owned(),
        "restore".to_owned(),
        "-b".to_owned(),
        quote(&backup.to_string_lossy()),
    ];
    line.extend(extra.iter().map(|arg| quote(arg)));
    let mut command = Command::new("script");
    for (key, _) in std::env::vars() {
        if key.starts_with("THORIUM__") || key.starts_with("THORCTL__") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args(["-qec", &line.join(" "), "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run script");
    // give the prompt time to appear before typing into it
    std::thread::sleep(std::time::Duration::from_secs(5));
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(format!("{typed}\r").as_bytes()).unwrap();
    stdin.flush().unwrap();
    let output = child.wait_with_output().expect("script failed");
    drop(stdin);
    output
}

/// On a target that holds data, `--wipe` without a terminal needs `--yes`; on a terminal the
/// namespace must be typed, even with `--yes`: another name aborts and the namespace proceeds;
/// neither refusal changes the target
#[tokio::test]
async fn a_wipe_needs_yes_or_the_typed_namespace() {
    // a backup, and a target that still holds its data
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "confirm").await;
    let ns = harness.conf.thorium.namespace.clone();
    let before = target_state(harness).await;
    // without a terminal and without --yes, the wipe is refused, naming both flags
    let refused = restore_with(harness.thoradm(), &backup, &["--wipe", &ns]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let record = refusal(&backup, "confirmation").expect("no confirmation refusal");
    assert!(
        record.error.contains("--yes") && record.error.contains("--wipe"),
        "{record:?}"
    );
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&backup));
    // on a terminal, typing another name aborts, even with --yes
    let typed = restore_on_a_terminal(harness, &backup, &["--wipe", &ns, "--yes"], "testing_wrong");
    assert_eq!(typed.status.code(), Some(1), "{}", describe(&typed));
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&backup));
    // typing the namespace proceeds
    let typed = restore_on_a_terminal(harness, &backup, &["--wipe", &ns], &ns);
    assert_eq!(typed.status.code(), Some(0), "{}", describe(&typed));
    assert!(any_deletion(&backup));
}

/// A wipe whose object deletes keep failing stops the restore with exit 1 after truncating the
/// tables, before Redis, recording the bucket; a plain re-run is refused; once the fault clears,
/// a `--wipe` re-run resumes the earlier wipe, recreates the views it dropped, and the cluster
/// matches the backup
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_wipe_that_fails_partway_stops_and_resumes() {
    // a backup, a target holding its data, and S3 refusing every batch delete
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "partway").await;
    let ns = harness.conf.thorium.namespace.clone();
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    mock.set_rules(vec![S3Rule {
        method: Method::POST,
        key: "*".to_owned(),
        query: Some("delete".to_owned()),
        nth: None,
        behavior: S3Behavior::Status {
            status: 500,
            code: "InternalError".to_owned(),
        },
    }])
    .await;
    // a cluster config that sends thoradm's S3 requests through the mock
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let cluster_conf = backup.parent().unwrap().join("mock.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    // the Redis keys before, which a stopped wipe leaves alone
    let redis_before = target_state(harness).await.redis_keys;
    // the wipe stops at the first bucket, after every table was truncated
    let mut command = harness.thoradm_with(&cluster_conf);
    command.args(["--workers", "2", "backup", "restore", "-b"]);
    let run = tokio::process::Command::from(command)
        .arg(&backup)
        .args(["--wipe", &ns, "--yes", "--retries", "2"])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("failed to run thoradm");
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    // a wipe record names the bucket
    let wiped = records(&backup)
        .into_iter()
        .find(|record| record.operation == Operation::Wipe)
        .unwrap_or_else(|| panic!("no wipe record: {}", describe(&run)));
    assert!(wiped.bucket.is_some(), "{wiped:?}");
    // wipe.json has no finished time, and every table truncated
    let reports = wipe_reports(&backup);
    let (earlier, report) = reports.last().expect("no wipe.json").clone();
    assert!(
        report.get("finished").is_none_or(Value::is_null),
        "{report:#}"
    );
    assert!(
        report["tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|table| table["truncated"] == true),
        "{report:#}"
    );
    // nothing was restored: the tables are empty and Redis is as it was
    let state = target_state(harness).await;
    assert!(state.rows.values().all(|rows| *rows == 0), "{state:?}");
    assert_eq!(state.redis_keys, redis_before);
    // a plain re-run is refused, since the buckets still hold objects
    let refused = restore_with(harness.thoradm(), &backup, &["--yes"]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    assert!(refusal(&backup, "empty-target").is_some());
    // once the fault clears, a --wipe re-run resumes the earlier wipe and finishes the restore
    mock.set_rules(Vec::new()).await;
    let resumed = restore_with(harness.thoradm(), &backup, &["--wipe", &ns, "--yes"]);
    assert_eq!(resumed.status.code(), Some(0), "{}", describe(&resumed));
    // it names the earlier wipe and records it as resumed_from
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains(&earlier.display().to_string()), "{stderr}");
    let reports = wipe_reports(&backup);
    let (_, report) = reports.last().unwrap();
    assert_eq!(
        report["resumed_from"],
        earlier.display().to_string(),
        "{report:#}"
    );
    assert!(report["finished"].is_string(), "{report:#}");
    // the cluster matches the backup
    let verified = verify_cluster(harness, &backup);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
}

/// With `--wipe <namespace> --yes` on a target that holds data, a damaged backup and a backup
/// with a recorded failure each stop at their own check, and nothing is wiped
#[tokio::test]
async fn a_wipe_never_runs_for_an_unusable_backup() {
    // a backup, and a target that still holds its data
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "unusable").await;
    let ns = harness.conf.thorium.namespace.clone();
    let before: TargetState = target_state(harness).await;
    // one archive byte flipped: verify refuses
    let flipped = backup.parent().unwrap().join("flipped");
    copy_tree(&backup, &flipped);
    // the first byte of a samples_list archive that holds rows
    let manifest = Manifest::read(&flipped).await.unwrap().unwrap();
    let archive = manifest.tables["samples_list"]
        .archives
        .iter()
        .find(|archive| archive.entries > 0)
        .unwrap();
    corrupt::flip_byte(
        &flipped
            .join("samples_list")
            .join("data")
            .join(archive.uuid.to_string()),
        0,
    )
    .unwrap();
    // the wipe restore is refused by verify, and nothing was deleted
    let run = restore_with(harness.thoradm(), &flipped, &["--wipe", &ns, "--yes"]);
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    assert!(refusal(&flipped, "verify").is_some(), "{}", describe(&run));
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&flipped));
    // a recorded failure, without --accept-failures: its check refuses
    let recorded = backup.parent().unwrap().join("recorded");
    copy_tree(&backup, &recorded);
    let record = FailureRecord::failed("result-files", Operation::Download, "gone")
        .bucket("testing-results")
        .key("a-result/out.bin");
    // written as the backup's failures file, which the manifest counts and hashes
    let failures = recorded.join(FAILURES_FILE);
    std::fs::write(
        &failures,
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
    let sha = thoradm::backup::manifest::file_sha256(&failures)
        .await
        .unwrap();
    corrupt::edit_json(&recorded.join("manifest.json"), "/failures", Value::from(1)).unwrap();
    corrupt::edit_json(
        &recorded.join("manifest.json"),
        "/failures_sha256",
        Value::from(sha),
    )
    .unwrap();
    // the wipe restore is refused by that check, and nothing was deleted
    let run = restore_with(harness.thoradm(), &recorded, &["--wipe", &ns, "--yes"]);
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    assert!(
        refusal(&recorded, "recorded-failures").is_some(),
        "{}",
        describe(&run)
    );
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&recorded));
}

/// Copies a directory tree, leaving out the reports earlier runs wrote
///
/// # Arguments
///
/// * `from` - The directory to copy
/// * `to` - Where to put the copy
fn copy_tree(from: &Path, to: &Path) {
    // copy every file and directory below the root but its reports
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        if entry.file_name() == "reports" {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Creates a keyspace holding a backed-up table, which the wipe counts as a Thorium namespace
///
/// # Arguments
///
/// * `scylla` - A session to the cluster
/// * `keyspace` - The keyspace
async fn thorium_keyspace(scylla: &scylla::client::session::Session, keyspace: &str) {
    // the keyspace, then one table a backup holds
    scylla
        .query_unpaged(
            format!(
                "CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH replication = \
                 {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
            ),
            &[],
        )
        .await
        .unwrap();
    scylla
        .query_unpaged(
            format!("CREATE TABLE IF NOT EXISTS {keyspace}.samples_list (id UUID PRIMARY KEY)"),
            &[],
        )
        .await
        .unwrap();
}

/// The wipe's guards refuse a namespace that is a system keyspace or holds no backed-up table,
/// two roles sharing a bucket, and, while another Thorium keyspace exists, a default bucket name,
/// one without the namespace in it, or one that another namespace (a system-prefixed one too)
/// names at least as fully; buckets named for the namespace pass
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_wipe_guards_refuse_targets_it_cant_safely_wipe() {
    // the keyspaces the checks create beside this namespace's
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = harness.conf.thorium.namespace.clone();
    let other = format!("{ns}_neighbor");
    let bare = format!("{ns}_bare");
    let extended = format!("{ns}_dev");
    let tied = ns.replacen("testing_", "staging_", 1);
    let reserved = format!("system_{ns}");
    // run the checks, catching a failed one so the keyspaces are dropped either way
    let checked = AssertUnwindSafe(Box::pin(async {
        // another Thorium keyspace beside this one, and a keyspace holding no Thorium table
        for keyspace in [&other, &bare] {
            scylla
                .query_unpaged(
                    format!(
                        "CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH replication = \
                         {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
                    ),
                    &[],
                )
                .await
                .unwrap();
        }
        scylla
            .query_unpaged(
                format!("CREATE TABLE IF NOT EXISTS {other}.samples_list (id UUID PRIMARY KEY)"),
                &[],
            )
            .await
            .unwrap();
        let all: Vec<BackupComponents> = BackupComponents::iter()
            .filter(|component| !component.is_filter())
            .collect();
        let guard = |conf: thorium::Conf| {
            let scylla = scylla.clone();
            let all = all.clone();
            async move {
                let target = Target::new(&conf, &all);
                wipe::guards(&conf, &scylla, &target).await
            }
        };
        // the namespace's own buckets pass while another Thorium keyspace exists
        assert_eq!(guard(harness.conf.clone()).await, Ok(()));
        // two roles share a bucket
        let mut shared = harness.conf.clone();
        shared
            .thorium
            .ephemeral
            .bucket
            .clone_from(&shared.thorium.files.bucket);
        let error = guard(shared).await.expect_err("a shared bucket passed");
        assert!(
            error.contains("ephemeral") && error.contains("files"),
            "{error}"
        );
        // a default bucket name, and a name without the namespace, while another keyspace exists
        let mut default = harness.conf.clone();
        "thorium-files".clone_into(&mut default.thorium.files.bucket);
        let error = guard(default).await.expect_err("a default bucket passed");
        assert!(error.contains("thorium-files"), "{error}");
        let mut unnamed = harness.conf.clone();
        "testing-elsewhere-files".clone_into(&mut unnamed.thorium.files.bucket);
        let error = guard(unnamed).await.expect_err("an unnamed bucket passed");
        assert!(error.contains("testing-elsewhere-files"), "{error}");
        // a bucket named for a neighbor whose name extends this namespace's, or for this
        // namespace and a neighbor equally, may be the neighbor's
        let dashed = |keyspace: &str| keyspace.replace('_', "-");
        for keyspace in [&extended, &tied, &reserved] {
            thorium_keyspace(&scylla, keyspace).await;
        }
        for bucket in [
            format!("{}-files", dashed(&extended)),
            format!("{}-{}-files", dashed(&ns), dashed(&tied)),
            format!("{}-files", dashed(&reserved)),
        ] {
            let mut neighbors = harness.conf.clone();
            neighbors.thorium.files.bucket.clone_from(&bucket);
            let error = guard(neighbors).await.expect_err(&format!(
                "{bucket}, which a neighbor names as fully, passed"
            ));
            assert!(error.contains(&bucket), "{error}");
        }
        for keyspace in [&extended, &tied, &reserved] {
            scylla
                .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), &[])
                .await
                .unwrap();
        }
        // a system keyspace
        let mut system = harness.conf.clone();
        "system_schema".clone_into(&mut system.thorium.namespace);
        let error = guard(system).await.expect_err("a system keyspace passed");
        assert!(error.contains("system_schema"), "{error}");
        // a keyspace holding none of the backed-up tables
        let mut empty = harness.conf.clone();
        empty.thorium.namespace.clone_from(&bare);
        let error = guard(empty)
            .await
            .expect_err("a keyspace without Thorium tables passed");
        assert!(error.contains(&bare), "{error}");
    }))
    .catch_unwind()
    .await;
    // drop every keyspace the checks may have created, then report a failed check
    for keyspace in [&other, &bare, &extended, &tied, &reserved] {
        scylla
            .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), &[])
            .await
            .unwrap();
    }
    if let Err(panic) = checked {
        std::panic::resume_unwind(panic);
    }
}

/// The API reports its namespace on `GET /api/system/`; a wipe naming another namespace than
/// thoradm's config, or with a config naming another namespace than the API serves, refuses
/// before anything is deleted in either namespace, and so does an API that reports none
#[tokio::test]
async fn a_wipe_checks_the_named_and_the_api_namespace_first() {
    // the API reports the namespace it serves
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    let info: Value = reqwest::Client::new()
        .get(format!("{}/api/system/", harness.api_url))
        .header(
            "authorization",
            format!(
                "token {}",
                data_encoding::BASE64.encode(harness.token.as_bytes())
            ),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["namespace"], ns.as_str(), "{info:#}");
    // a body without the field parses with none
    let old: SystemInfo = serde_json::from_str(
        r#"{"k8s_cache":false,"bare_metal_cache":false,"windows_cache":false,"kvm_cache":false,"external_cache":false}"#,
    )
    .unwrap();
    assert_eq!(old.namespace, None);
    // a target that holds data, which no refusal may touch
    let (backup, _) = seeded_backup(harness, "namespaces").await;
    let before = target_state(harness).await;
    // a namespace other than thoradm's config names
    let elsewhere = format!("{ns}_elsewhere");
    let run = restore_with(harness.thoradm(), &backup, &["--wipe", &elsewhere, "--yes"]);
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    let record = refusal(&backup, "wipe").expect("no wipe refusal");
    assert!(
        record.error.contains(&ns) && record.error.contains(&elsewhere),
        "{record:?}"
    );
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&backup));
    // a config naming a namespace the API doesn't serve
    let mut conf = harness.conf.clone();
    conf.thorium.namespace.clone_from(&elsewhere);
    let cluster_conf = backup.parent().unwrap().join("elsewhere.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    let copy = backup.parent().unwrap().join("elsewhere-backup");
    copy_tree(&backup, &copy);
    let run = restore_with(
        harness.thoradm_with(&cluster_conf),
        &copy,
        &["--wipe", &elsewhere, "--yes"],
    );
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    let record = refusal(&copy, "wipe").expect("no wipe refusal");
    assert!(
        record.error.contains(&ns) && record.error.contains(&elsewhere),
        "{record:?}"
    );
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&copy));
    // an API that reports no namespace refuses the wipe too
    let error = wipe::check_api(&harness.conf, Ok(old)).expect_err("a missing namespace passed");
    assert!(error.contains("namespace"), "{error}");
    let matching = SystemInfo {
        namespace: Some(ns.clone()),
        ..SystemInfo::default()
    };
    assert_eq!(wipe::check_api(&harness.conf, Ok(matching)), Ok(ns.clone()));
}

/// A wipe whose config points a role at the bucket of a neighbor namespace, whose name extends
/// this one's, refuses before anything is deleted, and the neighbor's object survives
#[tokio::test]
async fn a_wipe_never_deletes_a_neighbors_objects() {
    // a neighbor namespace whose name extends this one's, holding an object in its own bucket
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = harness.conf.thorium.namespace.clone();
    let neighbor = format!("{ns}_dev");
    thorium_keyspace(&scylla, &neighbor).await;
    let bucket = format!("{}-files", neighbor.replace('_', "-"));
    let s3 = harness.s3();
    let _ = s3.create_bucket().bucket(&bucket).send().await;
    s3.put_object()
        .bucket(&bucket)
        .key("neighbors-object")
        .body(b"the neighbor's".to_vec().into())
        .send()
        .await
        .unwrap();
    // this namespace's backup, and a config whose files role names the neighbor's bucket
    let (backup, _) = seeded_backup(harness, "neighbor").await;
    let mut conf = harness.conf.clone();
    conf.thorium.files.bucket.clone_from(&bucket);
    let cluster_conf = backup.parent().unwrap().join("neighbor.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    let run = restore_with(
        harness.thoradm_with(&cluster_conf),
        &backup,
        &["--wipe", &ns, "--yes"],
    );
    let kept = list_keys(&s3, &bucket).await.unwrap();
    scylla
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {neighbor}"), &[])
        .await
        .unwrap();
    empty_bucket(&s3, &bucket).await.unwrap();
    s3.delete_bucket().bucket(&bucket).send().await.unwrap();
    // the wipe refused, naming the bucket, and deleted nothing
    assert!(
        kept.contains(&"neighbors-object".to_owned()),
        "the neighbor's object was deleted: {kept:?}\n{}",
        describe(&run)
    );
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    let record = refusal(&backup, "wipe").expect("no wipe refusal");
    assert!(record.error.contains(&bucket), "{record:?}");
    assert!(!any_deletion(&backup));
}
