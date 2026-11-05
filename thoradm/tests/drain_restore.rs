//! A backup carries the recorded drain requests without their arguments, and a restore merges them
//! with the target's so no request's status moves backward
//!
//! Every test restores or writes the state tables, so each holds the binary's restore guard.

mod common;

use chrono::Utc;
use common::cycle::describe;
use common::harness::Harness;
use common::record::{self, at};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thoradm::backup::schema::{BackupSchemaRecord, SCHEMA_FILE};
use thoradm::backup::schema_restore::{Origin, restore_record};
use thorium::models::schema::MigrationRecordKind;
use thorium::models::schema::drain_requests::{
    CreatorMembership, DrainRequest, DrainRequestRow, DrainRequestStore, DrainStatus,
    PipelineIdentity,
};
use thorium::models::{ReactionStatus, RepoDependencyRequest};

/// The history the source deployments of these tests record
const STEPS: [&str; 2] = ["scylla-role", "finalize"];

/// Text only the seeded argument rows hold, so a backup that carried one is found by it
const ARGS_MARKER: &str = "drain-args-marker-7f3c9a";

/// The keyspace a test writes drain requests in, and the cluster config naming it
struct Target {
    /// The keyspace
    namespace: String,
    /// The cluster config thoradm reads to reach it
    cluster_conf: PathBuf,
}

/// Empties the harness's data and state tables and gives them as the target for drain requests
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn fresh_target(harness: &Harness) -> Target {
    // the data and Redis a fresh install has, and no state rows
    record::empty_target(harness).await;
    record::clear(harness).await;
    Target {
        namespace: harness.conf.thorium.namespace.clone(),
        cluster_conf: harness.cluster_conf.clone(),
    }
}

/// Prepares the drain request statements in a target's keyspace
///
/// # Arguments
///
/// * `harness` - The harness whose session is used
/// * `target` - The target
async fn store(harness: &Harness, target: &Target) -> DrainRequestStore {
    // the store the cancel and the resubmission write through
    DrainRequestStore::prepare(harness.scylla().await, &target.namespace)
        .await
        .expect("the drain request statements prepare")
}

/// Every drain request row of a target's keyspace, keyed by id
///
/// # Arguments
///
/// * `store` - The target's drain request store
async fn drain_rows(store: &DrainRequestStore) -> BTreeMap<String, DrainRequestRow> {
    // one page holds every row a test writes
    store
        .list_page(None, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect()
}

/// Writes a deployment's state rows into a target's keyspace
///
/// # Arguments
///
/// * `harness` - The harness whose session is used
/// * `target` - The target
/// * `tag` - Text that makes this deployment's values differ from another's
async fn deploy(harness: &Harness, target: &Target, tag: &str) {
    // a deployment at revision v02
    let rows = record::deployment("2026-10-v02", tag, 2, &STEPS);
    let scylla = harness.scylla().await;
    record::insert(&scylla, &target.namespace, &rows).await;
}

/// Backs up a target, tables and Redis without objects
///
/// # Arguments
///
/// * `harness` - The harness naming the scratch directory
/// * `target` - The target
/// * `name` - The scratch directory's name
fn backup_target(harness: &Harness, target: &Target, name: &str) -> PathBuf {
    // the backup the other helpers take, of the target's keyspace
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm_with(&target.cluster_conf)
        .args(["--workers", "2", "backup", "new", "no-s3", "-m", "2", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    out
}

/// Builds a recorded request's row with every cell the cancel writes
///
/// # Arguments
///
/// * `id` - The cancelled reaction's id
/// * `status` - How far its resubmission got
/// * `entry` - The reserved new reaction id, if one was reserved
/// * `result` - The new id or the reason, if the row is final or stale
fn request(
    id: &str,
    status: DrainStatus,
    entry: Option<&str>,
    result: Option<&str>,
) -> DrainRequestRow {
    // a request of one sample and one repo, with arguments kept apart
    let value = DrainRequest {
        group: "minerva".to_owned(),
        pipeline: "owls".to_owned(),
        samples: vec!["a".repeat(64)],
        repos: vec![RepoDependencyRequest {
            url: "github.com/curl/curl".to_owned(),
            commitish: None,
            kind: None,
        }],
        tags: vec!["batch-7".to_owned()],
        parent: None,
        trigger_depth: Some(0),
        creator: "athena".to_owned(),
        old_status: ReactionStatus::Started,
        last_activity: Some(at(7)),
        orphan: false,
        args_sha256: Some("0".repeat(64)),
        args_bytes: 42,
        buffers: false,
        cache: false,
        identity: PipelineIdentity {
            group: "minerva".to_owned(),
            name: "owls".to_owned(),
            creator: "athena".to_owned(),
            order_sha256: "1".repeat(64),
        },
        membership: CreatorMembership {
            held_role: true,
            role: "user".to_owned(),
        },
        binding: Some(vec![vec!["tool-1".to_owned()]]),
    };
    // a final or stale row has finished
    let finished = matches!(
        status,
        DrainStatus::Resubmitted | DrainStatus::Refused | DrainStatus::Stale
    )
    .then(|| at(9));
    DrainRequestRow {
        id: id.to_owned(),
        value: value.to_json().expect("a request renders"),
        attempt: Some("2026-10-01T00:05:00.000000000Z".to_owned()),
        status,
        entry: entry.map(ToOwned::to_owned),
        result: result.map(ToOwned::to_owned),
        revision: Some("2026-10-v04".to_owned()),
        started: Some(at(8)),
        finished,
        updated: Some(at(10)),
    }
}

/// A request row as the cells a backup's record carries it
///
/// # Arguments
///
/// * `row` - The row
fn raw(row: &DrainRequestRow) -> Value {
    // the kind and the raw text columns the row is written as
    json!({
        "kind": "drain_request", "id": row.id, "value": row.value, "attempt": row.attempt,
        "status": row.status.as_str(), "entry": row.entry, "result": row.result,
        "revision": row.revision, "started": row.started, "finished": row.finished,
        "updated": row.updated,
    })
}

/// An argument row as the cells an older backup's record carries it among its unknown kinds
///
/// # Arguments
///
/// * `id` - The cancelled reaction's id
fn raw_args(id: &str) -> Value {
    // the value is the reaction's argument JSON
    json!({
        "kind": "drain_request_args", "id": id,
        "value": format!("{{\"stage\":{{\"positionals\":[\"{ARGS_MARKER}\"]}}}}"),
        "updated": "2026-10-01T00:10:00Z",
    })
}

/// Writes request rows into the harness's namespace
///
/// # Arguments
///
/// * `store` - The drain request store
/// * `rows` - The rows to write
async fn seed(store: &DrainRequestStore, rows: &[DrainRequestRow]) {
    // each row is new, so each insert is applied
    for row in rows {
        assert!(
            store.insert_if_absent(row).await.unwrap(),
            "{} existed",
            row.id
        );
    }
}

/// Builds a backup's record of a deployment at revision v02 whose record carries some drain
/// rows, the way the thoradm that wrote it laid them out
///
/// # Arguments
///
/// * `drains` - The record's `drain_requests` rows
/// * `other` - The record's rows of kinds the writer didn't know
fn backup_record(drains: &[Value], other: &[Value]) -> BackupSchemaRecord {
    // the deployment's rows, leaving out the Job result a backup never carries
    let mut source = record::deployment("2026-10-v02", "new", 1, &STEPS);
    source
        .migrations
        .retain(|row| row.kind != MigrationRecordKind::JobResult);
    serde_json::from_value(json!({
        "format": 1, "thoradm_version": "1.8.1", "migrations": source.migrations,
        "issues": source.issues, "drain_requests": drains, "other": other,
    }))
    .expect("the record parses")
}

/// Restores a record into the harness's namespace the way a restore of a whole backup does
///
/// # Arguments
///
/// * `harness` - The harness whose session is used
/// * `target` - The target
/// * `backup` - The backup's record
///
/// # Returns
///
/// What the restore did with the record, as its summary carries it
async fn restore(harness: &Harness, target: &Target, backup: &BackupSchemaRecord) -> Value {
    // a restore at a fixed stamp, by the test
    let origin = Origin {
        reference: "/backups/drains",
        by: "test",
        at: Utc::now(),
        stamp: "2026-10-19T08:00:00.000000000Z",
        baseline: false,
    };
    let scylla = harness.scylla().await;
    let done = restore_record(&scylla, &target.namespace, backup, &origin)
        .await
        .expect("the record restores");
    serde_json::to_value(done).unwrap()
}

/// Says whether any file under a directory holds some text
///
/// # Arguments
///
/// * `dir` - The directory to search
/// * `needle` - The text to look for
fn any_file_holds(dir: &Path, needle: &str) -> bool {
    // every file, at any depth, read as bytes
    std::fs::read_dir(dir).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            return any_file_holds(&path, needle);
        }
        let bytes = std::fs::read(&path).unwrap();
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    })
}

/// The rows a set of request rows reads back as, keyed by id
///
/// # Arguments
///
/// * `rows` - The rows
fn by_id(rows: &[DrainRequestRow]) -> BTreeMap<String, DrainRequestRow> {
    // one entry per row
    rows.iter()
        .map(|row| (row.id.clone(), row.clone()))
        .collect()
}

/// Every drain request status survives a backup and a restore into a cluster with no record, with
/// every cell the cancel and the resubmission write, and no argument row comes back
#[tokio::test]
async fn drain_request_rows_survive_backup_and_restore() {
    // a deployment holding one request of each status, and one request's arguments
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let target = fresh_target(harness).await;
    deploy(harness, &target, "src").await;
    let store = store(harness, &target).await;
    let seeded = [
        request("r-1", DrainStatus::Recorded, None, None),
        request("r-2", DrainStatus::Resubmitting, Some("new-2"), None),
        request(
            "r-3",
            DrainStatus::Resubmitted,
            Some("new-3"),
            Some("new-3"),
        ),
        request(
            "r-4",
            DrainStatus::Refused,
            None,
            Some("the pipeline changed"),
        ),
        request("r-5", DrainStatus::Stale, None, Some("idle for 31 days")),
    ];
    seed(&store, &seeded).await;
    store
        .put_args("r-1", &format!("{{\"marker\":\"{ARGS_MARKER}\"}}"))
        .await
        .unwrap();
    let backup = backup_target(harness, &target, "drains-survive");
    // the record carries each request's cells under its own field
    let text = std::fs::read_to_string(backup.join(SCHEMA_FILE)).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();
    let ids: Vec<&str> = json["drain_requests"]
        .as_array()
        .expect("the record carries drain requests")
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["r-1", "r-2", "r-3", "r-4", "r-5"]);
    // restored into a cluster that holds no record, every row comes back as it was
    let target = fresh_target(harness).await;
    let report = backup.parent().unwrap().join("report");
    let output = harness
        .thoradm_with(&target.cluster_conf)
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(&backup)
        .arg("--report-dir")
        .arg(&report)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to run thoradm");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let store = self::store(harness, &target).await;
    assert_eq!(drain_rows(&store).await, by_id(&seeded));
    // the arguments were never backed up, so none come back
    assert_eq!(store.get_args("r-1").await.unwrap(), None);
}

/// An argument row, even one whose request row is gone, reaches no file of a backup
#[tokio::test]
async fn drain_request_args_are_not_backed_up() {
    // a deployment holding only a request's arguments
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::empty_target(harness).await;
    record::set(
        harness,
        &record::deployment("2026-10-v02", "args", 2, &STEPS),
    )
    .await;
    let target = Target {
        namespace: harness.conf.thorium.namespace.clone(),
        cluster_conf: harness.cluster_conf.clone(),
    };
    let store = store(harness, &target).await;
    store
        .put_args("a-1", &format!("{{\"marker\":\"{ARGS_MARKER}\"}}"))
        .await
        .unwrap();
    // a request row written without a condition, which a keyspace on tablets takes too
    record::insert_raw(harness, "drain_request", "a-2", "{}").await;
    // the typed rows read beside a drain request row
    let rows = record::read(harness).await;
    assert!(
        rows.get(MigrationRecordKind::Revision, "deployment")
            .is_some()
    );
    let backup = record::backup_now(harness, "drains-args");
    // no file of the backup holds the arguments or names their kind
    assert!(!any_file_holds(&backup, ARGS_MARKER));
    let text = std::fs::read_to_string(backup.join(SCHEMA_FILE)).unwrap();
    assert!(!text.contains("drain_request_args"), "{text}");
    // the target's own row is left as it was
    assert!(store.get_args("a-1").await.unwrap().is_some());
}

/// A restore keeps the requests only the target recorded and their arguments, brings back the
/// ones only the backup holds, and counts the rows it wrote
#[tokio::test]
async fn restore_merges_drain_requests_with_the_targets() {
    // a target that recorded one request of its own and resubmitted one the backup also holds
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let target = fresh_target(harness).await;
    deploy(harness, &target, "old").await;
    let store = store(harness, &target).await;
    let target_only = request("m-target", DrainStatus::Recorded, None, None);
    let shared = request(
        "m-shared",
        DrainStatus::Resubmitted,
        Some("n-1"),
        Some("n-1"),
    );
    seed(&store, &[target_only.clone(), shared.clone()]).await;
    store.put_args("m-target", "{\"kept\":true}").await.unwrap();
    // a backup holding a request of its own and an earlier copy of the shared one
    let backup_only = request("m-backup", DrainStatus::Recorded, None, None);
    let shared_earlier = request("m-shared", DrainStatus::Recorded, None, None);
    let done = restore(
        harness,
        &target,
        &backup_record(&[raw(&backup_only), raw(&shared_earlier)], &[]),
    )
    .await;
    // the target's rows stand, and the backup's own row is added
    assert_eq!(
        drain_rows(&store).await,
        by_id(&[target_only, shared, backup_only])
    );
    assert_eq!(
        store.get_args("m-target").await.unwrap().as_deref(),
        Some("{\"kept\":true}")
    );
    // only the added row was written
    assert_eq!(done["drain_requests"], json!(1), "{done}");
}

/// A restore writes a backup's request only over a target row that ranks lower, and a winning
/// backup row keeps the reservation the target's row holds
#[tokio::test]
async fn restore_never_moves_a_drain_request_backward() {
    // four target rows, one per case
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let target = fresh_target(harness).await;
    deploy(harness, &target, "old").await;
    let store = store(harness, &target).await;
    let ahead = request(
        "b-ahead",
        DrainStatus::Resubmitted,
        Some("n-a"),
        Some("n-a"),
    );
    let behind = request("b-behind", DrainStatus::Recorded, None, None);
    let tie = request(
        "b-tie",
        DrainStatus::Stale,
        None,
        Some("outside the attempt"),
    );
    let reserved = request(
        "b-reserved",
        DrainStatus::Resubmitting,
        Some("n-target"),
        None,
    );
    seed(
        &store,
        &[ahead.clone(), behind.clone(), tie.clone(), reserved.clone()],
    )
    .await;
    // the backup's copy of each
    let ahead_backup = request("b-ahead", DrainStatus::Recorded, None, None);
    let behind_backup = request(
        "b-behind",
        DrainStatus::Resubmitted,
        Some("n-b"),
        Some("n-b"),
    );
    let tie_backup = request("b-tie", DrainStatus::Recorded, None, None);
    let reserved_backup = request(
        "b-reserved",
        DrainStatus::Resubmitted,
        Some("n-backup"),
        Some("n-backup"),
    );
    let done = restore(
        harness,
        &target,
        &backup_record(
            &[
                raw(&ahead_backup),
                raw(&behind_backup),
                raw(&tie_backup),
                raw(&reserved_backup),
            ],
            &[],
        ),
    )
    .await;
    let after = drain_rows(&store).await;
    // a backup row behind the target's never replaces it
    assert_eq!(after["b-ahead"], ahead);
    // a backup row ahead of the target's replaces its status, result, and finish time
    let moved = &after["b-behind"];
    assert_eq!(moved.status, DrainStatus::Resubmitted);
    assert_eq!(moved.result.as_deref(), Some("n-b"));
    assert_eq!(moved.entry.as_deref(), Some("n-b"));
    assert_eq!(moved.finished, behind_backup.finished);
    // a tie keeps the target's row, so a stale row never turns back into a recorded one
    assert_eq!(after["b-tie"], tie);
    // a winning backup row keeps the target's set reservation
    let kept = &after["b-reserved"];
    assert_eq!(kept.status, DrainStatus::Resubmitted);
    assert_eq!(kept.entry.as_deref(), Some("n-target"));
    assert_eq!(kept.result.as_deref(), Some("n-backup"));
    // the two winning rows were written
    assert_eq!(done["drain_requests"], json!(2), "{done}");
}

/// A backup an older thoradm took carries drain rows among its unknown kinds: the requests go
/// through the same merge, and the argument rows are dropped and counted
#[tokio::test]
async fn unknown_kind_drain_rows_are_merged_and_args_dropped() {
    // a target that resubmitted one of the backup's requests
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let target = fresh_target(harness).await;
    deploy(harness, &target, "old").await;
    let store = store(harness, &target).await;
    let resubmitted = request("u-1", DrainStatus::Resubmitted, Some("n-u"), Some("n-u"));
    seed(&store, std::slice::from_ref(&resubmitted)).await;
    // the older backup's unknown rows: both requests as recorded, and both argument rows
    let first = request("u-1", DrainStatus::Recorded, None, None);
    let second = request("u-2", DrainStatus::Recorded, None, None);
    let other = [raw(&first), raw(&second), raw_args("u-1"), raw_args("u-2")];
    let done = restore(harness, &target, &backup_record(&[], &other)).await;
    // the resubmitted row stands and the unknown request is added
    assert_eq!(drain_rows(&store).await, by_id(&[resubmitted, second]));
    // no argument row was written, and both were counted as dropped
    for id in ["u-1", "u-2"] {
        assert_eq!(store.get_args(id).await.unwrap(), None, "{id}");
    }
    assert_eq!(done["drain_args_dropped"], json!(2), "{done}");
    assert_eq!(done["drain_requests"], json!(1), "{done}");
}
