//! The checks a restore runs before its first write: unusable backups, a target that holds data,
//! a backup with recorded failures, and what the confirmation names
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use axum::http::Method;
use common::corrupt;
use common::cycle::{admin_login, describe, put_admin_login, wipe};
use common::harness::{Harness, list_keys, write_cluster_conf};
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::names::Names;
use common::seed;
use common::state::{TargetState, target_state};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thoradm::backup::coverage::{Category, Coverage, Registry};
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, Operation, read_records};
use thoradm::backup::manifest::{InProgressMarker, MANIFEST_FILE, Manifest};
use thoradm::backup::sink::TokioFs;

/// The components these tests back up: samples with their objects, tags, and Redis
const COMPONENTS: &str = "samples-list,s3-ids,s3-ids-objects,tags,redis";

/// What one restore did
struct Run {
    /// The process output
    output: Output,
    /// The records of its report directory's failures file
    records: Vec<FailureRecord>,
    /// Its report directory
    report_dir: PathBuf,
}

impl Run {
    /// Describes the run for an assertion message
    fn describe(&self) -> String {
        // the exit code, stderr, and the records
        format!(
            "{}\nrecords: {:#?}",
            describe(&self.output),
            self.records
                .iter()
                .map(|record| format!(
                    "{:?} {} {:?} {:?}: {}",
                    record.operation, record.component, record.object, record.key, record.error
                ))
                .collect::<Vec<_>>()
        )
    }

    /// Returns the preflight refusal's record, which names the check that refused
    fn refusal(&self) -> Option<&FailureRecord> {
        // the refusal is the restore's own preflight record
        self.records.iter().find(|record| {
            record.component == "restore" && record.operation == Operation::Preflight
        })
    }

    /// Checks the run was refused by one check with exit 1, its reason containing some text
    ///
    /// # Arguments
    ///
    /// * `check` - The preflight check that must have refused
    /// * `text` - Text the refusal's reason must contain
    fn assert_refused(&self, check: &str, text: &str) {
        // exit 1 and a preflight record naming the check
        assert_eq!(self.output.status.code(), Some(1), "{}", self.describe());
        let refusal = self
            .refusal()
            .unwrap_or_else(|| panic!("no preflight record: {}", self.describe()));
        assert_eq!(
            refusal.object.as_deref(),
            Some(check),
            "{}",
            self.describe()
        );
        assert!(refusal.error.contains(text), "{text}: {}", self.describe());
    }

    /// Returns the restore's summary, when it wrote one
    fn summary(&self) -> serde_json::Value {
        // the summary sits beside the failures file
        let bytes = std::fs::read(self.report_dir.join("summary.json"))
            .unwrap_or_else(|error| panic!("no summary.json ({error}): {}", self.describe()));
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// Restores a backup without a terminal, its report in a fresh directory beside the backup
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to restore
/// * `extra` - Extra arguments for `backup restore`
fn restore(harness: &Harness, backup: &Path, extra: &[&str]) -> Run {
    // a fresh report directory, so this run's records are the only ones read
    let report_dir = backup
        .parent()
        .expect("the backup has a parent")
        .join(format!("report-{}", uuid::Uuid::new_v4()));
    let output = harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(&report_dir)
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm");
    let records = read_records(&report_dir.join(FAILURES_FILE)).unwrap_or_default();
    Run {
        output,
        records,
        report_dir,
    }
}

/// Empties the target, seeds a group with a sample, and backs up its tables, objects, and Redis
/// state, returning the backup
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
async fn seeded_backup(harness: &Harness, name: &str) -> PathBuf {
    // one sample in a group of its own, in a target no other test left data in
    empty_target(harness).await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("ceres");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "ceres.bin", names.suffix.as_bytes()).await;
    // a backup of what restore will write back
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", COMPONENTS, "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    out
}

/// Empties the target's backed-up data as a disaster would, then initializes the system again,
/// as a fresh install does, so the Redis backup of a later seed finds the system settings
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

/// Copies a directory tree, leaving out the reports earlier runs wrote
///
/// # Arguments
///
/// * `from` - The directory to copy
/// * `to` - Where to put the copy
fn copy_backup(from: &Path, to: &Path) {
    // copy every file and directory below the root but its reports
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        if entry.file_name() == "reports" {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_backup(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Returns the data file of the first archive of a table that holds a partition
///
/// # Arguments
///
/// * `backup` - The backup
/// * `table` - The table
async fn data_file(backup: &Path, table: &str) -> PathBuf {
    // the manifest lists every archive with its entry count
    let manifest = Manifest::read(backup).await.unwrap().expect("no manifest");
    let archive = manifest.tables[table]
        .archives
        .iter()
        .find(|archive| archive.entries > 0)
        .unwrap_or_else(|| panic!("{table} has no archive holding a partition"));
    backup
        .join(table)
        .join("data")
        .join(archive.uuid.to_string())
}

/// Checks a refused restore left the target as it was
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `before` - The target before the restore
/// * `run` - The refused restore
async fn assert_unchanged(harness: &Harness, before: &TargetState, run: &Run) {
    // every row count, Redis key, and object listing is as it was
    let after = target_state(harness).await;
    assert_eq!(
        &after,
        before,
        "the refused restore changed the target: {}",
        run.describe()
    );
}

/// Interrupted, newer, incomplete, damaged, and legacy backups, and backups whose manifest was
/// removed, are each refused before anything is written, naming the check, and `--legacy`
/// restores only a legacy backup
#[tokio::test]
async fn unusable_backups_are_refused_before_any_write() {
    // a clean backup, and an empty target so only the backup's checks can refuse
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let backup = seeded_backup(harness, "unusable").await;
    empty_target(harness).await;
    let before = target_state(harness).await;
    let copy = |name: &str| {
        let to = backup.parent().unwrap().join(name);
        copy_backup(&backup, &to);
        to
    };
    // an interrupted backup: a marker and no manifest
    let interrupted = copy("interrupted");
    corrupt::delete(&interrupted.join(MANIFEST_FILE)).unwrap();
    InProgressMarker::new(chrono::Utc::now(), Vec::new())
        .write(&interrupted, &TokioFs)
        .await
        .unwrap();
    let run = restore(harness, &interrupted, &["--yes"]);
    run.assert_refused("manifest", "interrupted");
    assert_unchanged(harness, &before, &run).await;
    // --legacy doesn't make an interrupted backup restorable
    let run = restore(harness, &interrupted, &["--yes", "--legacy"]);
    run.assert_refused("manifest", "interrupted");
    assert_unchanged(harness, &before, &run).await;
    // a manifest of a newer format version
    let newer = copy("newer");
    let version = thoradm::backup::manifest::FORMAT_VERSION + 1;
    corrupt::edit_json(
        &newer.join(MANIFEST_FILE),
        "/format_version",
        serde_json::Value::from(version),
    )
    .unwrap();
    let run = restore(harness, &newer, &["--yes"]);
    run.assert_refused("manifest", &format!("format version {version}"));
    assert_unchanged(harness, &before, &run).await;
    // a table directory the manifest lists, missing
    let missing_table = copy("missing-table");
    std::fs::remove_dir_all(missing_table.join("tags")).unwrap();
    let run = restore(harness, &missing_table, &["--yes"]);
    run.assert_refused("verify", "backup verify found");
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some("tags")),
        "{}",
        run.describe()
    );
    assert_unchanged(harness, &before, &run).await;
    // a missing redis.json
    let missing_redis = copy("missing-redis");
    corrupt::delete(&missing_redis.join("redis.json")).unwrap();
    let run = restore(harness, &missing_redis, &["--yes"]);
    run.assert_refused("verify", "backup verify found");
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some("redis.json")),
        "{}",
        run.describe()
    );
    assert_unchanged(harness, &before, &run).await;
    // a flipped byte in an archive
    let flipped = copy("flipped");
    corrupt::flip_byte(&data_file(&flipped, "samples_list").await, 0).unwrap();
    let run = restore(harness, &flipped, &["--yes"]);
    run.assert_refused("verify", "backup verify found");
    assert_unchanged(harness, &before, &run).await;
    // --legacy given for a backup with a manifest
    let run = restore(harness, &backup, &["--yes", "--legacy"]);
    run.assert_refused("manifest", "--legacy");
    assert_unchanged(harness, &before, &run).await;
    // a backup whose manifest was removed is refused, with or without --legacy, since its
    // archives' format can't be known
    let removed = copy("manifest-removed");
    corrupt::delete(&removed.join(MANIFEST_FILE)).unwrap();
    let run = restore(harness, &removed, &["--yes"]);
    run.assert_refused("manifest", "--legacy");
    assert_unchanged(harness, &before, &run).await;
    let run = restore(harness, &removed, &["--yes", "--legacy"]);
    run.assert_refused("manifest", "its manifest was removed");
    assert_unchanged(harness, &before, &run).await;
    // a legacy backup, the committed one main's thoradm wrote, without --legacy
    let legacy = backup.parent().unwrap().join("legacy");
    copy_backup(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/legacy/backup"),
        &legacy,
    );
    let run = restore(harness, &legacy, &["--yes"]);
    run.assert_refused("manifest", "--legacy");
    assert_unchanged(harness, &before, &run).await;
    // and with it: the legacy backup restores, warning that its completeness wasn't checked; the
    // legacy backup replaces the harness admin's login, so later tests get it back
    let login = admin_login(harness).await;
    let run = restore(harness, &legacy, &["--yes", "--legacy"]);
    put_admin_login(harness, &login).await;
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(stderr.contains("as a legacy backup (--legacy)"), "{stderr}");
    assert!(stderr.contains("restored as a legacy backup"), "{stderr}");
    assert_eq!(run.summary()["legacy"], true, "{}", run.describe());
    assert!(target_state(harness).await.rows["samples_list"] > 0);
}

/// A target that holds a sample is refused, naming the table, the bucket, and `--merge`; with
/// `--merge` the restore keeps what the target holds; an empty target restores without a flag
#[tokio::test]
async fn a_target_holding_data_needs_merge() {
    // back up a sample, which the target then still holds
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let backup = seeded_backup(harness, "non-empty").await;
    let before = target_state(harness).await;
    // without a flag the restore is refused, naming what holds data and the flag
    let run = restore(harness, &backup, &["--yes"]);
    run.assert_refused("empty-target", "--merge");
    let refusal = run.refusal().unwrap();
    // the refusal names the table and the bucket that hold data, and nothing changed
    let ns = &harness.conf.thorium.namespace;
    assert!(
        refusal.error.contains(&format!("{ns}.samples_list")),
        "{}",
        run.describe()
    );
    assert!(
        refusal.error.contains(&harness.conf.thorium.files.bucket),
        "{}",
        run.describe()
    );
    assert_unchanged(harness, &before, &run).await;
    // a sample the backup doesn't hold survives a --merge restore
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("vulcan");
    seed::group(&client, &group).await;
    let extra = seed::sample(&client, &group, "vulcan.bin", names.suffix.as_bytes()).await;
    // every object the target holds before the merge
    let extra_objects = list_keys(&harness.s3(), &harness.conf.thorium.files.bucket)
        .await
        .unwrap();
    // the merge restores, naming what it merged into
    let run = restore(harness, &backup, &["--yes", "--merge"]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    let merged = run.summary()["merged_into"].clone();
    assert_ne!(merged, serde_json::json!([]), "{}", run.describe());
    // the extra sample's row and every object it held are still there
    let scylla = harness.scylla().await;
    let found = scylla
        .query_unpaged(
            format!("SELECT sha256 FROM {ns}.samples_list WHERE group = ? ALLOW FILTERING"),
            (group.as_str(),),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect::<Vec<_>>();
    assert!(found.contains(&extra), "{found:?}");
    let objects = list_keys(&harness.s3(), &harness.conf.thorium.files.bucket)
        .await
        .unwrap();
    for key in &extra_objects {
        assert!(objects.contains(key), "{key} is gone after the merge");
    }
    // an empty target restores without a flag
    empty_target(harness).await;
    let run = restore(harness, &backup, &["--yes"]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    assert!(target_state(harness).await.rows["samples_list"] > 0);
}

/// A backup whose manifest records a failed object is refused, listing the object and naming
/// `--accept-failures`; with it the restore exits 0, warns, and its summary lists the object;
/// the flag takes no value
#[tokio::test]
async fn a_backup_with_recorded_failures_needs_accept_failures() {
    // seed two samples and back them up while S3 answers 404 for one of their objects
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("janus");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "janus-a.bin", b"the first face").await;
    seed::sample(&client, &group, "janus-b.bin", b"the second face").await;
    // one sample object, which S3 answers 404 for through a mock
    let files = list_keys(&harness.s3(), &harness.conf.thorium.files.bucket)
        .await
        .unwrap();
    let missing = files.first().expect("no sample objects").clone();
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    mock.set_rules(vec![S3Rule {
        method: Method::GET,
        key: missing.clone(),
        query: None,
        nth: None,
        behavior: S3Behavior::Status {
            status: 404,
            code: "NoSuchKey".to_owned(),
        },
    }])
    .await;
    // a cluster config that sends the backup's S3 requests through the mock
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let scratch = harness.scratch("recorded");
    let cluster_conf = scratch.join("thorium.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    // the backup records the object as failed and exits 2
    let backup = scratch.join("backup");
    let mut command = harness.thoradm_with(&cluster_conf);
    command
        .args(["backup", "new", COMPONENTS, "--retries", "1", "-o"])
        .arg(&backup);
    let taken = tokio::process::Command::from(command)
        .output()
        .await
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    let manifest = Manifest::read(&backup).await.unwrap().expect("no manifest");
    assert_eq!(manifest.failures, 1);
    // the restore is refused, listing the missing object and naming the flag
    empty_target(harness).await;
    let before = target_state(harness).await;
    let run = restore(harness, &backup, &["--yes"]);
    run.assert_refused("recorded-failures", "--accept-failures");
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some(missing.as_str())),
        "{}",
        run.describe()
    );
    assert_unchanged(harness, &before, &run).await;
    // with --accept-failures it restores, warns, and lists the missing object
    let run = restore(harness, &backup, &["--yes", "--accept-failures"]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    // the log warns and lists the object
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(stderr.contains("(--accept-failures)"), "{stderr}");
    assert!(
        stderr.lines().any(
            |line| line.contains("restore summary: missing s3-ids-objects")
                && line.contains(&missing)
        ),
        "{stderr}"
    );
    // so does the summary, by component, bucket, and key
    let listed = run.summary()["missing_from_backup"].clone();
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed:#}");
    assert_eq!(listed[0]["key"], missing.as_str(), "{listed:#}");
    assert_eq!(listed[0]["component"], "s3-ids-objects", "{listed:#}");
    assert!(listed[0]["bucket"].is_string(), "{listed:#}");
    // the flag takes no value
    let run = restore(harness, &backup, &["--yes", "--accept-failures=1"]);
    assert_eq!(run.output.status.code(), Some(1), "{}", run.describe());
    assert!(
        String::from_utf8_lossy(&run.output.stderr).contains("--accept-failures"),
        "{}",
        run.describe()
    );
}

/// Under `--yes` the confirmation goes to the log, naming the Redis host and namespace and every
/// excluded registry entry with its reason
#[tokio::test]
async fn the_confirmation_names_redis_and_everything_the_backup_lacks() {
    // restore a clean backup into an empty target
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let backup = seeded_backup(harness, "confirmation").await;
    empty_target(harness).await;
    let run = restore(harness, &backup, &["--yes"]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    // the Redis host and namespace
    let conf = &harness.conf;
    let redis = format!(
        "Redis {}:{}, namespace {}",
        conf.redis.host, conf.redis.port, conf.thorium.namespace
    );
    assert!(stderr.contains(&redis), "{redis}: {stderr}");
    assert!(
        stderr.contains("The Redis restore replaces the namespace's users"),
        "{stderr}"
    );
    // the Redis restore deletes every key of the namespace, and only of the namespace
    let wipe = format!(
        "deletes every key of namespace {} in the Redis database at {}:{}",
        conf.thorium.namespace, conf.redis.host, conf.redis.port
    );
    assert!(
        stderr.contains(&wipe),
        "the confirmation doesn't say the namespace's keys are deleted: {stderr}"
    );
    assert!(
        stderr.contains("keys of other namespaces are left alone"),
        "the confirmation doesn't say other namespaces are left alone: {stderr}"
    );
    assert!(!stderr.contains("whole Redis database"), "{stderr}");
    // a backup without notifications says nothing of their TTLs
    assert!(!stderr.contains("default TTL"), "{stderr}");
    // every ephemeral entry, and every other excluded one, with its reason
    let mut ephemeral = 0;
    for entry in Registry::excluded() {
        let Coverage::Excluded { reason } = entry.coverage else {
            continue;
        };
        ephemeral += usize::from(entry.category == Category::Ephemeral);
        let line = format!("{} ({}): {reason}", entry.name, entry.category.label());
        assert!(stderr.contains(&line), "{line}: {stderr}");
    }
    assert!(ephemeral > 5, "only {ephemeral} ephemeral entries");
}
