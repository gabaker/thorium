//! S3 downloads during a backup land whole or not at all: transient faults are retried,
//! permanent ones and disk faults are recorded with no file left behind, a file an earlier run
//! left is checked before it is kept, and many objects of one partition stay under a low
//! descriptor limit

mod common;

use aws_sdk_s3::primitives::ByteStream;
use axum::http::Method;
use common::cycle::{describe, verify};
use common::faulty_sink::{FaultySink, SinkCall, SinkFault};
use common::harness::Harness;
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::names::Names;
use data_encoding::HEXLOWER;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thoradm::args::{BackupComponents, NewBackup};
use thoradm::backup::controllers::backup;
use thoradm::backup::failures::{
    FAILURES_FILE, FailureKind, FailureRecord, Operation, read_records,
};
use thoradm::backup::manifest::{InProgressMarker, Manifest, ObjectIndexLine};
use thoradm::backup::sink::{SinkFactory, TokioFs};
use thoradm::backup::{STAGING_DIR, staging_name};
use thoradm::outcome::RunOutcome;
use thorium::{Conf, CtlConf};
use uuid::Uuid;

/// How many attempts the backups in these tests make per object
const RETRIES: u32 = 3;

/// One object seeded into the files bucket with its `s3_ids` row
#[derive(Debug, Clone)]
struct Seeded {
    /// The object's key, its s3 id
    key: String,
    /// The object's bytes
    bytes: Vec<u8>,
}

/// Returns the lowercase hex sha256 of some bytes
///
/// # Arguments
///
/// * `bytes` - The bytes to hash
fn sha256(bytes: &[u8]) -> String {
    // hash and encode
    HEXLOWER.encode(&Sha256::digest(bytes))
}

/// Seeds one object of a given size in the files bucket and then its `s3_ids` row
///
/// The object goes in before its row, so a backup running beside this test never finds a row
/// whose object is missing.
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `size` - The object's size in bytes
async fn seed_object(harness: &Harness, size: usize) -> Seeded {
    // random bytes under a fresh s3 id
    let mut bytes = vec![0_u8; size];
    rand::rng().fill_bytes(&mut bytes);
    let key = Uuid::new_v4().to_string();
    // upload the object
    harness
        .s3()
        .put_object()
        .bucket(&harness.conf.thorium.files.bucket)
        .key(&key)
        .body(ByteStream::from(bytes.clone()))
        .send()
        .await
        .expect("failed to upload an object");
    // add the row that names it
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    scylla
        .query_unpaged(
            format!("INSERT INTO {ns}.s3_ids (type, id, sha256) VALUES ('File', ?, ?)"),
            (Uuid::parse_str(&key).unwrap(), sha256(&bytes)),
        )
        .await
        .expect("failed to insert an s3 id row");
    Seeded { key, bytes }
}

/// Returns where a backup writes a files-bucket object
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `key` - The object's key
fn object_path(out: &Path, key: &str) -> PathBuf {
    // objects are spread over subdirectories named by the key's first six characters
    out.join("s3_ids")
        .join("objects")
        .join("files")
        .join(&key[..6])
        .join(key)
}

/// Returns the staging directory of the s3 ids objects, where downloads are written until they
/// are complete
///
/// # Arguments
///
/// * `out` - The backup directory
fn staging_dir(out: &Path) -> PathBuf {
    // the staging directory sits beside objects/ in the component's directory
    out.join("s3_ids").join(STAGING_DIR)
}

/// Backs up the s3 ids table and its objects in-process
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `conf` - The config to back up with, which may point S3 at a `MockS3`
/// * `out` - The backup directory
/// * `sinks` - The file-system seam the backup writes through
async fn backup_objects(
    harness: &Harness,
    conf: &Conf,
    out: &Path,
    sinks: Arc<dyn SinkFactory>,
) -> RunOutcome {
    // run to the end, recording failures as they come
    Box::pin(backup_objects_with(harness, conf, out, sinks, false)).await
}

/// Backs up the s3 ids table and its objects in-process, optionally stopping at the first failure
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `conf` - The config to back up with, which may point S3 at a `MockS3`
/// * `out` - The backup directory
/// * `sinks` - The file-system seam the backup writes through
/// * `abort_on_failure` - Whether the run stops at its first failure, as `--abort-on-failure` does
async fn backup_objects_with(
    harness: &Harness,
    conf: &Conf,
    out: &Path,
    sinks: Arc<dyn SinkFactory>,
    abort_on_failure: bool,
) -> RunOutcome {
    // read through the harness's Scylla and write through the given seam
    let scylla = harness.scylla().await;
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).expect("failed to load the ctl config");
    let args = NewBackup {
        components: vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
        output: out.to_path_buf(),
        multiplier: 2,
        retries: RETRIES,
        s3_concurrency: 64,
        abort_on_failure,
        allow_version_mismatch: false,
        prune_dangling: false,
    };
    Box::pin(backup::run(conf, ctl_conf, &scylla, 2, &args, sinks))
        .await
        .expect("the backup failed to run")
}

/// Starts a `MockS3` in front of the harness's S3 and returns it with a config that uses it
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn mocked(harness: &Harness) -> (MockS3, Conf) {
    // forward to the real S3 and point a copy of the config at the mock
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    (mock, conf)
}

/// Builds a rule for every GET of one key
///
/// # Arguments
///
/// * `key` - The object key
/// * `nth` - The match to apply it to, or `None` for every match
/// * `behavior` - What to do to the request
fn get_rule(key: &str, nth: Option<u64>, behavior: S3Behavior) -> S3Rule {
    // match downloads of exactly this key
    S3Rule {
        method: Method::GET,
        key: key.to_owned(),
        query: None,
        nth,
        behavior,
    }
}

/// Asserts an object landed at its final path with exactly its bytes and no partial file
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `object` - The seeded object
fn assert_landed(out: &Path, object: &Seeded) {
    // the file holds the object's bytes
    let path = object_path(out, &object.key);
    let found =
        std::fs::read(&path).unwrap_or_else(|err| panic!("{} wasn't backed up: {err}", object.key));
    assert_eq!(
        found.len(),
        object.bytes.len(),
        "{} has the wrong length",
        object.key
    );
    assert_eq!(
        sha256(&found),
        sha256(&object.bytes),
        "{} has other bytes",
        object.key
    );
    // nothing was left half written: a finished component removes its staging directory
    assert!(
        !staging_dir(out).exists(),
        "{} was left behind",
        staging_dir(out).display()
    );
}

/// Asserts an object left neither a final nor a partial file
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `object` - The seeded object
fn assert_absent(out: &Path, object: &Seeded) {
    // neither the object nor anything staged for it exists
    let path = object_path(out, &object.key);
    assert!(!path.exists(), "{} was left at its final path", object.key);
    assert!(
        !staging_dir(out).exists(),
        "{} was left behind",
        staging_dir(out).display()
    );
}

/// Returns the failed download record naming an object
///
/// # Arguments
///
/// * `records` - The run's records
/// * `object` - The seeded object
fn download_record<'a>(records: &'a [FailureRecord], object: &Seeded) -> &'a FailureRecord {
    // the record names the key
    records
        .iter()
        .find(|record| record.key.as_deref() == Some(object.key.as_str()))
        .unwrap_or_else(|| panic!("no record of {} in {records:?}", object.key))
}

/// Downloads cut off by a 503, a reset connection, or a body shorter than its length are retried
/// and land complete
#[tokio::test]
async fn transient_download_faults_are_retried_and_land_complete() {
    let harness = Harness::get().await;
    // objects at the multipart part-size boundary and one small one
    let slow = seed_object(harness, 5_242_880).await;
    let reset = seed_object(harness, 1_000_000).await;
    let short = seed_object(harness, 5_242_881).await;
    let (mock, conf) = mocked(harness).await;
    // fail the first download of each object one way
    mock.set_rules(vec![
        get_rule(
            &slow.key,
            Some(1),
            S3Behavior::Status {
                status: 503,
                code: "SlowDown".to_owned(),
            },
        ),
        get_rule(&reset.key, Some(1), S3Behavior::ResetAfter(1_000)),
        get_rule(&short.key, Some(1), S3Behavior::TruncateBody(1_000_000)),
    ])
    .await;
    let out = harness.scratch("transient-downloads");
    let outcome = Box::pin(backup_objects(harness, &conf, &out, Arc::new(TokioFs))).await;
    // nothing was recorded and every object landed whole
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(records.is_empty(), "{records:?}");
    assert_eq!(outcome, RunOutcome::Complete);
    for object in [&slow, &reset, &short] {
        assert_landed(&out, object);
    }
}

/// Downloads that keep failing, and a missing object, are recorded with no file left at the
/// final path or beside it; the other objects land, and a run into a new directory after the
/// faults clear is complete and passes `backup verify`
#[tokio::test]
async fn permanent_download_faults_are_recorded_and_leave_no_file() {
    let harness = Harness::get().await;
    let slow = seed_object(harness, 5_242_880).await;
    let reset = seed_object(harness, 1_000_000).await;
    let short = seed_object(harness, 5_242_881).await;
    let missing = seed_object(harness, 2_000).await;
    let healthy = seed_object(harness, 3_000).await;
    let (mock, conf) = mocked(harness).await;
    // fail every download of four objects
    mock.set_rules(vec![
        get_rule(
            &slow.key,
            None,
            S3Behavior::Status {
                status: 503,
                code: "SlowDown".to_owned(),
            },
        ),
        get_rule(&reset.key, None, S3Behavior::ResetAfter(1_000)),
        get_rule(&short.key, None, S3Behavior::TruncateBody(1_000_000)),
        get_rule(
            &missing.key,
            None,
            S3Behavior::Status {
                status: 404,
                code: "NoSuchKey".to_owned(),
            },
        ),
    ])
    .await;
    let out = harness.scratch("permanent-downloads");
    let outcome = Box::pin(backup_objects(harness, &conf, &out, Arc::new(TokioFs))).await;
    // the run finished with each failed object recorded once
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    for object in [&slow, &reset, &short, &missing] {
        let record = download_record(&records, object);
        assert_eq!(record.kind, FailureKind::Failed);
        assert_eq!(record.operation, Operation::Download);
        assert_eq!(record.component, "s3-ids-objects");
        assert_eq!(
            record.bucket.as_deref(),
            Some(harness.conf.thorium.files.bucket.as_str())
        );
        assert_absent(&out, object);
    }
    // transient faults used every attempt; the missing object failed on its first
    for object in [&slow, &reset, &short] {
        assert_eq!(
            download_record(&records, object).attempts,
            RETRIES,
            "{}",
            object.key
        );
    }
    assert_eq!(download_record(&records, &missing).attempts, 1);
    // every other object landed
    assert_landed(&out, &healthy);
    // once the faults clear, a fresh backup is complete and verifies
    mock.set_rules(Vec::new()).await;
    let out = harness.scratch("permanent-downloads-cleared");
    let outcome = Box::pin(backup_objects(harness, &conf, &out, Arc::new(TokioFs))).await;
    assert_eq!(outcome, RunOutcome::Complete);
    let verified = verify(harness, &out);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
}

/// A download that runs out of disk, or whose write comes up short, is recorded and leaves no
/// file at its final path
#[tokio::test]
async fn disk_faults_on_a_download_are_recorded_and_leave_no_file() {
    let harness = Harness::get().await;
    let full = seed_object(harness, 2_000_000).await;
    let short = seed_object(harness, 2_000_000).await;
    let healthy = seed_object(harness, 4_000).await;
    // fill the disk after 1,000 bytes of one object, and cut one write of another short
    let sink = FaultySink::new();
    let bucket = &harness.conf.thorium.files.bucket;
    let staged = |object: &Seeded| format!("*/{STAGING_DIR}/{}", staging_name(bucket, &object.key));
    sink.add_fault(staged(&full), SinkFault::NoSpaceAfter(1_000));
    sink.add_fault(staged(&short), SinkFault::ShortWrite(500));
    let out = harness.scratch("disk-faults");
    let outcome = Box::pin(backup_objects(harness, &harness.conf, &out, Arc::new(sink))).await;
    // the run finished with both objects recorded and neither left on disk
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    for object in [&full, &short] {
        let record = download_record(&records, object);
        assert_eq!(record.operation, Operation::Download);
        assert_absent(&out, object);
    }
    // the other objects landed
    assert_landed(&out, &healthy);
}

/// A file an interrupted run left at an object's final path is kept only when its size matches
/// the object and its sha256 matches the interrupted run's object index; otherwise it is
/// downloaded again
#[tokio::test]
async fn a_file_left_at_the_final_path_is_checked_before_it_is_kept() {
    let harness = Harness::get().await;
    let cut = seed_object(harness, 3_000_000).await;
    let whole = seed_object(harness, 2_000).await;
    let altered = seed_object(harness, 2_000).await;
    let unindexed = seed_object(harness, 2_000).await;
    // an interrupted backup left 1,000 bytes of one object, all of two others, and a file of the
    // right size but other bytes for a third
    let out = harness.scratch("left-files");
    InProgressMarker::new(
        chrono::Utc::now(),
        vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
    )
    .write(&out, &TokioFs)
    .await
    .unwrap();
    let mut other_bytes = altered.bytes.clone();
    other_bytes[0] ^= 0xff;
    let left: [(&Seeded, Vec<u8>); 4] = [
        (&cut, cut.bytes[..1_000].to_vec()),
        (&whole, whole.bytes.clone()),
        (&altered, other_bytes),
        (&unindexed, unindexed.bytes.clone()),
    ];
    for (object, bytes) in &left {
        let path = object_path(&out, &object.key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
    // its index lists every object it downloaded whole, except the one it didn't get to index
    let bucket = harness.conf.thorium.files.bucket.clone();
    let mut index = String::new();
    for object in [&cut, &whole, &altered] {
        let line = ObjectIndexLine {
            bucket: bucket.clone(),
            key: object.key.clone(),
            size: object.bytes.len() as u64,
            etag: String::new(),
            sha256: sha256(&object.bytes),
        };
        index.push_str(&serde_json::to_string(&line).unwrap());
        index.push('\n');
    }
    std::fs::write(out.join("s3_ids").join("objects.index.jsonl"), index).unwrap();
    let (mock, conf) = mocked(harness).await;
    let outcome = Box::pin(backup_objects(harness, &conf, &out, Arc::new(TokioFs))).await;
    assert_eq!(outcome, RunOutcome::Complete);
    // every object is whole on disk
    for object in [&cut, &whole, &altered, &unindexed] {
        assert_landed(&out, object);
    }
    // only the whole, indexed file was kept; the others were downloaded again
    let seen = mock.seen().await;
    let downloaded = |key: &str| {
        seen.iter()
            .any(|request| request.method == Method::GET && request.key == key)
    };
    assert!(downloaded(&cut.key), "the cut file wasn't downloaded again");
    assert!(
        downloaded(&altered.key),
        "the altered file wasn't downloaded again"
    );
    assert!(
        downloaded(&unindexed.key),
        "the unindexed file wasn't downloaded again"
    );
    assert!(
        !downloaded(&whole.key),
        "the whole file was downloaded again"
    );
}

/// An object that several rows name is downloaded once and lands whole
#[tokio::test]
async fn an_object_named_by_many_rows_is_downloaded_once() {
    let harness = Harness::get().await;
    let shared = seed_object(harness, 1_000_000).await;
    // more rows of the same s3 id, each with another sha256, all naming the one object
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for copy in 0..40 {
        scylla
            .query_unpaged(
                format!("INSERT INTO {ns}.s3_ids (type, id, sha256) VALUES ('File', ?, ?)"),
                (Uuid::parse_str(&shared.key).unwrap(), format!("{copy:064}")),
            )
            .await
            .expect("failed to insert an s3 id row");
    }
    let (mock, conf) = mocked(harness).await;
    let out = harness.scratch("named-by-many");
    let outcome = Box::pin(backup_objects(harness, &conf, &out, Arc::new(TokioFs))).await;
    // nothing was recorded, the object landed whole, and it was fetched once
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(records.is_empty(), "{records:?}");
    assert_eq!(outcome, RunOutcome::Complete);
    assert_landed(&out, &shared);
    let fetched = mock
        .seen()
        .await
        .iter()
        .filter(|request| request.method == Method::GET && request.key == shared.key)
        .count();
    assert_eq!(fetched, 1);
}

/// The thoradm binary under a 256-descriptor limit backs up all 2,000 files of one result and
/// exits 0
#[tokio::test]
async fn many_objects_of_one_partition_land_under_a_low_descriptor_limit() {
    let harness = Harness::get().await;
    // one result whose row names 2,000 files, each uploaded to the results bucket
    let id = Uuid::new_v4();
    let names: Vec<String> = (0..2_000).map(|i| format!("io-{i:04}.txt")).collect();
    let s3 = harness.s3();
    let bucket = &harness.conf.thorium.results.bucket;
    futures::future::join_all(names.chunks(100).map(|chunk| {
        let s3 = s3.clone();
        async move {
            for name in chunk {
                s3.put_object()
                    .bucket(bucket)
                    .key(format!("{id}/{name}"))
                    .body(ByteStream::from(name.as_bytes().to_vec()))
                    .send()
                    .await
                    .expect("failed to upload a result file");
            }
        }
    }))
    .await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let tool = Names::new().group("vulcan");
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.results (id, uploaded, tool, cmd, result, files, display_type) \
                 VALUES (?, toTimestamp(now()), ?, 'forge', 'the forge is lit', ?, 'String')"
            ),
            (id, tool.as_str(), &names),
        )
        .await
        .expect("failed to insert the result");
    // back up the results and their files with the descriptor limit lowered in the child
    let out = harness.scratch("descriptor-limit");
    {
        let components = "results,result-files";
        let mut command = harness.thoradm();
        command.args(["backup", "new", components, "-o"]).arg(&out);
        // SAFETY: setrlimit only changes this child's limits between fork and exec
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 256,
                    rlim_max: 256,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let output = command.output().expect("failed to run thoradm");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{components}:\n{stderr}");
    }
    // every file of the result was backed up
    let dir = out
        .join("results")
        .join("objects")
        .join(&id.to_string()[..6])
        .join(id.to_string());
    for name in &names {
        let found = std::fs::read(dir.join(name)).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(found, name.as_bytes());
    }
}

/// Backs up the results table and its files in-process
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `out` - The backup directory
/// * `sinks` - The file-system seam the backup writes through
async fn backup_result_files(
    harness: &Harness,
    out: &Path,
    sinks: Arc<dyn SinkFactory>,
) -> RunOutcome {
    // read through the harness's Scylla and write through the given seam
    let scylla = harness.scylla().await;
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).expect("failed to load the ctl config");
    let args = NewBackup {
        components: vec![BackupComponents::Results, BackupComponents::ResultFiles],
        output: out.to_path_buf(),
        multiplier: 2,
        retries: RETRIES,
        s3_concurrency: 64,
        abort_on_failure: false,
        allow_version_mismatch: false,
        prune_dangling: false,
    };
    Box::pin(backup::run(
        &harness.conf,
        ctl_conf,
        &scylla,
        2,
        &args,
        sinks,
    ))
    .await
    .expect("the backup failed to run")
}

/// Result files named `x` and `x.partial` in one result each land whole with their own bytes,
/// and the object index and manifest describe exactly what landed
#[tokio::test]
async fn result_files_named_like_staged_downloads_land_intact() {
    let harness = Harness::get().await;
    let s3 = harness.s3();
    let bucket = &harness.conf.thorium.results.bucket;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    // 40 results, each naming `out<n>.bin.partial` first and then `out<n>.bin`, with other sizes
    let mut expected: Vec<(String, PathBuf, Vec<u8>)> = Vec::new();
    let out = harness.scratch("partial-names");
    for n in 0..40 {
        let id = Uuid::new_v4();
        let names = vec![format!("out{n}.bin.partial"), format!("out{n}.bin")];
        for (i, name) in names.iter().enumerate() {
            let mut bytes = vec![0_u8; 20_000 + i * 50_000 + n * 7];
            rand::rng().fill_bytes(&mut bytes);
            let key = format!("{id}/{name}");
            s3.put_object()
                .bucket(bucket)
                .key(&key)
                .body(ByteStream::from(bytes.clone()))
                .send()
                .await
                .expect("failed to upload a result file");
            let path = out
                .join("results")
                .join("objects")
                .join(&id.to_string()[..6])
                .join(id.to_string())
                .join(name);
            expected.push((key, path, bytes));
        }
        let tool = Names::new().group("juno");
        scylla
            .query_unpaged(
                format!(
                    "INSERT INTO {ns}.results (id, uploaded, tool, cmd, result, files, \
                     display_type) VALUES (?, toTimestamp(now()), ?, 'forge', 'lit', ?, 'String')"
                ),
                (id, tool.as_str(), &names),
            )
            .await
            .expect("failed to insert a result");
    }
    let outcome = Box::pin(backup_result_files(harness, &out, Arc::new(TokioFs))).await;
    // nothing was recorded
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(records.is_empty(), "{records:?}");
    assert_eq!(outcome, RunOutcome::Complete);
    // every file holds its own object's bytes
    for (key, path, bytes) in &expected {
        let found = std::fs::read(path).unwrap_or_else(|err| panic!("{key} is missing: {err}"));
        assert_eq!(
            found.len(),
            bytes.len(),
            "{key} has another object's length"
        );
        assert_eq!(sha256(&found), sha256(bytes), "{key} has other bytes");
    }
    // the index lists each object once, with the sha256 of the file it landed in
    let index = std::fs::read_to_string(out.join("results").join("objects.index.jsonl")).unwrap();
    let lines: Vec<ObjectIndexLine> = index
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (key, _, bytes) in &expected {
        let listed: Vec<&ObjectIndexLine> = lines.iter().filter(|line| line.key == *key).collect();
        assert_eq!(listed.len(), 1, "{key} is listed {} times", listed.len());
        assert_eq!(listed[0].size, bytes.len() as u64, "{key}'s listed size");
        assert_eq!(listed[0].sha256, sha256(bytes), "{key}'s listed sha256");
    }
    // every line, including those of results other tests add, matches the file it names
    for line in &lines {
        let path = out
            .join("results")
            .join("objects")
            .join(&line.key[..6])
            .join(&line.key);
        let found = std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", line.key));
        assert_eq!(found.len() as u64, line.size, "{}'s file length", line.key);
        assert_eq!(sha256(&found), line.sha256, "{}'s file sha256", line.key);
    }
    // the manifest counts exactly the objects and bytes the index lists
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    let entry = &manifest.s3["result-files"];
    assert_eq!(entry.objects, lines.len() as u64);
    assert_eq!(entry.bytes, lines.iter().map(|line| line.size).sum::<u64>());
    // no staging file is left anywhere in the backup
    let staged = out.join("results").join(STAGING_DIR);
    assert!(!staged.exists(), "{} was left behind", staged.display());
}

/// A download an interrupted run left in the staging directory never reaches the finished backup
#[tokio::test]
async fn a_staged_download_an_interrupted_run_left_is_removed() {
    let harness = Harness::get().await;
    let object = seed_object(harness, 2_000).await;
    // an interrupted backup left half of a download in its staging directory
    let out = harness.scratch("stray-staging");
    InProgressMarker::new(
        chrono::Utc::now(),
        vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
    )
    .write(&out, &TokioFs)
    .await
    .unwrap();
    let stray = staging_dir(&out).join(staging_name("gone", "gone"));
    std::fs::create_dir_all(staging_dir(&out)).unwrap();
    std::fs::write(&stray, b"half an object").unwrap();
    // the resumed run finishes with the object and without the staged file
    let outcome = Box::pin(backup_objects(
        harness,
        &harness.conf,
        &out,
        Arc::new(TokioFs),
    ))
    .await;
    assert_eq!(outcome, RunOutcome::Complete);
    assert!(out.join("manifest.json").exists());
    assert_landed(&out, &object);
}

/// A file an interrupted run left for an object that can't be downloaded again is removed, so
/// the finished backup never holds a cut or stale file at an object's path
#[tokio::test]
async fn a_rejected_left_file_whose_download_fails_is_removed() {
    let harness = Harness::get().await;
    let cut = seed_object(harness, 3_000_000).await;
    let deleted = seed_object(harness, 2_000).await;
    // an interrupted backup left 1,000 bytes of one object and all of another, unindexed
    let out = harness.scratch("rejected-left-files");
    InProgressMarker::new(
        chrono::Utc::now(),
        vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
    )
    .write(&out, &TokioFs)
    .await
    .unwrap();
    for (object, bytes) in [(&cut, &cut.bytes[..1_000]), (&deleted, &deleted.bytes[..])] {
        let path = object_path(&out, &object.key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
    // the cut object's download now fails, and the other object is gone from S3 altogether
    let (mock, conf) = mocked(harness).await;
    let missing = || S3Behavior::Status {
        status: 404,
        code: "NoSuchKey".to_owned(),
    };
    mock.set_rules(vec![
        get_rule(&cut.key, None, missing()),
        get_rule(&deleted.key, None, missing()),
        S3Rule {
            method: Method::HEAD,
            key: deleted.key.clone(),
            query: None,
            nth: None,
            behavior: missing(),
        },
    ])
    .await;
    let outcome = Box::pin(backup_objects(harness, &conf, &out, Arc::new(TokioFs))).await;
    // both objects are recorded and neither file is left in the finished backup
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    assert!(out.join("manifest.json").exists());
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    for object in [&cut, &deleted] {
        assert_eq!(
            download_record(&records, object).operation,
            Operation::Download
        );
        assert_absent(&out, object);
    }
}

/// A rejected left file is removed before its object is downloaded again, so a run that
/// `--abort-on-failure` stops on that download, which skips the resume's removal of unlanded
/// files, leaves nothing at the object's path either
#[tokio::test]
async fn a_rejected_left_file_is_removed_before_a_download_that_stops_the_run() {
    let harness = Harness::get().await;
    let cut = seed_object(harness, 3_000_000).await;
    // an interrupted backup left 1,000 bytes of the object, unindexed
    let out = harness.scratch("rejected-left-file-stopped");
    InProgressMarker::new(
        chrono::Utc::now(),
        vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
    )
    .write(&out, &TokioFs)
    .await
    .unwrap();
    let path = object_path(&out, &cut.key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &cut.bytes[..1_000]).unwrap();
    // the object's download now fails, which stops the run
    let (mock, conf) = mocked(harness).await;
    mock.set_rules(vec![get_rule(
        &cut.key,
        None,
        S3Behavior::Status {
            status: 404,
            code: "NoSuchKey".to_owned(),
        },
    )])
    .await;
    let outcome = Box::pin(backup_objects_with(
        harness,
        &conf,
        &out,
        Arc::new(TokioFs),
        true,
    ))
    .await;
    // the run stopped on this object, stayed resumable, and left no file at its path
    assert_eq!(outcome, RunOutcome::Aborted);
    assert!(!out.join("manifest.json").exists());
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert_eq!(
        download_record(&records, &cut).operation,
        Operation::Download
    );
    assert_absent(&out, &cut);
}

/// Files an interrupted run left under `objects/` for objects no row names any more, including
/// one named like a staged download, never reach the finished backup, where restore would upload
/// them as objects
#[tokio::test]
async fn files_of_objects_no_row_names_are_not_left_in_a_finished_backup() {
    let harness = Harness::get().await;
    let object = seed_object(harness, 2_000).await;
    // an interrupted backup left a whole file and a partial one for objects since deleted
    let out = harness.scratch("unnamed-left-files");
    InProgressMarker::new(
        chrono::Utc::now(),
        vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
    )
    .write(&out, &TokioFs)
    .await
    .unwrap();
    let gone = Uuid::new_v4().to_string();
    let orphan = object_path(&out, &gone);
    let stray = orphan.with_file_name(format!("{gone}.partial"));
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"a whole object whose row is gone").unwrap();
    std::fs::write(&stray, b"half an object").unwrap();
    // the resumed run finishes with only the objects its rows name
    let outcome = Box::pin(backup_objects(
        harness,
        &harness.conf,
        &out,
        Arc::new(TokioFs),
    ))
    .await;
    assert_eq!(outcome, RunOutcome::Complete);
    assert!(out.join("manifest.json").exists());
    assert_landed(&out, &object);
    for left in [&orphan, &stray] {
        assert!(
            !left.exists(),
            "{} stayed in the finished backup",
            left.display()
        );
    }
}

/// Objects landing in one directory sync it once, after every rename into it and before the
/// object index is synced for the manifest
#[tokio::test]
async fn a_directory_of_landed_objects_is_synced_once_before_the_index() {
    let harness = Harness::get().await;
    // one result whose row names 200 files, all landing in the result's directory
    let id = Uuid::new_v4();
    let names: Vec<String> = (0..200).map(|i| format!("sync-{i:03}.txt")).collect();
    let s3 = harness.s3();
    let bucket = &harness.conf.thorium.results.bucket;
    for name in &names {
        s3.put_object()
            .bucket(bucket)
            .key(format!("{id}/{name}"))
            .body(ByteStream::from(name.as_bytes().to_vec()))
            .send()
            .await
            .expect("failed to upload a result file");
    }
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let tool = Names::new().group("minerva");
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.results (id, uploaded, tool, cmd, result, files, display_type) \
                 VALUES (?, toTimestamp(now()), ?, 'loom', 'woven', ?, 'String')"
            ),
            (id, tool.as_str(), &names),
        )
        .await
        .expect("failed to insert the result");
    // back up through a seam that records every call
    let out = harness.scratch("dir-syncs");
    let sink = FaultySink::new();
    let outcome = Box::pin(backup_result_files(harness, &out, Arc::new(sink.clone()))).await;
    assert_eq!(outcome, RunOutcome::Complete);
    let calls = sink.calls();
    // the result's directory is synced once
    let dir = out
        .join("results")
        .join("objects")
        .join(&id.to_string()[..6])
        .join(id.to_string());
    let position = |wanted: &SinkCall| calls.iter().position(|call| call == wanted);
    let syncs: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| **call == SinkCall::SyncDir(dir.clone()))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        syncs.len(),
        1,
        "{} was synced {} times",
        dir.display(),
        syncs.len()
    );
    // after the last rename into it, and before the index is synced
    let last_rename = calls
        .iter()
        .rposition(
            |call| matches!(call, SinkCall::Rename(_, to) if to.parent() == Some(dir.as_path())),
        )
        .expect("nothing was renamed into the result's directory");
    assert!(
        last_rename < syncs[0],
        "a rename came after the directory sync"
    );
    let index_sync = position(&SinkCall::Sync(
        out.join("results").join("objects.index.jsonl"),
    ))
    .expect("the index was never synced");
    assert!(
        syncs[0] < index_sync,
        "the index was synced before the directory"
    );
}

/// An interrupted run's object index whose last line was torn inside a multi-byte character
/// doesn't stop the resume, which lands every object
#[tokio::test]
async fn an_index_torn_inside_a_character_doesnt_stop_the_resume() {
    let harness = Harness::get().await;
    let object = seed_object(harness, 2_000).await;
    // an interrupted backup's index ends in a line cut inside the two bytes of an é
    let out = harness.scratch("torn-index");
    InProgressMarker::new(
        chrono::Utc::now(),
        vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
    )
    .write(&out, &TokioFs)
    .await
    .unwrap();
    std::fs::create_dir_all(out.join("s3_ids")).unwrap();
    let line = ObjectIndexLine {
        bucket: harness.conf.thorium.files.bucket.clone(),
        key: "r\u{e9}sum\u{e9}.txt".to_owned(),
        size: 10,
        etag: String::new(),
        sha256: "0".repeat(64),
    };
    let text = serde_json::to_string(&line).unwrap();
    let cut = text.find('\u{e9}').unwrap() + 1;
    std::fs::write(
        out.join("s3_ids").join("objects.index.jsonl"),
        &text.as_bytes()[..cut],
    )
    .unwrap();
    // the resumed run finishes and lands the object
    let outcome = Box::pin(backup_objects(
        harness,
        &harness.conf,
        &out,
        Arc::new(TokioFs),
    ))
    .await;
    assert_eq!(outcome, RunOutcome::Complete);
    assert_landed(&out, &object);
}

/// Every directory a backup creates on an object's path is synced into its parent before the
/// object index is synced, so a crash can't drop a directory whose objects the index lists
#[tokio::test]
async fn new_object_directories_are_synced_into_their_parents_before_the_index() {
    let harness = Harness::get().await;
    // one result whose row names two files, landing in directories no backup has created yet
    let id = Uuid::new_v4();
    let names = vec!["web-0.txt".to_owned(), "web-1.txt".to_owned()];
    let s3 = harness.s3();
    let bucket = &harness.conf.thorium.results.bucket;
    for name in &names {
        s3.put_object()
            .bucket(bucket)
            .key(format!("{id}/{name}"))
            .body(ByteStream::from(name.as_bytes().to_vec()))
            .send()
            .await
            .expect("failed to upload a result file");
    }
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let tool = Names::new().group("arachne");
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.results (id, uploaded, tool, cmd, result, files, display_type) \
                 VALUES (?, toTimestamp(now()), ?, 'spin', 'spun', ?, 'String')"
            ),
            (id, tool.as_str(), &names),
        )
        .await
        .expect("failed to insert the result");
    // back up through a seam that records every call
    let out = harness.scratch("new-dir-syncs");
    let sink = FaultySink::new();
    let outcome = Box::pin(backup_result_files(harness, &out, Arc::new(sink.clone()))).await;
    assert_eq!(outcome, RunOutcome::Complete);
    let calls = sink.calls();
    let index_sync = calls
        .iter()
        .position(|call| *call == SinkCall::Sync(out.join("results").join("objects.index.jsonl")))
        .expect("the index was never synced");
    // the parent of every directory created on the result's path is synced before the index
    let component = out.join("results");
    let objects = component.join("objects");
    let prefix = objects.join(&id.to_string()[..6]);
    for parent in [&component, &objects, &prefix] {
        let synced = calls
            .iter()
            .position(|call| *call == SinkCall::SyncDir(parent.clone()))
            .unwrap_or_else(|| panic!("{} was never synced", parent.display()));
        assert!(
            synced < index_sync,
            "{} was synced after the index",
            parent.display()
        );
    }
}
