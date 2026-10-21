//! Failure accounting of real backup runs: records, summaries, exit codes, and stopping early

mod common;

use common::harness::Harness;
use common::names::Names;
use common::seed;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
use std::sync::Arc;
use thoradm::backup::controllers::restore::RestoreController;
use thoradm::backup::failures::{
    FAILURES_FILE, FailureKind, FailureLog, FailurePolicy, Operation, read_records,
};
use thoradm::backup::sink::TokioFs;
use thoradm::outcome::RunOutcome;
use thorium::CtlConf;

/// Makes a directory read-only or writable again
///
/// # Arguments
///
/// * `dir` - The directory to change
/// * `writable` - Whether to make it writable
fn set_writable(dir: &Path, writable: bool) {
    // read-only keeps listing and traversal but refuses new files
    let mode = if writable { 0o755 } else { 0o555 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Runs `thoradm backup new` into an output directory with stdout and stderr both piped
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `out` - The output directory
/// * `extra` - Extra arguments for `backup new`
fn backup_new(harness: &Harness, out: &Path, extra: &[&str]) -> Output {
    // run the binary with both output streams captured, so neither is a terminal
    harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "2", "-o"])
        .arg(out)
        .args(extra)
        .output()
        .expect("failed to run thoradm")
}

/// Seeds one sample so the samples list table has a row to back up
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn seed_one_sample(harness: &Harness) {
    // upload one sample into a fresh group
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("minerva");
    seed::group(&client, &group).await;
    seed::sample(
        &client,
        &group,
        "ceres.bin",
        b"a sample for the accounting tests",
    )
    .await;
}

/// An unwritable table directory is recorded and reported on stderr, the other components
/// finish, the run exits 2, and a new backup after the fault is fixed exits 0
#[tokio::test]
async fn unwritable_table_directory_is_recorded_and_the_run_finishes_with_exit_2() {
    // seed a sample and make its table's data directory read-only
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    seed_one_sample(harness).await;
    let out = harness.scratch("unwritable-continue");
    let data = out.join("samples_list").join("data");
    std::fs::create_dir_all(&data).unwrap();
    set_writable(&data, false);
    // back up everything with both output streams piped
    let output = backup_new(harness, &out, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // the run reached its end with recorded failures
    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    // the error appears on stderr through the log, with the component's summary line
    assert!(stderr.contains("ERROR"), "stderr:\n{stderr}");
    assert!(stderr.contains("Permission denied"), "stderr:\n{stderr}");
    let summary = stderr
        .lines()
        .find(|line| line.contains("samples-list: "))
        .unwrap_or_else(|| panic!("no samples-list summary in:\n{stderr}"));
    assert!(
        summary.contains("samples-list: 0 handled, 0 skipped, "),
        "{summary}"
    );
    assert!(!summary.contains(" 0 failed"), "{summary}");
    // the failures file names the component, table, and operation
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    let failed = records
        .iter()
        .find(|record| record.component == "samples-list")
        .unwrap_or_else(|| panic!("no samples-list record in {records:?}"));
    assert_eq!(failed.kind, FailureKind::Failed);
    assert_eq!(failed.operation, Operation::WriteArchive);
    assert_eq!(failed.table.as_deref(), Some("samples_list"));
    // the record names an archive file directly inside the table's data directory
    let archive = Path::new(failed.key.as_deref().unwrap());
    assert_eq!(archive.parent(), Some(data.as_path()), "{archive:?}");
    // the components after it still ran
    assert!(out.join("tags").join("maps").is_dir());
    assert!(out.join("redis.json").is_file());
    // the finished backup is never changed, so once the fault is fixed a backup into a new
    // directory completes
    set_writable(&data, true);
    let rerun = backup_new(harness, &harness.scratch("unwritable-continue-rerun"), &[]);
    let rerun_stderr = String::from_utf8_lossy(&rerun.stderr);
    assert_eq!(rerun.status.code(), Some(0), "stderr:\n{rerun_stderr}");
    let summary = rerun_stderr
        .lines()
        .find(|line| line.contains("samples-list: "))
        .unwrap_or_else(|| panic!("no samples-list summary in:\n{rerun_stderr}"));
    assert!(
        summary.contains(" handled, 0 skipped, 0 failed"),
        "{summary}"
    );
    assert!(!summary.contains("samples-list: 0 handled"), "{summary}");
}

/// Under `--abort-on-failure` the same fault is recorded, the run stops with exit 1, and the
/// components after it don't run
#[tokio::test]
async fn unwritable_table_directory_stops_the_run_under_abort_on_failure() {
    // seed a sample and make its table's data directory read-only
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    seed_one_sample(harness).await;
    let out = harness.scratch("unwritable-abort");
    let data = out.join("samples_list").join("data");
    std::fs::create_dir_all(&data).unwrap();
    set_writable(&data, false);
    // back up a few components, the samples list among them, stopping at the first failure
    let output = backup_new(
        harness,
        &out,
        &["redis,s3-ids,samples-list,tags", "--abort-on-failure"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // the run stopped as an abort
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    // the record names the component and operation
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(
        records
            .iter()
            .all(|record| record.component == "samples-list"),
        "{records:?}"
    );
    assert_eq!(records[0].operation, Operation::WriteArchive);
    // the table before it ran; the table after it and Redis, which runs after every table,
    // didn't
    assert!(out.join("s3_ids").join("maps").is_dir());
    assert!(!out.join("tags").exists(), "tags ran after the abort");
    assert!(
        !out.join("redis.json").exists(),
        "redis ran after the abort"
    );
    set_writable(&data, true);
}

/// Returns every record the restores of a backup wrote in their report directories
///
/// # Arguments
///
/// * `backup` - The backup the restores read
fn restore_records(backup: &Path) -> Vec<thoradm::backup::failures::FailureRecord> {
    // each restore writes its failures file in its own report directory
    let Ok(reports) = std::fs::read_dir(backup.join("reports")) else {
        return Vec::new();
    };
    reports
        .filter_map(Result::ok)
        .map(|report| report.path().join(FAILURES_FILE))
        .filter(|path| path.is_file())
        .flat_map(|path| read_records(&path).unwrap())
        .collect()
}

/// Returns the first directory under a root that holds a file
///
/// # Arguments
///
/// * `root` - The directory to search
fn dir_with_a_file(root: &Path) -> Option<std::path::PathBuf> {
    // search depth first, files before subdirectories
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    if entries.iter().any(|path| path.is_file()) {
        return Some(root.to_path_buf());
    }
    entries
        .iter()
        .filter(|path| path.is_dir())
        .find_map(|path| dir_with_a_file(path))
}

/// A backed-up object that can't be read refuses the restore at its preflight; the restore step
/// itself, past the preflight, records the object on its own and carries on to finish with
/// failures instead of stopping
#[tokio::test]
async fn an_unreadable_backed_up_object_is_recorded_and_the_restore_finishes() {
    // back up a sample's row and object
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    seed_one_sample(harness).await;
    let out = harness.scratch("unreadable-object");
    let taken = backup_new(harness, &out, &["s3-ids,s3-ids-objects"]);
    let stderr = String::from_utf8_lossy(&taken.stderr);
    assert_eq!(taken.status.code(), Some(0), "stderr:\n{stderr}");
    // a directory that can be listed but not searched lists its objects but can't stat them
    let objects = out.join("s3_ids").join("objects");
    let unreadable = dir_with_a_file(&objects).expect("the backup holds no objects");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o444)).unwrap();
    // the preflight's verify finds the object unreadable and refuses before any write
    let refused = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "restore",
            "--confirm",
            "--merge",
            "-b",
        ])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(1), "stderr:\n{stderr}");
    let records = restore_records(&out);
    assert!(
        records
            .iter()
            .any(|record| record.operation == Operation::Verify
                && record.component == "s3-ids-objects"),
        "{records:?}"
    );
    // the restore step itself records the object and finishes
    let reports = harness.scratch("unreadable-object-report");
    let log = Arc::new(
        FailureLog::open(&reports, FailurePolicy::Continue, &TokioFs)
            .await
            .unwrap(),
    );
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let scylla = harness.scylla().await;
    let mut controller = RestoreController::new(&harness.conf, ctl_conf, &scylla, 2);
    let restored = Box::pin(controller.restore(&out, &log)).await;
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();
    restored.expect("the restore stopped instead of recording the object");
    assert_eq!(
        RunOutcome::finished(&log),
        RunOutcome::CompletedWithFailures
    );
    let records = read_records(&reports.join(FAILURES_FILE)).unwrap();
    let record = records
        .iter()
        .find(|record| record.operation == Operation::ReadArchive)
        .unwrap_or_else(|| panic!("no record of the unreadable object in {records:?}"));
    assert_eq!(record.component, "s3-ids-objects");
    let key = Path::new(record.key.as_deref().unwrap_or_default());
    assert_eq!(key.parent(), Some(unreadable.as_path()), "{record:?}");
}

/// A backup of tables alone runs on a host with no root certificates, where building an S3
/// client fails, because only S3 components build one
#[tokio::test]
async fn a_table_backup_needs_no_root_certificates() {
    // seed a sample under the shared guard, so the restoring test can't drop the schema under it
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    seed_one_sample(harness).await;
    // an empty certificate file and directory stand in for a host with no root certificates
    let scratch = harness.scratch("no-root-certificates");
    let certs = scratch.join("certs");
    std::fs::create_dir_all(&certs).unwrap();
    let bundle = scratch.join("empty.pem");
    std::fs::write(&bundle, b"").unwrap();
    let out = scratch.join("backup");
    let taken = harness
        .thoradm()
        .env("SSL_CERT_FILE", &bundle)
        .env("SSL_CERT_DIR", &certs)
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            "samples-list",
            "-m",
            "2",
            "-o",
        ])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    // the tables were backed up and the run exited 0
    let stderr = String::from_utf8_lossy(&taken.stderr);
    assert_eq!(taken.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        out.join("samples_list").join("maps").is_dir(),
        "stderr:\n{stderr}"
    );
}
