//! `backup verify` and `backup scrub` check a backup directory against its manifest: a clean
//! backup passes every check, and each kind of damage is recorded naming what is damaged

mod common;

use common::corrupt;
use common::fixture;
use common::harness::Harness;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Output;
use thoradm::backup::coverage::{COVERAGE, Coverage, Store};
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, read_records};
use thoradm::backup::manifest::{InProgressMarker, MANIFEST_FILE, MARKER_FILE, Manifest};
use thoradm::backup::sink::TokioFs;
use tokio::sync::OnceCell;

/// The clean backup every test copies, taken once per binary
static BACKUP: OnceCell<PathBuf> = OnceCell::const_new();

/// Returns the clean backup of the broad fixture, taking it on first use
///
/// The backup holds the `schema.json` sibling its run wrote, so the sibling checks have something
/// to check.
async fn clean_backup() -> &'static Path {
    BACKUP
        .get_or_init(|| async {
            // seed one of everything, every value class of every table, and a profile icon
            let harness = Harness::get().await;
            fixture::broad(harness).await;
            let _ = Box::pin(fixture::value_classes(harness)).await;
            common::seed::icon(&harness.api_url, &harness.token).await;
            let out = harness.scratch("clean");
            let taken = harness
                .thoradm()
                .args(["--workers", "4", "backup", "new", "-m", "4", "-o"])
                .arg(&out)
                .output()
                .expect("failed to run thoradm");
            assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
            out
        })
        .await
}

/// Describes a finished run for an assertion message
///
/// # Arguments
///
/// * `output` - The run's output
fn describe(output: &Output) -> String {
    // the exit code and stderr
    format!(
        "exit {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Copies a directory tree
///
/// # Arguments
///
/// * `from` - The directory to copy
/// * `to` - Where to put the copy
fn copy_dir(from: &Path, to: &Path) {
    // copy every file and directory below the root
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Returns a fresh copy of the clean backup, without the reports earlier runs wrote
///
/// # Arguments
///
/// * `name` - The copy's name
async fn copy_of_clean(name: &str) -> PathBuf {
    // copy the backup into this test's scratch directory
    let harness = Harness::get().await;
    let copy = harness.scratch(name).join("backup");
    copy_dir(clean_backup().await, &copy);
    let _ = std::fs::remove_dir_all(copy.join("reports"));
    copy
}

/// Removes a copy once its checks passed, since copies of the fixture backup are large
///
/// # Arguments
///
/// * `copy` - The copy `copy_of_clean` made
fn discard(copy: &Path) {
    // the copy and its reports sit in the test's scratch directory
    let _ = std::fs::remove_dir_all(copy.parent().unwrap_or(copy));
}

/// What one verify or scrub run did
struct Run {
    /// The process output
    output: Output,
    /// The records its failures file holds
    records: Vec<FailureRecord>,
    /// Its `verify.json`, when it wrote one
    report: Option<Value>,
}

impl Run {
    /// Describes the run for an assertion message
    fn describe(&self) -> String {
        // the exit code, the records, and stderr
        format!(
            "{}\nrecords: {:#?}",
            describe(&self.output),
            self.records
                .iter()
                .map(|record| format!(
                    "{} {:?} {:?} {:?}: {}",
                    record.component, record.table, record.key, record.object, record.error
                ))
                .collect::<Vec<_>>()
        )
    }

    /// Returns whether some record names a path or object containing the given text
    ///
    /// # Arguments
    ///
    /// * `text` - The text a record's key, object, or error must contain
    fn names(&self, text: &str) -> bool {
        // look in every field that names the object
        self.records.iter().any(|record| {
            record.key.as_deref().is_some_and(|key| key.contains(text))
                || record
                    .object
                    .as_deref()
                    .is_some_and(|object| object.contains(text))
                || record.error.contains(text)
        })
    }
}

/// Runs `backup verify` or `backup scrub` on a backup with its report in a fresh directory
///
/// # Arguments
///
/// * `command` - `verify` or `scrub`
/// * `backup` - The backup to check
/// * `extra` - Extra arguments, after the command
async fn check(command: &str, backup: &Path, extra: &[&str]) -> Run {
    // scrub writes its report under the backup; verify takes a report directory
    let harness = Harness::get().await;
    let report_dir = backup
        .parent()
        .expect("the backup has a parent")
        .join(format!("{command}-report-{}", uuid::Uuid::new_v4()));
    let mut cmd = harness.thoradm();
    let workers = if extra.contains(&"--workers1") {
        "1"
    } else {
        "4"
    };
    cmd.args(["--workers", workers, "backup", command, "-b"])
        .arg(backup);
    if command == "verify" {
        cmd.arg("--report-dir").arg(&report_dir);
    }
    cmd.args(extra.iter().filter(|arg| **arg != "--workers1"));
    let output = cmd.output().expect("failed to run thoradm");
    // find the failures file the run wrote
    let failures = if command == "verify" {
        report_dir.join(FAILURES_FILE)
    } else {
        newest_report(backup, "scrub-").join(FAILURES_FILE)
    };
    let records = read_records(&failures).unwrap_or_default();
    let report = std::fs::read(report_dir.join("verify.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    Run {
        output,
        records,
        report,
    }
}

/// Returns the newest report directory of an operation under a backup
///
/// # Arguments
///
/// * `backup` - The backup
/// * `prefix` - The report directory's prefix (`scrub-`)
fn newest_report(backup: &Path, prefix: &str) -> PathBuf {
    // report directories are named by the time their run started
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(backup.join("reports")).map_or_else(
        |_| Vec::new(),
        |listing| {
            listing
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
                })
                .collect()
        },
    );
    dirs.sort();
    dirs.pop()
        .unwrap_or_else(|| backup.join("reports").join("none"))
}

/// Returns every table component the registry backs up, as runs name them
fn table_components() -> BTreeSet<String> {
    // each backed-up table's component
    COVERAGE
        .iter()
        .filter(|entry| entry.store == Store::ScyllaTable)
        .filter_map(|entry| match entry.coverage {
            Coverage::BackedUp(component) => Some(component.to_string()),
            Coverage::Excluded { .. } => None,
        })
        .collect()
}

/// Returns the map and data file of the first archive of a table that holds a partition
///
/// # Arguments
///
/// * `backup` - The backup
/// * `table` - The table
async fn archive_of(backup: &Path, table: &str) -> (PathBuf, PathBuf) {
    // the manifest lists every archive with its entry count
    let manifest = Manifest::read(backup).await.unwrap().expect("no manifest");
    let archive = manifest.tables[table]
        .archives
        .iter()
        .find(|archive| archive.entries > 0)
        .unwrap_or_else(|| panic!("{table} has no archive holding a partition"));
    let dir = backup.join(table);
    (
        dir.join("maps")
            .join(format!("{}.thoriummap", archive.uuid)),
        dir.join("data").join(archive.uuid.to_string()),
    )
}

/// Returns the path of the first non-empty object an S3 component's index lists
///
/// An empty object can't be truncated or have a byte flipped, so a test that damages the object
/// it gets back needs one with content.
///
/// # Arguments
///
/// * `backup` - The backup
/// * `table` - The table directory holding the objects
fn first_object(backup: &Path, table: &str) -> PathBuf {
    // walk the objects directory to its first file that holds bytes
    let mut pending = vec![backup.join(table).join("objects")];
    while let Some(dir) = pending.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        if let Some(file) = entries
            .iter()
            .find(|path| path.is_file() && std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0))
        {
            return file.clone();
        }
        // only directories are walked further; an empty file is skipped
        pending.extend(entries.into_iter().rev().filter(|path| path.is_dir()));
    }
    panic!("{table} holds no objects")
}

/// A clean backup verifies with exit 0, every table, S3 component, `redis.json`, and
/// `schema.json` checked; scrub checks every table's partitions too
#[tokio::test]
async fn a_clean_backup_verifies_with_every_table_checked() {
    // verify the clean backup
    let backup = copy_of_clean("clean-verify").await;
    let run = check("verify", &backup, &[]).await;
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    assert!(run.records.is_empty(), "{}", run.describe());
    // the report names the backup as complete, in this thoradm's format, and every check passed
    let report = run.report.as_ref().expect("verify wrote no verify.json");
    assert_eq!(report["mode"], "backup", "{report:#}");
    assert_eq!(report["complete_backup"], true, "{report:#}");
    assert_eq!(
        report["format_version"],
        thoradm::backup::manifest::FORMAT_VERSION,
        "{report:#}"
    );
    assert_eq!(report["stopped_early"], false, "{report:#}");
    let checks = report["checks"].as_array().expect("no checks");
    assert!(
        checks.iter().all(|check| check["failed"] == 0),
        "{report:#}"
    );
    // every partition of every one of the 15 tables was checked
    let partitioned: BTreeSet<String> = checks
        .iter()
        .filter(|check| check["check"] == "partition-sha256" && check["checked"].as_u64() > Some(0))
        .map(|check| check["component"].as_str().unwrap().to_owned())
        .collect();
    let tables = table_components();
    assert_eq!(tables.len(), 15, "{tables:?}");
    assert_eq!(partitioned, tables, "{report:#}");
    // every S3 component, redis.json, and the schema.json sibling were checked
    let components: BTreeSet<&str> = checks
        .iter()
        .filter(|check| check["checked"].as_u64() > Some(0))
        .map(|check| check["component"].as_str().unwrap())
        .collect();
    for component in [
        "s3-ids-objects",
        "comment-attachments",
        "result-files",
        "graphics",
        "redis",
        "siblings",
    ] {
        assert!(
            components.contains(component),
            "{component} unchecked: {report:#}"
        );
    }
    // scrub checks the partitions of every table, and exits 0
    let scrub = check("scrub", &backup, &[]).await;
    assert_eq!(scrub.output.status.code(), Some(0), "{}", scrub.describe());
    let stderr = String::from_utf8_lossy(&scrub.output.stderr);
    for table in &tables {
        assert!(
            stderr
                .lines()
                .any(|line| line.contains(&format!("{table}: "))
                    && line.contains(" handled")
                    && !line.contains(": 0 handled")),
            "scrub didn't check {table}:\n{stderr}"
        );
    }
    discard(&backup);
}

/// Damage to every kind of file a backup holds is recorded naming the damaged file or object,
/// and a missing manifest stops verify with exit 1
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn each_kind_of_damage_is_recorded_naming_what_is_damaged() {
    // a clean copy passes
    let clean = copy_of_clean("damage-clean").await;
    let run = check("verify", &clean, &[]).await;
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    discard(&clean);
    // an interrupted backup: a marker and no manifest
    let backup = copy_of_clean("damage-interrupted").await;
    corrupt::delete(&backup.join(MANIFEST_FILE)).unwrap();
    InProgressMarker::new(chrono::Utc::now(), Vec::new())
        .write(&backup, &TokioFs)
        .await
        .unwrap();
    let run = check("verify", &backup, &[]).await;
    assert_eq!(run.output.status.code(), Some(1), "{}", run.describe());
    assert!(run.names("interrupted"), "{}", run.describe());
    discard(&backup);
    // a legacy backup: neither file
    let backup = copy_of_clean("damage-legacy").await;
    corrupt::delete(&backup.join(MANIFEST_FILE)).unwrap();
    let run = check("verify", &backup, &[]).await;
    assert_eq!(run.output.status.code(), Some(1), "{}", run.describe());
    assert!(run.names(MANIFEST_FILE), "{}", run.describe());
    assert_eq!(
        run.report
            .as_ref()
            .map(|report| report["complete_backup"].clone()),
        Some(Value::Bool(false)),
        "{}",
        run.describe()
    );
    discard(&backup);
    // a map trimmed in the middle of an entry
    let backup = copy_of_clean("damage-map").await;
    let (map, _) = archive_of(&backup, "samples_list").await;
    corrupt::truncate_by(&map, 40).unwrap();
    expect_recorded(&backup, &map).await;
    // a data file shorter than its last partition's end
    let backup = copy_of_clean("damage-data").await;
    let (_, data) = archive_of(&backup, "tags").await;
    corrupt::truncate_by(&data, 1).unwrap();
    expect_recorded(&backup, &data).await;
    // one flipped byte inside a partition
    let backup = copy_of_clean("damage-flip").await;
    let (map, data) = archive_of(&backup, "entities").await;
    corrupt::flip_byte(&data, 0).unwrap();
    expect_recorded(&backup, &map).await;
    // an archive the manifest lists, deleted
    let backup = copy_of_clean("damage-deleted").await;
    let (map, _) = archive_of(&backup, "notifications").await;
    corrupt::delete(&map).unwrap();
    expect_recorded(&backup, &map).await;
    // an archive the manifest doesn't list, added beside the others
    let backup = copy_of_clean("damage-unlisted").await;
    let (map, data) = archive_of(&backup, "associations").await;
    let stranger = uuid::Uuid::new_v4().to_string();
    let added = map.with_file_name(format!("{stranger}.thoriummap"));
    std::fs::copy(&map, &added).unwrap();
    std::fs::copy(&data, data.with_file_name(&stranger)).unwrap();
    expect_recorded(&backup, &added).await;
    // an S3 object deleted, truncated, given a flipped byte, or added
    for (name, table) in [
        ("object-deleted", "s3_ids"),
        ("object-truncated", "comments"),
        ("object-flipped", "results"),
        ("object-added", "results"),
    ] {
        let backup = copy_of_clean(name).await;
        let object = first_object(&backup, table);
        let named = match name {
            "object-deleted" => {
                corrupt::delete(&object).unwrap();
                object
            }
            "object-truncated" => {
                corrupt::truncate_by(&object, 1).unwrap();
                object
            }
            "object-flipped" => {
                corrupt::flip_byte(&object, 0).unwrap();
                object
            }
            _ => {
                let added = object.with_file_name("stowaway.bin");
                corrupt::add_file(&added, b"no row names this").unwrap();
                added
            }
        };
        expect_recorded(&backup, &named).await;
    }
    // an object index edited
    let backup = copy_of_clean("damage-index").await;
    let index = backup.join("s3_ids").join("objects.index.jsonl");
    let text = std::fs::read_to_string(&index).unwrap();
    let edited = text.replacen("\"size\":", "\"size\":1", 1);
    assert_ne!(text, edited);
    std::fs::write(&index, edited).unwrap();
    expect_recorded(&backup, &index).await;
    // redis.json with a flipped byte, truncated, or with a record removed
    for name in ["redis-flipped", "redis-truncated", "redis-removed"] {
        let backup = copy_of_clean(name).await;
        let redis = backup.join("redis.json");
        match name {
            "redis-flipped" => corrupt::flip_byte(&redis, 3).unwrap(),
            "redis-truncated" => corrupt::truncate_by(&redis, 10).unwrap(),
            _ => {
                let mut doc: Value =
                    serde_json::from_slice(&std::fs::read(&redis).unwrap()).unwrap();
                doc["users"].as_array_mut().unwrap().pop();
                std::fs::write(&redis, serde_json::to_vec(&doc).unwrap()).unwrap();
            }
        }
        expect_recorded(&backup, &redis).await;
    }
    // schema.json changed
    let backup = copy_of_clean("damage-schema").await;
    let schema = backup.join("schema.json");
    std::fs::write(&schema, br#"{"revision": "2026-10-v02"}"#).unwrap();
    expect_recorded(&backup, &schema).await;
    // a manifest that records a failure, listing the backup's record
    let backup = copy_of_clean("damage-recorded").await;
    let record = FailureRecord::failed(
        "result-files",
        thoradm::backup::failures::Operation::Download,
        "the object went missing",
    )
    .bucket("testing-results")
    .key("a-result/out.bin");
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    std::fs::write(backup.join(FAILURES_FILE), &line).unwrap();
    let sha = thoradm::backup::manifest::file_sha256(&backup.join(FAILURES_FILE))
        .await
        .unwrap();
    corrupt::edit_json(&backup.join(MANIFEST_FILE), "/failures", Value::from(1)).unwrap();
    corrupt::edit_json(
        &backup.join(MANIFEST_FILE),
        "/failures_sha256",
        Value::from(sha),
    )
    .unwrap();
    let run = check("verify", &backup, &[]).await;
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    assert!(run.names("a-result/out.bin"), "{}", run.describe());
    discard(&backup);
}

/// Checks that verify records a damaged file or object, naming it, and exits 2
///
/// # Arguments
///
/// * `backup` - The damaged backup
/// * `damaged` - The file or object that was damaged
async fn expect_recorded(backup: &Path, damaged: &Path) {
    // the record names the damaged path relative to the backup
    let run = check("verify", backup, &[]).await;
    let relative = damaged
        .strip_prefix(backup)
        .unwrap_or(damaged)
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        run.output.status.code(),
        Some(2),
        "{relative}: {}",
        run.describe()
    );
    assert!(
        run.names(&relative),
        "no record names {relative}: {}",
        run.describe()
    );
    discard(backup);
}

/// Under `--abort-on-failure` with one worker, verify and scrub stop at the first of two damaged
/// tables with exactly one record and exit 1; without it, both record both and exit 2
#[tokio::test]
async fn abort_on_failure_stops_verify_and_scrub_at_the_first_damage() {
    // flip a byte in two tables' archives
    let backup = copy_of_clean("abort-two").await;
    for table in ["samples_list", "tags"] {
        let (_, data) = archive_of(&backup, table).await;
        corrupt::flip_byte(&data, 0).unwrap();
    }
    // verify and scrub stop at the first with one record
    let verify = check("verify", &backup, &["--workers1", "--abort-on-failure"]).await;
    assert_eq!(
        verify.output.status.code(),
        Some(1),
        "{}",
        verify.describe()
    );
    assert_eq!(verify.records.len(), 1, "{}", verify.describe());
    let report = verify.report.as_ref().expect("verify wrote no verify.json");
    assert_eq!(report["stopped_early"], true, "{report:#}");
    let scrub = check("scrub", &backup, &["--workers1", "--abort-on-failure"]).await;
    assert_eq!(scrub.output.status.code(), Some(1), "{}", scrub.describe());
    assert_eq!(scrub.records.len(), 1, "{}", scrub.describe());
    // without the flag both record both and exit 2
    let verify = check("verify", &backup, &["--workers1"]).await;
    assert_eq!(
        verify.output.status.code(),
        Some(2),
        "{}",
        verify.describe()
    );
    assert_eq!(verify.records.len(), 2, "{}", verify.describe());
    let scrub = check("scrub", &backup, &["--workers1"]).await;
    assert_eq!(scrub.output.status.code(), Some(2), "{}", scrub.describe());
    assert_eq!(scrub.records.len(), 2, "{}", scrub.describe());
    discard(&backup);
}

/// A manifest of a newer format version fails the manifest check, naming the version: verify
/// exits 1 with a record in its report directory and writes nothing into the backup
#[tokio::test]
async fn verify_refuses_a_newer_format() {
    // a manifest one format version past this thoradm's
    let backup = copy_of_clean("newer").await;
    let newer = thoradm::backup::manifest::FORMAT_VERSION + 1;
    corrupt::edit_json(
        &backup.join(MANIFEST_FILE),
        "/format_version",
        Value::from(newer),
    )
    .unwrap();
    let run = check("verify", &backup, &[]).await;
    assert_eq!(run.output.status.code(), Some(1), "{}", run.describe());
    assert!(
        run.names(&format!("format version {newer}")),
        "{}",
        run.describe()
    );
    // nothing was written into the backup
    assert!(!backup.join("reports").exists(), "{}", run.describe());
    assert!(!backup.join(MARKER_FILE).exists());
    discard(&backup);
}
