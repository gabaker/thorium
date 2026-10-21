//! What a backup run does with its output directory: a fresh backup copies every object, a
//! finished backup is never touched again, an interrupted one is resumed, and unrelated files are
//! never mixed in

mod common;

use axum::http::Method;
use common::archives;
use common::harness::{Harness, list_keys};
use common::kill;
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::names::Names;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use thoradm::args::{BackupComponents, NewBackup};
use thoradm::backup::controllers::backup;
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, Operation, read_records};
use thoradm::backup::manifest::{LOCK_FILE, MANIFEST_FILE, MARKER_FILE, Manifest};
use thoradm::backup::sink::TokioFs;
use thoradm::backup::tables::{Comment, Output as ResultRow, SamplesList};
use thoradm::outcome::RunOutcome;
use thorium::CtlConf;
use thorium::Thorium;
use thorium::client::ResultsClient;
use thorium::models::{
    Buffer, CommentRequest, OnDiskFile, OutputDisplayType, OutputRequest, SampleRequest,
};

/// Held by each test, so one test's seeding never lands between another's backups
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The components these tests back up: the tables that name objects, and the objects
const COMPONENTS: &str =
    "samples-list,s3-ids,s3-ids-objects,comments,comment-attachments,results,result-files";

/// Uploads a sample with a comment and attachment and a result with two files
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `client` - An admin client
async fn seed(harness: &Harness, client: &Thorium) {
    // a group and a sample in it
    let names = Names::new();
    let group = names.group("minerva");
    common::seed::group(client, &group).await;
    let data = Buffer::new(format!("a sample for {}", names.suffix).into_bytes()).name("owl.bin");
    let sample = client
        .files
        .create(SampleRequest::new_buffer(data, vec![group.as_str()]))
        .await
        .unwrap()
        .sha256;
    // a comment with an attachment
    let scratch = harness.scratch(&format!("seed-{}", names.suffix));
    let attachment = scratch.join("notes.txt");
    std::fs::write(&attachment, b"an attached note").unwrap();
    let comment = CommentRequest::new(&sample, "an owl was seen")
        .file(OnDiskFile::new(&attachment).trim_prefix(&scratch));
    client.files.comment(comment).await.unwrap();
    // a result with two files
    let result = OutputRequest::new(
        sample.clone(),
        "owl-tool",
        "an owl result",
        OutputDisplayType::String,
    )
    .buffer(Buffer::new("result file").name("out.txt"))
    .buffer(Buffer::new("nested result file").name("nested/out.txt"));
    client.files.create_result(result).await.unwrap();
}

/// Runs `thoradm backup new` with the test components into a directory
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `out` - The backup directory
fn backup(harness: &Harness, out: &Path) -> Output {
    // a small worker count keeps the run quick
    harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            COMPONENTS,
            "-m",
            "2",
            "-o",
        ])
        .arg(out)
        .output()
        .expect("failed to run thoradm")
}

/// Returns every file under a directory, relative to it, with its bytes
///
/// # Arguments
///
/// * `dir` - The directory to walk
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    // walk depth first and keep every regular file
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in listing.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.insert(path.strip_prefix(dir).unwrap().to_path_buf(), bytes);
            }
        }
    }
    files
}

/// Asserts a backup holds a file for every object of a bucket, under a component's objects
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `out` - The backup directory
/// * `table` - The table directory the component writes objects under
/// * `bucket` - The bucket the objects live in
async fn assert_every_object(harness: &Harness, out: &Path, table: &str, bucket: &str) {
    // every key in the bucket ends one of the backed-up files' paths
    let keys = list_keys(&harness.s3(), bucket).await.unwrap();
    assert!(!keys.is_empty(), "{bucket} holds no objects");
    let files: Vec<String> = tree(&out.join(table).join("objects"))
        .into_keys()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    for key in keys {
        assert!(
            files.iter().any(|file| file.ends_with(&key)),
            "{key} of {bucket} isn't in the backup: {files:?}"
        );
    }
}

/// Returns how many rows each table's archives hold
///
/// # Arguments
///
/// * `out` - The backup directory
async fn row_counts(out: &Path) -> (usize, usize, usize) {
    // count the rows of the three tables the test components archive
    let samples = archives::rows::<SamplesList, _, _>(&out.join("samples_list"), |_| ()).await;
    let comments = archives::rows::<Comment, _, _>(&out.join("comments"), |_| ()).await;
    let results = archives::rows::<ResultRow, _, _>(&out.join("results"), |_| ()).await;
    (samples.len(), comments.len(), results.len())
}

/// A fresh backup copies every comment attachment and result file, and a second run into the
/// finished directory is refused without changing anything in it
#[tokio::test]
async fn a_fresh_backup_copies_every_object_and_a_finished_one_is_never_changed() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let client = harness.client().await;
    seed(harness, &client).await;
    // a fresh backup completes and holds every attachment and result file
    let out = harness.scratch("fresh");
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let attachments = harness.conf.thorium.attachments.bucket.clone();
    let results = harness.conf.thorium.results.bucket.clone();
    assert_every_object(harness, &out, "comments", &attachments).await;
    assert_every_object(harness, &out, "results", &results).await;
    // a second run into the finished backup is refused and leaves every file as it was
    let before = tree(&out);
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("finished backup"), "{stderr}");
    assert_eq!(tree(&out), before, "the finished backup changed");
}

/// A directory holding files of no backup is refused without changing anything in it
#[tokio::test]
async fn a_directory_of_unrelated_files_is_refused() {
    // take this binary's turn at comparing backups in the shared namespace
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // a directory holding a file no backup wrote
    let out = harness.scratch("unrelated");
    std::fs::write(out.join("holiday.jpg"), b"not a backup").unwrap();
    let before = tree(&out);
    // the backup refuses it with exit 1, naming what the directory lacks
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("neither"), "{stderr}");
    // and leaves it exactly as it was
    assert_eq!(tree(&out), before, "the unrelated directory changed");
}

/// A directory another run is backing up into is refused with exit 1, naming the other run, and
/// left as it was
#[tokio::test]
async fn a_directory_another_run_is_writing_is_refused() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // another run holds the lock on the directory it is backing up into
    let out = harness.scratch("locked");
    let held = std::fs::File::create(out.join(LOCK_FILE)).unwrap();
    held.try_lock().unwrap();
    let before = tree(&out);
    // the backup refuses it with exit 1, naming the other run
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("another thoradm run"), "{stderr}");
    // and writes nothing into it
    assert_eq!(tree(&out), before, "the locked directory changed");
    assert!(!out.join(MARKER_FILE).exists());
    drop(held);
}

/// A lock file whose run has ended doesn't stop a backup into its directory: the next run takes
/// the lock and finishes
#[tokio::test]
async fn a_lock_file_an_ended_run_left_does_not_stop_a_backup() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // a run that ended before writing anything else left its unlocked lock file
    let out = harness.scratch("left-lock");
    std::fs::write(out.join(LOCK_FILE), b"").unwrap();
    // the backup takes the lock and finishes
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(out.join(MANIFEST_FILE).is_file());
}

/// A backup killed after its tables, run again into its directory, finishes with exactly one set
/// of archives per table and every object
#[tokio::test]
async fn an_interrupted_backup_resumes_with_one_set_of_archives() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let client = harness.client().await;
    seed(harness, &client).await;
    // kill a backup once its tables are done and its first S3 component starts
    let out = harness.scratch("interrupted");
    let mut command = harness.thoradm();
    command
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            COMPONENTS,
            "-m",
            "2",
            "-o",
        ])
        .arg(&out);
    let killed = kill::run_until(command, "Backing up the objects of")
        .await
        .expect("failed to run thoradm");
    assert!(
        killed.killed,
        "the backup finished before it could be killed"
    );
    assert!(out.join("backup-in-progress.json").is_file());
    assert!(!out.join("manifest.json").exists());
    // running it again resumes into the directory and finishes
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(out.join("manifest.json").is_file());
    assert!(!out.join("backup-in-progress.json").exists());
    // each table holds the rows of one scan: the same counts as a backup that ran straight
    // through
    let reference = harness.scratch("interrupted-reference");
    let output = backup(harness, &reference);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(row_counts(&out).await, row_counts(&reference).await);
    // every object is there
    let attachments = harness.conf.thorium.attachments.bucket.clone();
    let results = harness.conf.thorium.results.bucket.clone();
    assert_every_object(harness, &out, "comments", &attachments).await;
    assert_every_object(harness, &out, "results", &results).await;
}

/// Returns the records of every failures file under a backup's `reports/backup-*` directories
///
/// # Arguments
///
/// * `out` - The backup directory
fn moved_records(out: &Path) -> Vec<FailureRecord> {
    // each interrupted run's failures file moves to a report directory named for its start time
    let Ok(reports) = std::fs::read_dir(out.join("reports")) else {
        return Vec::new();
    };
    reports
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("backup-"))
        .map(|entry| entry.path().join(FAILURES_FILE))
        .filter(|path| path.is_file())
        .flat_map(|path| read_records(&path).unwrap())
        .collect()
}

/// A backup killed part way leaves its marker and no manifest; restore refuses it, and running
/// the same backup again finishes it with every table's rows
#[tokio::test]
async fn a_killed_backup_is_refused_by_restore_and_finished_by_a_rerun() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    // a refusal that failed would restore, so keep every other test of the binary out
    let _guard = harness.restore_guard().await;
    let client = harness.client().await;
    seed(harness, &client).await;
    // kill a backup once its second table is done
    let out = harness.scratch("killed");
    let mut command = harness.thoradm();
    command
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            COMPONENTS,
            "-m",
            "2",
            "-o",
        ])
        .arg(&out);
    let killed = kill::run_until(command, "comments: ")
        .await
        .expect("failed to run thoradm");
    assert!(
        killed.killed,
        "the backup finished before it could be killed"
    );
    assert!(out.join(MARKER_FILE).is_file());
    assert!(!out.join(MANIFEST_FILE).exists());
    // restore refuses the interrupted backup with a preflight record, changing nothing
    let refused = harness
        .thoradm()
        .args(["backup", "restore", "--yes", "-b"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(1), "{stderr}");
    let records = common::cycle::failure_records(&out);
    assert!(
        records
            .iter()
            .any(|record| record.operation == Operation::Preflight
                && record.error.contains("interrupted")),
        "{records:?}"
    );
    // verify reports it as interrupted and can't go on, exiting 1
    let verified = verify(harness, &out);
    let stderr = String::from_utf8_lossy(&verified.stderr);
    assert_eq!(verified.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("interrupted"), "{stderr}");
    // running the same backup again finishes it
    let output = backup(harness, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!out.join(MARKER_FILE).exists());
    // and verify finds the finished backup whole
    let verified = verify(harness, &out);
    let stderr = String::from_utf8_lossy(&verified.stderr);
    assert_eq!(verified.status.code(), Some(0), "{stderr}");
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    assert_eq!((manifest.failures, manifest.skipped), (0, 0));
    // the finished backup holds the rows its manifest lists, once
    let (samples, comments, results) = row_counts(&out).await;
    let listed = |table: &str| {
        usize::try_from(manifest.tables.get(table).map_or(0, |entry| entry.rows)).unwrap()
    };
    assert_eq!(samples, listed("samples_list"));
    assert_eq!(comments, listed("comments"));
    assert_eq!(results, listed("results"));
}

/// Verifies a backup directory, with the report in its default place inside the backup
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `out` - The backup to verify
fn verify(harness: &Harness, out: &Path) -> Output {
    // the default report directory is under the backup's reports/
    harness
        .thoradm()
        .args(["backup", "verify", "-b"])
        .arg(out)
        .output()
        .expect("failed to run thoradm")
}

/// Starts a `MockS3` that answers 404 for one sample's object, returning every sample object's
/// key, the missing one, the mock, and a config that uses it
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn missing_object(harness: &Harness) -> (Vec<String>, String, MockS3, thorium::Conf) {
    // pick the first object of the files bucket
    let s3 = harness.s3();
    let files = list_keys(&s3, &harness.conf.thorium.files.bucket)
        .await
        .unwrap();
    let missing = files.first().expect("no sample objects").clone();
    // point a copy of the config at a mock that refuses that object
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
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
    (files, missing, mock, conf)
}

/// A backup stopped by `--abort-on-failure` at a missing object keeps its marker; once the fault
/// clears, running it again resumes, keeps the objects that landed, finishes, and moves the
/// stopped run's records under `reports/`
#[tokio::test]
async fn a_backup_stopped_at_a_failure_resumes_and_keeps_what_landed() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let client = harness.client().await;
    for _ in 0..4 {
        seed(harness, &client).await;
    }
    // one sample's object answers 404 for as long as the rule is set
    let (files, missing, mock, conf) = missing_object(harness).await;
    // back up the samples' objects, stopping at the first failure
    let out = harness.scratch("stopped");
    let scylla = harness.scylla().await;
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let mut args = NewBackup {
        components: vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
        output: out.clone(),
        multiplier: 2,
        retries: 2,
        s3_concurrency: 1,
        abort_on_failure: true,
        allow_version_mismatch: false,
        prune_dangling: false,
    };
    let outcome = Box::pin(backup::run(
        &conf,
        ctl_conf.clone(),
        &scylla,
        1,
        &args,
        Arc::new(TokioFs),
    ))
    .await
    .expect("the backup failed to run");
    // the run stopped with the object recorded, its marker kept, and no manifest
    assert_eq!(outcome, RunOutcome::Aborted);
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.key.as_deref() == Some(missing.as_str())),
        "{records:?}"
    );
    assert!(out.join(MARKER_FILE).is_file());
    assert!(!out.join(MANIFEST_FILE).exists());
    // the objects that landed before it stopped
    let landed: Vec<String> = files
        .iter()
        .filter(|key| {
            out.join("s3_ids")
                .join("objects")
                .join("files")
                .join(&key[..6])
                .join(key)
                .is_file()
        })
        .cloned()
        .collect();
    // once the fault clears, running it again resumes and finishes
    mock.set_rules(Vec::new()).await;
    mock.clear_seen().await;
    args.abort_on_failure = false;
    let outcome = Box::pin(backup::run(
        &conf,
        ctl_conf,
        &scylla,
        1,
        &args,
        Arc::new(TokioFs),
    ))
    .await
    .expect("the backup failed to run");
    assert_eq!(outcome, RunOutcome::Complete);
    assert!(out.join(MANIFEST_FILE).is_file());
    assert!(!out.join(MARKER_FILE).exists());
    // the objects that had landed weren't downloaded again; the missing one was
    let fetched: Vec<String> = mock
        .seen()
        .await
        .into_iter()
        .filter(|request| request.method == Method::GET)
        .map(|request| request.key)
        .collect();
    assert!(fetched.contains(&missing), "{fetched:?}");
    for key in &landed {
        assert!(!fetched.contains(key), "{key} was downloaded again");
    }
    // the backup's own failures file holds nothing; the stopped run's records moved
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(records.is_empty(), "{records:?}");
    let moved = moved_records(&out);
    assert!(
        moved
            .iter()
            .any(|record| record.key.as_deref() == Some(missing.as_str())),
        "{moved:?}"
    );
    assert_eq!(Manifest::read(&out).await.unwrap().unwrap().failures, 0);
}
