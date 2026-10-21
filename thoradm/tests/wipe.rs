//! `backup restore --wipe <namespace>`: the namespace named and the API's namespace are checked
//! first, the wipe's guards refuse targets it can't safely delete from, the wipe is confirmed by
//! `--confirm` or the typed namespace, and it replaces the target's backed-up data with the backup,
//! keeping excluded data and resuming after a failure partway
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use axum::http::Method;
use common::corrupt;
use common::cycle::{describe, wipe};
use common::fixture;
use common::harness::{Harness, bucket_for, list_keys, write_cluster_conf};
use common::mock_s3::MockS3;
use common::names::Names;
use common::proxy::FaultProxy;
use common::seed;
use common::state::{TargetState, target_state};
use futures::FutureExt;
use serde_json::Value;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use strum::IntoEnumIterator;
use thoradm::args::BackupComponents;
use thoradm::backup::failures::{
    FAILURES_FILE, FailureLog, FailurePolicy, FailureRecord, Operation,
};
use thoradm::backup::manifest::Manifest;
use thoradm::backup::sink::TokioFs;
use thoradm::backup::target::Target;
use thoradm::backup::views::ViewStatements;
use thoradm::backup::wipe;
use thoradm::shared::retry::RetryPolicy;
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
    // a truncated table
    wipe_reports(backup).iter().any(|(_, report)| {
        report["tables"]
            .as_array()
            .is_some_and(|tables| tables.iter().any(|table| table["truncated"] == true))
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
/// backup's tables and Redis state exactly, keeps the excluded data and every object the target
/// held, and records the tables it wiped and the buckets it left
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
    // the files bucket now holds B's object beside what the backup holds, which the wipe leaves
    let before_wipe_files = list_keys(&harness.s3(), &files).await.unwrap();
    let orphans: Vec<&String> = before_wipe_files
        .iter()
        .filter(|key| !backed_up_files.contains(key))
        .collect();
    assert!(
        !orphans.is_empty(),
        "B left no object: {before_wipe_files:?}"
    );
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
    // the wipe restore runs without a terminal under --confirm
    let restored = restore_with(harness.thoradm(), &backup, &["--wipe", &ns, "--confirm"]);
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
    // the files bucket holds what the backup holds with B's object still there, since the wipe
    // sends no S3 request: D's object is back and B's remains
    let after_files = list_keys(&harness.s3(), &files).await.unwrap();
    for key in backed_up_files.iter().chain(&before_wipe_files) {
        assert!(after_files.contains(key), "{key} is gone: {after_files:?}");
    }
    assert_eq!(
        after_files.len(),
        before_wipe_files
            .iter()
            .chain(&backed_up_files)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        "{after_files:?}"
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
    // the buckets are named as left untouched, with no per-bucket counts
    let left = report["buckets_left"]
        .as_array()
        .unwrap_or_else(|| panic!("wipe.json has no buckets_left: {report:#}"));
    assert!(
        left.iter().any(|bucket| bucket == files.as_str()),
        "{report:#}"
    );
    assert!(report.get("buckets").is_none(), "{report:#}");
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
        !stderr.contains(&format!("Wiped bucket {files}")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("bucket {files}")) && stderr.contains("left untouched"),
        "the plan doesn't name the bucket as left untouched:\n{stderr}"
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

/// On a target that holds data, `--wipe` without a terminal needs `--confirm`; on a terminal the
/// namespace must be typed, even with `--confirm`: another name aborts and the namespace proceeds;
/// neither refusal changes the target
#[tokio::test]
async fn a_wipe_needs_confirm_or_the_typed_namespace() {
    // a backup, and a target that still holds its data
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "confirm").await;
    let ns = harness.conf.thorium.namespace.clone();
    let before = target_state(harness).await;
    // without a terminal and without --confirm, the wipe is refused, naming both flags
    let refused = restore_with(harness.thoradm(), &backup, &["--wipe", &ns]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let record = refusal(&backup, "confirmation").expect("no confirmation refusal");
    assert!(
        record.error.contains("--confirm") && record.error.contains("--wipe"),
        "{record:?}"
    );
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&backup));
    // on a terminal, typing another name aborts, even with --confirm
    let typed = restore_on_a_terminal(
        harness,
        &backup,
        &["--wipe", &ns, "--confirm"],
        "testing_wrong",
    );
    assert_eq!(typed.status.code(), Some(1), "{}", describe(&typed));
    assert_eq!(target_state(harness).await, before);
    assert!(!any_deletion(&backup));
    // typing the namespace proceeds
    let typed = restore_on_a_terminal(harness, &backup, &["--wipe", &ns], &ns);
    assert_eq!(typed.status.code(), Some(0), "{}", describe(&typed));
    assert!(any_deletion(&backup));
}

/// A wipe whose `TRUNCATE` of one table fails stops the restore before Redis, recording the table
/// and leaving it with its rows; a plain re-run is refused since that table still holds rows; a
/// `--wipe` re-run resumes the earlier wipe, recreates the views it dropped, and the cluster
/// matches the backup
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_wipe_that_fails_partway_stops_and_resumes() {
    // a backup and a target holding its data
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "partway").await;
    let ns = harness.conf.thorium.namespace.clone();
    // a session whose connections all pass through a proxy that resets the one that sends the
    // TRUNCATE of the samples list
    let node: std::net::SocketAddr = harness.conf.scylla.nodes[0].parse().unwrap();
    let proxy = FaultProxy::start(node).await.unwrap();
    let mut conf = harness.conf.clone();
    conf.scylla.nodes = vec![proxy.listen.to_string()];
    let translator =
        common::proxy::scylla_translator(&*harness.scylla().await, node, proxy.listen).await;
    thoradm::shared::scylla::route_through(&proxy.listen.to_string(), Arc::new(translator));
    let scylla = thoradm::shared::scylla::get_client(&conf)
        .await
        .expect("failed to open a session");
    proxy.reset_on(Some(format!("TRUNCATE {ns}.samples_list").into_bytes()));
    // the Redis keys before, which a stopped wipe leaves alone
    let redis_before = target_state(harness).await.redis_keys;
    // the wipe, run in process into a report directory a later run finds beside the backup's
    let all: Vec<BackupComponents> = BackupComponents::iter()
        .filter(|component| !component.is_filter())
        .collect();
    let target = Target::new(&harness.conf, &all);
    let reports = backup.join("reports").join("restore-fault");
    std::fs::create_dir_all(&reports).unwrap();
    let log = FailureLog::open(&reports, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    // the statements of the views the wipe drops, which the resumed restore recreates them from
    let (views, unfound) = ViewStatements::capture(&scylla, &ns, &target.tables, &backup, &reports)
        .await
        .expect("failed to capture the views");
    assert!(unfound.is_empty(), "{unfound:?}");
    views
        .save(&reports, &TokioFs)
        .await
        .expect("failed to save the views");
    let snapshots = wipe::ScyllaSnapshots { scylla: &scylla };
    let plan = wipe::plan(&scylla, &snapshots, &target, ns.clone(), &backup, None)
        .await
        .expect("failed to build the plan");
    let wiper = wipe::Wiper {
        scylla: &scylla,
        snapshots: &snapshots,
        target: &target,
        log: &log,
        reports: &reports,
        sinks: &TokioFs,
        retry: RetryPolicy::new(1),
    };
    let stopped = wiper.run(plan).await.expect("the wipe errored");
    assert!(stopped.is_none(), "a failed TRUNCATE didn't stop the wipe");
    assert!(!proxy.waiting(), "the fault never fired");
    // a wipe record names the table
    let failure = records(&backup)
        .into_iter()
        .find(|record| record.operation == Operation::Wipe)
        .expect("no wipe record");
    assert_eq!(
        failure.table.as_deref(),
        Some("samples_list"),
        "{failure:?}"
    );
    assert!(failure.bucket.is_none(), "{failure:?}");
    // wipe.json has no finished time, the failed table isn't truncated, and the tables before it
    // are; the buckets are named as left, and no bucket was counted
    let reports_found = wipe_reports(&backup);
    let (earlier, report) = reports_found.last().expect("no wipe.json").clone();
    assert!(
        report.get("finished").is_none_or(Value::is_null),
        "{report:#}"
    );
    let tables = report["tables"].as_array().unwrap();
    let at = tables
        .iter()
        .position(|table| table["table"] == "samples_list")
        .expect("samples_list isn't in the plan");
    assert_eq!(tables[at]["truncated"], false, "{report:#}");
    assert!(
        tables[..at].iter().all(|table| table["truncated"] == true),
        "{report:#}"
    );
    assert!(report["buckets_left"].as_array().is_some(), "{report:#}");
    assert!(report.get("buckets").is_none(), "{report:#}");
    // nothing was restored: the failed table keeps its rows and Redis is as it was
    let state = target_state(harness).await;
    assert!(state.rows["samples_list"] > 0, "{state:?}");
    assert_eq!(state.redis_keys, redis_before);
    // a plain re-run is refused, since the table not yet truncated still holds rows, and the
    // refusal names tables only
    let refused = restore_with(harness.thoradm(), &backup, &["--confirm"]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let record = refusal(&backup, "empty-target").expect("no empty-target refusal");
    assert!(record.error.contains("samples_list"), "{record:?}");
    assert!(!record.error.contains("bucket"), "{record:?}");
    // the fault has fired, so a --wipe re-run resumes the earlier wipe and finishes the restore
    let resumed = restore_with(harness.thoradm(), &backup, &["--wipe", &ns, "--confirm"]);
    assert_eq!(resumed.status.code(), Some(0), "{}", describe(&resumed));
    // it names the earlier wipe and records it as resumed_from
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains(&earlier.display().to_string()), "{stderr}");
    let reports_found = wipe_reports(&backup);
    let (_, report) = reports_found
        .iter()
        .rev()
        .find(|(dir, _)| *dir != earlier)
        .expect("the resumed wipe wrote no wipe.json");
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

/// With `--wipe <namespace> --confirm` on a target that holds data, a damaged backup and a backup
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
    let run = restore_with(harness.thoradm(), &flipped, &["--wipe", &ns, "--confirm"]);
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
    let run = restore_with(harness.thoradm(), &recorded, &["--wipe", &ns, "--confirm"]);
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

/// The wipe's guards refuse a namespace that is a system keyspace or holds no backed-up table;
/// bucket names are no concern of the wipe, which deletes no object, so roles sharing a bucket,
/// default bucket names, and bucket names a neighbor namespace carries all pass while another
/// Thorium keyspace exists
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
        let guard = |conf: thorium::Conf| {
            let scylla = scylla.clone();
            async move { wipe::guards(&conf, &scylla).await }
        };
        // the namespace's own config passes while another Thorium keyspace exists
        assert_eq!(guard(harness.conf.clone()).await, Ok(()));
        // two roles sharing a bucket pass: nothing is deleted from a bucket
        let mut shared = harness.conf.clone();
        shared
            .thorium
            .ephemeral
            .bucket
            .clone_from(&shared.thorium.files.bucket);
        assert_eq!(guard(shared).await, Ok(()));
        // a default bucket name, and a name without the namespace, pass beside another keyspace
        let mut default = harness.conf.clone();
        "thorium-files".clone_into(&mut default.thorium.files.bucket);
        assert_eq!(guard(default).await, Ok(()));
        let mut unnamed = harness.conf.clone();
        "testing-elsewhere-files".clone_into(&mut unnamed.thorium.files.bucket);
        assert_eq!(guard(unnamed).await, Ok(()));
        // a bucket named for a neighbor whose name extends this namespace's passes too
        thorium_keyspace(&scylla, &extended).await;
        let mut neighbors = harness.conf.clone();
        neighbors.thorium.files.bucket = format!("{}-files", extended.replace('_', "-"));
        assert_eq!(guard(neighbors).await, Ok(()));
        scylla
            .query_unpaged(format!("DROP KEYSPACE IF EXISTS {extended}"), &[])
            .await
            .unwrap();
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
    for keyspace in [&other, &bare, &extended] {
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
    let run = restore_with(
        harness.thoradm(),
        &backup,
        &["--wipe", &elsewhere, "--confirm"],
    );
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
        &["--wipe", &elsewhere, "--confirm"],
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

/// A wipe restore never lists a bucket or deletes an object: every S3 request it sends is the
/// restore's own put or head of a key the backup names, and an object the backup doesn't name
/// survives
#[tokio::test(flavor = "multi_thread")]
async fn a_wipe_sends_no_s3_request() {
    // a backup, and an object in the files bucket that no backed-up row names
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "no-s3").await;
    let ns = harness.conf.thorium.namespace.clone();
    let files = bucket_for(&harness.conf, "files");
    common::objects::put(
        &harness.s3(),
        &files,
        "orphan/left-behind.bin",
        &common::columns::Body::Bytes(b"no row names this".to_vec()),
    )
    .await;
    // a cluster config that sends thoradm's S3 requests through a recording proxy
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let cluster_conf = backup.parent().unwrap().join("recording.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    let run = restore_with(
        harness.thoradm_with(&cluster_conf),
        &backup,
        &["--wipe", &ns, "--confirm"],
    );
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    // no bucket-level request (a listing) and no delete or batch delete was sent
    let seen = mock.seen().await;
    assert!(!seen.is_empty(), "the restore sent no S3 request at all");
    for request in &seen {
        assert!(
            !request.key.is_empty(),
            "a bucket-level request: {} {}",
            request.method,
            request.path
        );
        assert!(
            request.method != Method::DELETE && !request.path.contains("delete"),
            "a deleting request: {} {}",
            request.method,
            request.path
        );
    }
    // the unnamed object is still there
    let kept = list_keys(&harness.s3(), &files).await.unwrap();
    assert!(
        kept.contains(&"orphan/left-behind.bin".to_owned()),
        "{kept:?}"
    );
}

/// A `wipe.json` an earlier thoradm wrote, with per-bucket counts and no `buckets_left`, still
/// parses and is the unfinished wipe a `--wipe` re-run resumes
#[tokio::test]
async fn an_old_wipe_report_with_buckets_still_resumes() {
    // a backup and an old-format unfinished wipe.json among its restore reports
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "old-report").await;
    let ns = harness.conf.thorium.namespace.clone();
    let files = bucket_for(&harness.conf, "files");
    let earlier = backup.join("reports").join("restore-old");
    std::fs::create_dir_all(&earlier).unwrap();
    let old = serde_json::json!({
        "namespace": ns,
        "api_namespace": ns,
        "backup": backup,
        "started": "2026-10-01T00:00:00Z",
        "tables": [{
            "table": "samples_list",
            "estimated_partitions": 3,
            "views_dropped": [],
            "truncated": false
        }],
        "buckets": [{
            "role": "files",
            "bucket": files,
            "planned_objects": 4,
            "planned_bytes": 4096,
            "deleted_objects": 0,
            "deleted_bytes": 0,
            "done": false
        }],
        "kept": ["logs"]
    });
    std::fs::write(
        earlier.join("wipe.json"),
        serde_json::to_vec_pretty(&old).unwrap(),
    )
    .unwrap();
    // it is found as the unfinished wipe of the namespace
    let found =
        wipe::find_unfinished(&backup, &backup.join("reports").join("restore-new"), &ns).await;
    assert_eq!(found.as_deref(), Some(earlier.as_path()));
    // a --wipe re-run resumes it and names the buckets it left
    let resumed = restore_with(harness.thoradm(), &backup, &["--wipe", &ns, "--confirm"]);
    assert_eq!(resumed.status.code(), Some(0), "{}", describe(&resumed));
    let reports = wipe_reports(&backup);
    let (_, report) = reports
        .iter()
        .rev()
        .find(|(dir, _)| *dir != earlier)
        .expect("the resumed wipe wrote no wipe.json");
    assert_eq!(
        report["resumed_from"],
        earlier.display().to_string(),
        "{report:#}"
    );
    assert!(report["finished"].is_string(), "{report:#}");
    let left = report["buckets_left"].as_array().expect("no buckets_left");
    assert!(
        left.iter().any(|bucket| bucket == files.as_str()),
        "{report:#}"
    );
}
