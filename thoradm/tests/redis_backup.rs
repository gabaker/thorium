//! The Redis half of a backup: `redis.json` is private and lands whole or not at all

mod common;

use axum::http::Method;
use common::faulty_sink::{FaultySink, SinkFault};
use common::harness::{Harness, redis_pool, write_ctl_conf};
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use thoradm::args::{BackupComponents, NewBackup, RestoreBackup};
use thoradm::backup::controllers::{backup, restore};
use thoradm::backup::failures::{FAILURES_FILE, FailureKind, read_records};
use thoradm::backup::manifest::InProgressMarker;
use thoradm::backup::sink::{SinkFactory, TokioFs};
use thoradm::outcome::RunOutcome;
use thorium::test_utilities::generators;
use thorium::{Conf, CtlConf};

/// Returns the permission bits of a path
///
/// # Arguments
///
/// * `path` - The file or directory
fn mode(path: &Path) -> u32 {
    // keep only the permission bits
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Backs up only the Redis component in-process
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `conf` - The config to back up with
/// * `ctl_conf` - The Thorctl config naming the API
/// * `out` - The backup directory
/// * `sinks` - The file-system seam the backup writes through
async fn backup_redis(
    harness: &Harness,
    conf: &Conf,
    ctl_conf: CtlConf,
    out: &Path,
    sinks: Arc<dyn SinkFactory>,
) -> RunOutcome {
    // Redis alone needs no Scylla reads, but the controller takes a session
    let scylla = harness.scylla().await;
    let args = NewBackup {
        components: vec![BackupComponents::Redis],
        output: out.to_path_buf(),
        multiplier: 1,
        retries: 3,
        s3_concurrency: 64,
        abort_on_failure: false,
        allow_version_mismatch: false,
        prune_dangling: false,
    };
    Box::pin(backup::run(conf, ctl_conf, &scylla, 1, &args, sinks))
        .await
        .expect("the backup failed to run")
}

/// `backup new redis` into a new directory creates the directory 0700 and `redis.json` 0600
#[tokio::test]
async fn redis_json_is_private_in_a_private_directory() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // a directory that doesn't exist yet, under a scratch parent
    let out = harness.scratch("private").join("backup");
    let output = harness
        .thoradm()
        .args(["backup", "new", "redis", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // the new directory and the file holding every credential are private to this user
    assert_eq!(mode(&out), 0o700);
    assert_eq!(mode(&out.join("redis.json")), 0o600);
    // the state parses
    let text = std::fs::read_to_string(out.join("redis.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).expect("redis.json isn't JSON");
    // no temporary file is left
    assert!(!out.join("redis.json.tmp").exists());
}

/// A full disk while writing `redis.json`, and a rename that fails, each leave no `redis.json`
/// and record the failure
#[tokio::test]
async fn a_redis_json_that_cant_be_written_is_recorded_and_absent() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    // one run per fault, each into its own directory
    for (name, fault) in [
        ("no-space", SinkFault::NoSpaceAfter(0)),
        ("no-rename", SinkFault::FailRename),
    ] {
        // fail the write or the rename of redis.json only
        let sink = FaultySink::new();
        sink.add_fault("*redis.json*", fault);
        let out = harness.scratch(name);
        let outcome = Box::pin(backup_redis(
            harness,
            &harness.conf,
            ctl_conf.clone(),
            &out,
            Arc::new(sink),
        ))
        .await;
        // the run finished with the Redis state recorded as failed
        assert_eq!(outcome, RunOutcome::CompletedWithFailures, "{name}");
        let records = read_records(&out.join(FAILURES_FILE)).unwrap();
        let record = records
            .iter()
            .find(|record| record.component == "redis")
            .unwrap_or_else(|| panic!("{name}: no redis record in {records:?}"));
        assert_eq!(record.kind, FailureKind::Failed, "{name}");
        // neither redis.json nor its temporary file is left
        assert!(
            !out.join("redis.json").exists(),
            "{name}: redis.json exists"
        );
        assert!(
            !out.join("redis.json.tmp").exists(),
            "{name}: redis.json.tmp exists"
        );
    }
}

/// Starts a `MockS3` in front of the in-process API and returns it with a Thorctl config that
/// reaches the API only through it
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `name` - The scratch directory for the config
async fn proxied_api(harness: &Harness, name: &str) -> (MockS3, CtlConf) {
    // the proxy forwards every request to the API unless a rule says otherwise
    let mock = MockS3::start(&harness.api_url)
        .await
        .expect("failed to start the API proxy");
    let path = harness.scratch(name).join("thorctl.yml");
    write_ctl_conf(&mock.endpoint, &harness.token, &path).unwrap();
    let ctl_conf = CtlConf::from_path(&path).expect("failed to load the proxied ctl config");
    (mock, ctl_conf)
}

/// Builds a rule for the Redis backup request; the proxy reads `/api/system/backup` as bucket
/// `api` and key `system/backup`
///
/// # Arguments
///
/// * `nth` - The matching request to apply it to, or `None` for every match
/// * `behavior` - What to do to the request
fn backup_rule(nth: Option<u64>, behavior: S3Behavior) -> S3Rule {
    // match the API's backup route
    S3Rule {
        method: Method::GET,
        key: "system/backup".to_owned(),
        query: None,
        nth,
        behavior,
    }
}

/// A 502, a reset connection, and a cut-off body from the API are retried, and `redis.json`
/// lands whole
#[tokio::test]
async fn api_faults_during_the_redis_backup_are_retried() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let (mock, ctl_conf) = proxied_api(harness, "api-transient-conf").await;
    // each rule fires on the first request it sees, so the faults come one after another
    mock.set_rules(vec![
        backup_rule(
            Some(1),
            S3Behavior::Status {
                status: 502,
                code: "BadGateway".to_owned(),
            },
        ),
        backup_rule(Some(1), S3Behavior::ResetAfter(10)),
        backup_rule(Some(1), S3Behavior::TruncateBody(20)),
    ])
    .await;
    let out = harness.scratch("api-transient");
    let sinks: Arc<dyn SinkFactory> = Arc::new(thoradm::backup::sink::TokioFs);
    let args = NewBackup {
        components: vec![BackupComponents::Redis],
        output: out.clone(),
        multiplier: 1,
        retries: 5,
        s3_concurrency: 64,
        abort_on_failure: false,
        allow_version_mismatch: false,
        prune_dangling: false,
    };
    let scylla = harness.scylla().await;
    let outcome = Box::pin(backup::run(
        &harness.conf,
        ctl_conf,
        &scylla,
        1,
        &args,
        sinks,
    ))
    .await
    .expect("the backup failed to run");
    // nothing was recorded and redis.json holds the whole state
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(records.is_empty(), "{records:?}");
    assert_eq!(outcome, RunOutcome::Complete);
    let text = std::fs::read_to_string(out.join("redis.json")).unwrap();
    let state: serde_json::Value = serde_json::from_str(&text).expect("redis.json isn't JSON");
    assert!(state.get("users").is_some(), "{text}");
    // the API was asked four times: three faults, then the answer
    let asked = mock
        .seen()
        .await
        .iter()
        .filter(|request| request.key == "system/backup")
        .count();
    assert_eq!(asked, 4);
}

/// An API that keeps failing the Redis backup is recorded under `redis`, leaves no `redis.json`,
/// and the run exits 2
#[tokio::test]
async fn an_api_that_keeps_failing_is_recorded() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // the API answers every Redis backup request with a 502
    let (mock, ctl_conf) = proxied_api(harness, "api-permanent-conf").await;
    mock.set_rules(vec![backup_rule(
        None,
        S3Behavior::Status {
            status: 502,
            code: "BadGateway".to_owned(),
        },
    )])
    .await;
    // the run finishes with exit 2
    let out = harness.scratch("api-permanent");
    let outcome = Box::pin(backup_redis(
        harness,
        &harness.conf,
        ctl_conf,
        &out,
        Arc::new(thoradm::backup::sink::TokioFs),
    ))
    .await;
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    // the Redis state is recorded once, after every attempt
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    let record = records
        .iter()
        .find(|record| record.component == "redis")
        .unwrap_or_else(|| panic!("no redis record in {records:?}"));
    assert_eq!(record.attempts, 3, "{record:?}");
    // and no redis.json was written
    assert!(!out.join("redis.json").exists());
}

/// A resumed backup whose Redis fetch fails leaves no `redis.json` from the interrupted run, so
/// the manifest and the files agree that the backup holds no Redis state
#[tokio::test]
async fn a_resumed_run_whose_fetch_fails_keeps_no_stale_redis_json() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let (mock, ctl_conf) = proxied_api(harness, "stale-redis-conf").await;
    mock.set_rules(vec![backup_rule(
        None,
        S3Behavior::Status {
            status: 502,
            code: "BadGateway".to_owned(),
        },
    )])
    .await;
    // an interrupted backup left its redis.json and a temporary file
    let out = harness.scratch("stale-redis");
    InProgressMarker::new(chrono::Utc::now(), vec![BackupComponents::Redis])
        .write(&out, &TokioFs)
        .await
        .unwrap();
    std::fs::write(out.join("redis.json"), b"{\"stale\": true}").unwrap();
    std::fs::write(out.join("redis.json.tmp"), b"{\"stale\":").unwrap();
    // the resumed run records the fetch and finishes
    let outcome = Box::pin(backup_redis(
        harness,
        &harness.conf,
        ctl_conf,
        &out,
        Arc::new(TokioFs),
    ))
    .await;
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(
        records.iter().any(|record| record.component == "redis"),
        "no redis record in {records:?}"
    );
    // the manifest lists no Redis state and no file claims otherwise
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest.get("redis").is_none_or(serde_json::Value::is_null),
        "{manifest}"
    );
    assert!(
        !out.join("redis.json").exists(),
        "the stale redis.json was kept"
    );
    assert!(
        !out.join("redis.json.tmp").exists(),
        "the stale temporary file was kept"
    );
}

/// Returns every directory and file under a directory, with the directory itself
///
/// # Arguments
///
/// * `root` - The directory to walk
fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    // depth first, listing each entry once
    let mut found = vec![root.to_path_buf()];
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found
}

/// A backup into a directory created beforehand readable by everyone makes it private, and
/// every directory and file the backup writes in it is private to this user
#[tokio::test]
async fn a_backup_into_a_readable_directory_makes_everything_private() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // a sample, so the backup holds archives and an object as well as redis.json
    let client = harness.client().await;
    let group = common::names::Names::new().group("vesta");
    common::seed::group(&client, &group).await;
    common::seed::sample(
        &client,
        &group,
        "hearth.bin",
        b"a sample to back up privately",
    )
    .await;
    // an empty directory made beforehand that everyone can read
    let out = harness.scratch("readable");
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = harness
        .thoradm()
        .args([
            "backup",
            "new",
            "samples-list,s3-ids,s3-ids-objects,redis",
            "-o",
        ])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // the root and everything in it is private to this user
    let paths = walk(&out);
    assert!(
        paths
            .iter()
            .any(|path| path.ends_with("objects.index.jsonl")),
        "{paths:?}"
    );
    for path in &paths {
        let expected = if path.is_dir() { 0o700 } else { 0o600 };
        assert_eq!(
            mode(path),
            expected,
            "{} is {:o}",
            path.display(),
            mode(path)
        );
    }
}

/// Making a group-shared root private logs its whole former mode, so the setgid bit it drops is
/// visible as well as the group's access
#[tokio::test]
async fn making_a_setgid_root_private_logs_its_whole_mode() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // an empty group directory made beforehand that new files inherit the group of
    let out = harness.scratch("setgid");
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o2775)).unwrap();
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o7777,
        0o2775,
        "the scratch directory couldn't be made setgid"
    );
    let output = harness
        .thoradm()
        .args(["backup", "new", "samples-list", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // the root is private without its setgid bit, and the log names the whole mode it had
    let after = std::fs::metadata(&out).unwrap().permissions().mode() & 0o7777;
    assert_eq!(after, 0o700, "the root is {after:o}");
    assert!(stderr.contains("(it was 2775)"), "{stderr}");
}

/// A restore whose Redis step fails records it and exits 2, and its summary doesn't claim the
/// Redis restore deleted the namespace's keys
#[tokio::test]
async fn a_failed_redis_restore_isnt_summed_up_as_a_clear() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    // a backup of the Redis state alone
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let out = harness.scratch("failed-redis-restore");
    let backup = out.join("backup");
    let taken = Box::pin(backup_redis(
        harness,
        &harness.conf,
        ctl_conf,
        &backup,
        Arc::new(TokioFs),
    ))
    .await;
    assert_eq!(taken, RunOutcome::Complete);
    // the API answers every Redis restore request with a 502
    let (mock, ctl_conf) = proxied_api(harness, "failed-redis-restore-conf").await;
    mock.set_rules(vec![S3Rule {
        method: Method::POST,
        key: "system/restore".to_owned(),
        query: None,
        nth: None,
        behavior: S3Behavior::Status {
            status: 502,
            code: "BadGateway".to_owned(),
        },
    }])
    .await;
    // restore it through the proxy
    let report = out.join("report");
    let args = RestoreBackup {
        backup: backup.clone(),
        yes: true,
        merge: false,
        wipe: None,
        redis_flush_all: false,
        legacy: false,
        accept_failures: false,
        report_dir: Some(report.clone()),
        retries: 1,
        s3_concurrency: 64,
        abort_on_failure: false,
        allow_version_mismatch: false,
    };
    let scylla = harness.scylla().await;
    let outcome = Box::pin(restore::run(
        &harness.conf,
        ctl_conf,
        &scylla,
        1,
        &args,
        Arc::new(TokioFs),
    ))
    .await
    .expect("the restore failed to run");
    // the Redis step is recorded and the run exits 2
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    let records = read_records(&report.join(FAILURES_FILE)).unwrap();
    assert!(
        records.iter().any(|record| record.component == "redis"),
        "no redis record in {records:?}"
    );
    // the summary doesn't say the namespace's Redis keys were deleted
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    let warnings = summary["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("deleted every key of namespace"))),
        "{summary:#}"
    );
}

/// Sets one field of a Redis hash in the harness's Redis, returning the value it held
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `key` - The hash
/// * `field` - The field to set
/// * `value` - The value to set it to
async fn swap_field(harness: &Harness, key: &str, field: &str, value: &str) -> String {
    // read the old value and write the new one
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let (old,): (Option<String>,) = redis::pipe()
        .cmd("HGET")
        .arg(key)
        .arg(field)
        .cmd("HSET")
        .arg(key)
        .arg(field)
        .arg(value)
        .ignore()
        .query_async(&mut *conn)
        .await
        .unwrap();
    old.unwrap_or_else(|| panic!("{key} has no {field}"))
}

/// Records the API can't parse fail the Redis backup with one failure per key, no `redis.json`,
/// and exit 2, without copying a stored value into the failures file; once they are repaired,
/// a new backup exits 0
#[tokio::test]
async fn unparsable_records_are_recorded_one_per_key() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = &harness.conf.thorium.namespace;
    // a group with an image and a pipeline
    let client = harness.client().await;
    let (groups, images, pipes) = generators::gen_all(1, &client).await.unwrap();
    let group = &groups[0].name;
    let group_key = format!("{ns}:groups_data:{group}");
    let image_key = format!("{ns}:image_data:{group}:{}", images[0].name);
    let pipe_key = format!("{ns}:pipeline_data:{group}:{}", pipes[0].name);
    // corrupt one field of each with values of the wrong type
    let allowed = swap_field(
        harness,
        &group_key,
        "allowed",
        r#"{"files":"secret-allowed-bv"}"#,
    )
    .await;
    let resources = swap_field(
        harness,
        &image_key,
        "resources",
        r#"{"cpu":"secret-res-bv"}"#,
    )
    .await;
    let order = swap_field(harness, &pipe_key, "order", r#""secret-order-bv""#).await;
    // the backup records each key once, writes no redis.json, and exits 2
    let out = harness.scratch("unparsable");
    let run = harness
        .thoradm()
        .args(["backup", "new", "redis", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(2), "{stderr}");
    assert!(!out.join("redis.json").exists());
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    for key in [&group_key, &image_key, &pipe_key] {
        let matching = records
            .iter()
            .filter(|record| {
                record.component == "redis" && record.key.as_deref() == Some(key.as_str())
            })
            .count();
        assert_eq!(matching, 1, "{key} in {records:?}");
    }
    // no stored value reaches the failures file or the log
    let failures = std::fs::read_to_string(out.join(FAILURES_FILE)).unwrap();
    for secret in ["secret-allowed-bv", "secret-res-bv", "secret-order-bv"] {
        assert!(!failures.contains(secret), "{failures}");
        assert!(!stderr.contains(secret), "{stderr}");
    }
    // once repaired, a new backup, as a finished one is never changed, exits 0 with redis.json
    swap_field(harness, &group_key, "allowed", &allowed).await;
    swap_field(harness, &image_key, "resources", &resources).await;
    swap_field(harness, &pipe_key, "order", &order).await;
    let repaired = harness.scratch("unparsable-repaired");
    let rerun = harness
        .thoradm()
        .args(["backup", "new", "redis", "-o"])
        .arg(&repaired)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        rerun.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert!(repaired.join("redis.json").exists());
}
