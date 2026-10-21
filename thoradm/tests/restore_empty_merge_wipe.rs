//! The restore modes: an empty target, `--merge` into a target holding a second fixture, and
//! `--wipe <namespace>` over that target plus excluded data, each run twice and compared by the
//! oracle after every run
//!
//! Running every mode twice takes tens of minutes, so that test is ignored by default and runs in
//! its own CI job (`restore-empty-merge-wipe`) or with `--include-ignored`; a restore into an empty
//! target, run once, runs by default.
//!
//! The tests restore, so each holds the binary's restore guard.

mod common;

use common::classes::KNOWN_BROKEN;
use common::cycle::{census, describe, wipe};
use common::fixture;
use common::harness::{Harness, bucket_for, list_keys};
use common::oracle::{Difference, Oracle, Snapshot};
use common::state::target_state;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Output, Stdio};
use thoradm::backup::coverage::{Coverage, Registry, Store};

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

/// Returns whether a snapshot table or view holds data the backup doesn't: an excluded table, or
/// a view of one
///
/// # Arguments
///
/// * `name` - The table or view
fn excluded_table(name: &str) -> bool {
    // a view takes its base table's coverage
    let base = Registry::classify(Store::ScyllaView, name)
        .and_then(|view| view.base_table)
        .unwrap_or(name);
    Registry::classify(Store::ScyllaTable, base)
        .is_some_and(|entry| matches!(entry.coverage, Coverage::Excluded { .. }))
}

/// Returns whether a bucket role holds data the backup doesn't
///
/// # Arguments
///
/// * `role` - The bucket role
fn excluded_role(role: &str) -> bool {
    // the registry's entry for the role decides
    Registry::classify(Store::S3, role)
        .is_some_and(|entry| matches!(entry.coverage, Coverage::Excluded { .. }))
}

/// Returns what a merge of a backup into a target leaves: the target's data with every backed-up
/// row and object of the backup written over it, the backup's Redis state, census streams
/// counting both, and the excluded data as the target held it
///
/// # Arguments
///
/// * `target` - The target before the merge
/// * `source` - The namespace the backup was taken from
fn merged(target: &Snapshot, source: &Snapshot) -> Snapshot {
    // rows: the target's, with the backup's written over them by primary key
    let mut expected = target.clone();
    for (table, rows) in &source.scylla {
        if excluded_table(table) {
            continue;
        }
        let into = expected.scylla.entry(table.clone()).or_default();
        for (key, row) in rows {
            into.insert(key.clone(), row.clone());
        }
    }
    for (table, ttls) in &source.ttls {
        let into = expected.ttls.entry(table.clone()).or_default();
        for (key, ttl) in ttls {
            into.insert(key.clone(), *ttl);
        }
    }
    // objects: the backup's bytes at every key it holds
    for ((role, key), object) in &source.s3 {
        if !excluded_role(role) {
            expected
                .s3
                .insert((role.clone(), key.clone()), object.clone());
        }
    }
    // Redis is replaced by the backup's
    expected.redis = source.redis.clone();
    // the rebuilt census counts every row both held
    let mut census: BTreeMap<String, Vec<String>> = target.census.clone();
    for (key, members) in &source.census {
        let into = census.entry(key.clone()).or_default();
        into.extend(members.iter().cloned());
        into.sort();
        into.dedup();
    }
    expected.census = census;
    expected
}

/// Returns what a wipe restore leaves: the backup's tables and Redis state, with the excluded
/// data as the target held it, and every object the target held, which the wipe doesn't touch,
/// with the backup's bytes written over any key both hold
///
/// # Arguments
///
/// * `target` - The target before the wipe
/// * `source` - The namespace the backup was taken from
fn wiped(target: &Snapshot, source: &Snapshot) -> Snapshot {
    // the source, but every excluded table from the target
    let mut expected = source.clone();
    expected.scylla.retain(|table, _| !excluded_table(table));
    for (table, rows) in &target.scylla {
        if excluded_table(table) {
            expected.scylla.insert(table.clone(), rows.clone());
        }
    }
    // objects: the target's own, with the backup's bytes at every key it holds
    expected.s3 = target.s3.clone();
    for ((role, key), object) in &source.s3 {
        if !excluded_role(role) {
            expected
                .s3
                .insert((role.clone(), key.clone()), object.clone());
        }
    }
    expected
}

/// Compares a found snapshot with the expected one, leaving out the entries a known finding
/// already explains, and panics listing every other difference
///
/// # Arguments
///
/// * `step` - The step being checked, for the message
/// * `expected` - The expected snapshot
/// * `found` - The snapshot taken after the step
fn assert_matches(step: &str, expected: &Snapshot, found: &Snapshot) {
    // every difference outside the known-broken entries fails the step
    let known: Vec<&str> = KNOWN_BROKEN
        .iter()
        .flat_map(|broken| broken.entries.iter().copied())
        .collect();
    let differences: Vec<Difference> = Oracle::default()
        .compare(expected, found)
        .into_iter()
        .filter(|difference| !known.contains(&difference.entry.as_str()))
        .collect();
    assert!(
        differences.is_empty(),
        "{step}: {} differences:\n{}",
        differences.len(),
        differences
            .iter()
            .take(60)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Seeds a second fixture into the target: data of its own, the source's samples-list rows with
/// another description, and one of the source's file objects with other bytes
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `source_rows` - The source's samples-list rows, as `SELECT JSON` returned them
/// * `overwritten` - A file object key of the source, written with other bytes
async fn second_fixture(harness: &Harness, source_rows: &[String], overwritten: &str) {
    // data of its own
    let _ = fixture::clean(harness).await;
    // rows at the source's primary keys with other values in a column no view keys on
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let insert = format!("INSERT INTO {ns}.samples_list JSON ?");
    for row in source_rows {
        let mut value: Value = serde_json::from_str(row).unwrap();
        value["description"] = Value::from("the target's own description");
        scylla
            .query_unpaged(insert.as_str(), (value.to_string(),))
            .await
            .unwrap();
    }
    // an object at a source key with other bytes
    common::objects::put(
        &harness.s3(),
        &bucket_for(&harness.conf, "files"),
        overwritten,
        &common::columns::Body::Bytes(b"the target's own bytes".to_vec()),
    )
    .await;
}

/// Adds data the backup doesn't hold: a stage log line, an event, and an ephemeral object
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn excluded_data(harness: &Harness) {
    // the events table an older release left, which restores keep like the other excluded data
    common::legacy::create_events(harness).await;
    // rows that don't expire, so the comparison sees them unchanged
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.logs (reaction, stage, bucket, position, line) \
                 VALUES (?, 'kept', 0, 0, 'a line every restore keeps') USING TTL 0"
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
    common::objects::put(
        &harness.s3(),
        &bucket_for(&harness.conf, "ephemeral"),
        "kept/scratch.bin",
        &common::columns::Body::Bytes(b"scratch every restore keeps".to_vec()),
    )
    .await;
}

/// A restore into an empty target leaves the namespace equal to the source
#[tokio::test]
async fn an_empty_target_restore_gives_the_source() {
    // the source: the clean fixture with its census rebuilt, snapshotted and backed up
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let _ = fixture::clean(harness).await;
    let rebuilt = census(harness);
    assert_eq!(rebuilt.status.code(), Some(0), "{}", describe(&rebuilt));
    // the source as the oracle sees it, which a restore into an empty target leaves
    let source = Snapshot::take(harness).await;
    // a full backup of it
    let backup = harness.scratch("empty").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "4", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // an empty target, restored once, equals the source
    empty_target(harness).await;
    let run = restore(harness, &backup, &[]);
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    assert_matches("empty target", &source, &Snapshot::take(harness).await);
}

/// Each restore mode, run twice, leaves the state the mode promises: an empty target equals the
/// source and refuses a plain re-run without change; a merge keeps the target's own data and
/// writes the backup's over shared keys; a wipe equals the source's tables with the excluded data and the target's objects kept
#[tokio::test]
#[ignore = "takes tens of minutes: runs in the restore-empty-merge-wipe CI job, or with --include-ignored"]
#[allow(clippy::too_many_lines)]
async fn each_restore_mode_gives_its_result_twice() {
    // the source: the clean fixture with its census rebuilt, snapshotted and backed up
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    empty_target(harness).await;
    let _ = fixture::clean(harness).await;
    let rebuilt = census(harness);
    assert_eq!(rebuilt.status.code(), Some(0), "{}", describe(&rebuilt));
    // the source as the oracle sees it, which a restore into an empty target leaves
    let source = Snapshot::take(harness).await;
    // a full backup of it
    let backup = harness.scratch("modes").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "4", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // its samples_list rows and one file object, which the second fixture writes over
    let source_rows: Vec<String> = harness
        .scylla()
        .await
        .query_unpaged(format!("SELECT JSON * FROM {ns}.samples_list"), &[])
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect();
    assert!(!source_rows.is_empty(), "the fixture has samples-list rows");
    let overwritten = list_keys(&harness.s3(), &bucket_for(&harness.conf, "files"))
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the fixture has file objects");
    // an empty target: equal to the source
    empty_target(harness).await;
    let run = restore(harness, &backup, &[]);
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    assert_matches("empty target", &source, &Snapshot::take(harness).await);
    // a plain re-run is refused and changes nothing; a --merge re-run leaves it equal
    let before = target_state(harness).await;
    let run = restore(harness, &backup, &[]);
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    assert_eq!(target_state(harness).await, before);
    let run = restore(harness, &backup, &["--merge"]);
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    assert_matches(
        "empty target, merged again",
        &source,
        &Snapshot::take(harness).await,
    );
    // --merge into a second fixture: the target's own data kept, the backup's over shared keys
    empty_target(harness).await;
    second_fixture(harness, &source_rows, &overwritten).await;
    let rebuilt = census(harness);
    assert_eq!(rebuilt.status.code(), Some(0), "{}", describe(&rebuilt));
    // each merge leaves the target's own data and the backup's over shared keys
    let target = Snapshot::take(harness).await;
    let expected = merged(&target, &source);
    for pass in ["merge", "merge again"] {
        let run = restore(harness, &backup, &["--merge"]);
        assert_eq!(run.status.code(), Some(0), "{pass}: {}", describe(&run));
        assert_matches(pass, &expected, &Snapshot::take(harness).await);
    }
    // --wipe over the second fixture and excluded data: the source's tables, with the excluded data
    // and the target's objects kept
    empty_target(harness).await;
    second_fixture(harness, &source_rows, &overwritten).await;
    excluded_data(harness).await;
    // each wipe leaves the source's tables with the excluded data and the target's objects kept
    let target = Snapshot::take(harness).await;
    let expected = wiped(&target, &source);
    for pass in ["wipe", "wipe again"] {
        let run = restore(harness, &backup, &["--wipe", &ns]);
        assert_eq!(run.status.code(), Some(0), "{pass}: {}", describe(&run));
        assert_matches(pass, &expected, &Snapshot::take(harness).await);
    }
}
