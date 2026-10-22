//! `backup verify --cluster` checks a restored cluster against the backup's manifest: row counts
//! per token range, objects, the Redis state, views, and census streams; and an interrupted
//! restore is recovered by running it again with `--merge`
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::fixture;
use common::harness::{Harness, list_keys, redis_pool};
use common::kill;
use data_encoding::HEXLOWER;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, Operation, read_records};
use thoradm::backup::manifest::Manifest;
use thorium::models::{GroupAllowedUpdate, GroupUpdate};

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
/// what was seeded
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

/// Restores a backup without a terminal
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to restore
/// * `extra` - Extra `backup restore` arguments
fn restore(harness: &Harness, backup: &Path, extra: &[&str]) -> Output {
    // confirm with --confirm
    harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(backup)
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm")
}

/// What one `verify --cluster` run found
struct Run {
    /// The process output
    output: Output,
    /// The records of its failures file
    records: Vec<FailureRecord>,
    /// Its `verify.json`
    report: Value,
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
                    "{} {:?} {:?} {:?} {:?}: {}",
                    record.component,
                    record.table,
                    record.bucket,
                    record.token_range,
                    record.key,
                    record.error
                ))
                .collect::<Vec<_>>()
        )
    }
}

/// Runs `backup verify --cluster` against the namespace, its report in a fresh directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to check the cluster against
fn verify_cluster(harness: &Harness, backup: &Path) -> Run {
    // a fresh report directory beside the backup
    let report_dir = backup
        .parent()
        .unwrap()
        .join(format!("cluster-{}", uuid::Uuid::new_v4()));
    let output = harness
        .thoradm()
        .args(["--workers", "2", "backup", "verify", "--cluster", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(&report_dir)
        .output()
        .expect("failed to run thoradm");
    let records = read_records(&report_dir.join(FAILURES_FILE)).unwrap_or_default();
    let report = std::fs::read(report_dir.join("verify.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    Run {
        output,
        records,
        report,
    }
}

/// After a backup, wipe, and restore of the fixture, `verify --cluster` exits 0 with every row
/// count, object, Redis record, view, and census stream checked
#[tokio::test]
async fn a_restored_cluster_verifies() {
    // back up the fixture, empty the target, and restore it
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "clean").await;
    empty_target(harness).await;
    let restored = restore(harness, &backup, &[]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the cluster matches the backup in every check
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    assert_eq!(run.report["mode"], "cluster", "{:#}", run.report);
    let checks = run.report["checks"].as_array().expect("no checks");
    for check in ["row-count", "object", "redis-diff", "views", "census"] {
        assert!(
            checks
                .iter()
                .any(|found| found["check"] == check && found["checked"].as_u64() > Some(0)),
            "{check} checked nothing: {:#}",
            run.report
        );
    }
    assert!(
        checks.iter().all(|check| check["failed"] == 0),
        "{:#}",
        run.report
    );
}

/// After a clean restore, a deleted row, a deleted object, a group whose `allowed` changed, a
/// dropped view, and a deleted census stream each make `verify --cluster` exit 2, recording it
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn each_divergence_from_the_backup_is_found() {
    // a clean restore that verifies
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, seeded) = seeded_backup(harness, "diverge").await;
    empty_target(harness).await;
    let restored = restore(harness, &backup, &[]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    let ns = harness.conf.thorium.namespace.clone();
    let scylla = harness.scylla().await;
    // one row of one token range deleted
    let (group, year, bucket, uploaded, id) = scylla
        .query_unpaged(
            format!(
                "SELECT group, year, bucket, uploaded, id FROM {ns}.samples_list \
                 WHERE group = ? ALLOW FILTERING"
            ),
            (seeded.group.as_str(),),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .first_row::<(String, i32, i32, chrono::DateTime<chrono::Utc>, uuid::Uuid)>()
        .unwrap();
    scylla
        .query_unpaged(
            format!(
                "DELETE FROM {ns}.samples_list WHERE group = ? AND year = ? AND bucket = ? \
                 AND uploaded = ? AND id = ?"
            ),
            (group.as_str(), year, bucket, uploaded, id),
        )
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    assert!(
        run.records
            .iter()
            .any(|record| record.table.as_deref() == Some("samples_list")
                && record.token_range.is_some()),
        "{}",
        run.describe()
    );
    // one S3 object deleted
    let bucket_name = harness.conf.thorium.results.bucket.clone();
    let key = list_keys(&harness.s3(), &bucket_name)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the fixture has result files");
    harness
        .s3()
        .delete_object()
        .bucket(&bucket_name)
        .key(&key)
        .send()
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some(key.as_str())),
        "{}",
        run.describe()
    );
    // one group's allowed data changed through the API
    let update = GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_repos(),
        ..GroupUpdate::default()
    };
    harness
        .client()
        .await
        .groups
        .update(&seeded.group, &update)
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    let group_key = format!("groups/{}", seeded.group);
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some(group_key.as_str())),
        "{}",
        run.describe()
    );
    // one view dropped
    scylla
        .query_unpaged(format!("DROP MATERIALIZED VIEW {ns}.tags_by_item"), &[])
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some("tags_by_item")),
        "{}",
        run.describe()
    );
    // one census stream deleted
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let stream = format!("{ns}:census:repos:stream:{}:", seeded.group);
    let keys: Vec<String> = common::harness::scan_keys(&mut *conn, &format!("{stream}*"))
        .await
        .unwrap();
    let stream = keys
        .into_iter()
        .next()
        .expect("the fixture has a repo census");
    let _: () = redis::cmd("DEL")
        .arg(&stream)
        .query_async(&mut *conn)
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some(stream.as_str())),
        "{}",
        run.describe()
    );
    // bring the view back, as an API pod's schema setup would, for the tests after this one
    let _ = Box::pin(thorium::utils::Shared::with_opts(
        harness.conf.clone(),
        thorium::utils::StartupOpts { apply_schema: true },
    ))
    .await
    .expect("the API starts and applies the schema");
}

/// A restore killed during its table step leaves a partial target, which a plain re-run refuses;
/// a re-run with `--merge` finishes it, and `verify --cluster` passes
#[tokio::test]
async fn a_killed_restore_is_finished_by_a_merge_rerun() {
    // back up the fixture and empty the target
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "killed").await;
    empty_target(harness).await;
    // kill a restore once its first table is done
    let mut command = harness.thoradm();
    command
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(&backup);
    let killed = kill::run_until(command, "samples-list: ")
        .await
        .expect("failed to run thoradm");
    assert!(
        killed.killed,
        "the restore finished before it could be killed"
    );
    // the target holds part of the backup, so a plain re-run is refused
    let refused = restore(harness, &backup, &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let records = common::cycle::failure_records(&backup);
    assert!(
        records
            .iter()
            .any(|record| record.operation == Operation::Preflight
                && record.object.as_deref() == Some("empty-target")),
        "{records:#?}"
    );
    // a re-run with --merge finishes it, and the cluster matches the backup
    let merged = restore(harness, &backup, &["--merge"]);
    assert_eq!(merged.status.code(), Some(0), "{}", describe(&merged));
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    // the manifest's counts were what the check compared against
    let manifest = Manifest::read(&backup).await.unwrap().expect("no manifest");
    assert!(manifest.tables["samples_list"].rows > 0);
}

/// Groups a run's Redis records by record type and the fields named, for an assertion message
///
/// # Arguments
///
/// * `run` - The `verify --cluster` run
fn redis_differences(run: &Run) -> Vec<String> {
    // each record's type and fields, without the record's name
    let mut grouped: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for record in run
        .records
        .iter()
        .filter(|record| record.component == "redis")
    {
        let kind = record.key.as_deref().and_then(|key| key.split('/').next());
        let fields = record
            .error
            .split_once(" in ")
            .map_or(record.error.as_str(), |(_, fields)| fields);
        *grouped
            .entry(format!("{} {fields}", kind.unwrap_or("-")))
            .or_default() += 1;
    }
    grouped
        .into_iter()
        .map(|(group, count)| format!("{count} x {group}"))
        .collect()
}

/// After a restore of every value class, `verify --cluster` passes with every Redis record type
/// compared, scoped tokens and network-policy sets included, set-valued fields compare whatever
/// order the API lists their members in, and a scoped token or network-policy set missing from
/// the cluster is a difference
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn every_redis_record_comes_back_and_set_order_is_ignored() {
    // back up every value class, empty the target, and restore it
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    fixture::broad(harness).await;
    let _ = Box::pin(fixture::value_classes(harness)).await;
    let backup = harness.scratch("value-classes").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "4", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // the backup holds the fixture's scoped tokens and network-policy sets
    let redis: Value =
        serde_json::from_slice(&std::fs::read(backup.join("redis.json")).unwrap()).unwrap();
    for kind in ["scoped_tokens", "network_policy_sets"] {
        assert!(
            redis[kind]
                .as_array()
                .is_some_and(|records| !records.is_empty()),
            "the backup holds no {kind}"
        );
    }
    empty_target(harness).await;
    let restored = restore(harness, &backup, &[]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the restored notifications keep their TTLs, so the restore gives no default-TTL warning
    let stderr = String::from_utf8_lossy(&restored.stderr);
    let warned = stderr
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("default TTL"))
        .count();
    assert_eq!(warned, 0, "{stderr}");
    // nothing in Redis differs, and nothing is excused as a known loss
    let run = verify_cluster(harness, &backup);
    let differences = redis_differences(&run);
    assert!(
        differences.is_empty(),
        "Redis differences:\n{}",
        differences.join("\n")
    );
    assert_eq!(
        run.output.status.code(),
        Some(0),
        "{}",
        describe(&run.output)
    );
    assert!(run.report.get("known_losses").is_none(), "{:#}", run.report);
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(!stderr.contains("known loss"), "{stderr}");
    // a deleted scoped token and an emptied network-policy set are each a difference
    let token_name = redis["scoped_tokens"][0]["name"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .client()
        .await
        .users
        .delete_scoped_token(&token_name)
        .await
        .unwrap();
    let group = redis["network_policy_sets"][0]["group"]
        .as_str()
        .unwrap()
        .to_owned();
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let () = redis::cmd("DEL")
        .arg(format!(
            "{}:netpols:{group}",
            harness.conf.thorium.namespace
        ))
        .query_async(&mut *conn)
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    let keys: Vec<&str> = run
        .records
        .iter()
        .filter(|record| record.component == "redis")
        .filter_map(|record| record.key.as_deref())
        .collect();
    assert!(
        keys.iter().any(
            |key| key.starts_with("scoped_tokens/") && key.ends_with(&format!("/{token_name}"))
        ),
        "{keys:?}"
    );
    assert!(
        keys.contains(&format!("network_policy_sets/{group}").as_str()),
        "{keys:?}"
    );
}

/// After a restore of a backup taken before scoped tokens and network-policy sets were backed
/// up, which holds neither, `verify --cluster` passes: the restore rebuilds each group's
/// network-policy set from its images, and verify expects those sets
#[tokio::test]
async fn an_older_backup_without_the_newer_fields_verifies_after_restore() {
    // back up the broad fixture, whose images use network policies
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    fixture::broad(harness).await;
    let backup = harness.scratch("older-fields").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "4", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // drop the two newer fields, as an older API's backup lacks them, and fix the manifest
    let mut redis: Value =
        serde_json::from_slice(&std::fs::read(backup.join("redis.json")).unwrap()).unwrap();
    let fields = redis.as_object_mut().unwrap();
    let sets = fields.remove("network_policy_sets");
    assert!(
        sets.as_ref()
            .and_then(Value::as_array)
            .is_some_and(|sets| !sets.is_empty()),
        "the fixture holds no network-policy set"
    );
    fields.remove("scoped_tokens");
    let bytes = serde_json::to_vec(&redis).unwrap();
    std::fs::write(backup.join("redis.json"), &bytes).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    manifest["redis"]["len"] = Value::from(bytes.len());
    manifest["redis"]["sha256"] = Value::from(HEXLOWER.encode(&Sha256::digest(&bytes)));
    std::fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    // restore it, then verify the cluster against it
    empty_target(harness).await;
    let restored = restore(harness, &backup, &[]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let run = verify_cluster(harness, &backup);
    let differences = redis_differences(&run);
    assert!(
        differences.is_empty(),
        "Redis differences:\n{}",
        differences.join("\n")
    );
    assert_eq!(
        run.output.status.code(),
        Some(0),
        "{}",
        describe(&run.output)
    );
}

/// A cluster whose tool-instance runtimes were lost after the backup fails `verify --cluster`'s
/// Redis diff, naming the runtimes
#[tokio::test]
async fn lost_workload_runtimes_are_a_difference() {
    // one tool instance's average runtime in an otherwise fresh target, backed up with Redis
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let ns = harness.conf.thorium.namespace.clone();
    let runtimes = format!("{ns}:workload_runtimes");
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let () = redis::cmd("HSET")
        .arg(&runtimes)
        .arg(uuid::Uuid::new_v4().to_string())
        .arg(42.5)
        .query_async(&mut *conn)
        .await
        .unwrap();
    let backup = harness.scratch("lost-runtimes").join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", "redis", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // the unchanged cluster matches the backup
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.describe());
    // with the runtimes gone it no longer does, and the record names them
    let () = redis::cmd("DEL")
        .arg(&runtimes)
        .query_async(&mut *conn)
        .await
        .unwrap();
    let run = verify_cluster(harness, &backup);
    assert_eq!(run.output.status.code(), Some(2), "{}", run.describe());
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some("workload_runtimes")),
        "{}",
        run.describe()
    );
}
