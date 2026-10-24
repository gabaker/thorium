//! A restore into a dropped keyspace converges the keyspace to the backup's revision before it
//! loads, so the tables exist, and again after it loads, so every view exists without an API
//! restart
//!
//! The test drops the namespace's keyspace, which the binary's other tests would notice, so it is
//! the only one here.

mod common;

use common::cycle::describe;
use common::harness::Harness;
use common::record::{self, backup_of, restore};
use std::collections::BTreeSet;
use thorium::models::schema::{MigrationRecordKind, RevisionSnapshot};
use thorium::models::upgrades::SchemaBackend;

/// The objects released revisions created that no release reads, which a restore leaves uncreated
const UNUSED: [&str; 2] = ["events", "entities_by_name"];

/// Returns the names of the namespace's tables and views, sorted
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn objects(harness: &Harness) -> BTreeSet<String> {
    // the schema lists the tables and the views of the keyspace
    let scylla = harness.scylla().await;
    let mut names = BTreeSet::new();
    for cql in [
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
    ] {
        let rows = scylla
            .query_unpaged(cql, (harness.conf.thorium.namespace.as_str(),))
            .await
            .unwrap()
            .into_rows_result()
            .unwrap();
        names.extend(rows.rows::<(String,)>().unwrap().map(|row| row.unwrap().0));
    }
    names
}

/// `thoradm restore` runs `schema apply` before loading, converging to the backup's revision
/// snapshot so a dropped keyspace is recreated, and after loading, and every materialized view
/// exists after the restore without restarting the API
#[tokio::test]
async fn a_restore_recreates_a_dropped_keyspace_and_its_views() {
    // a backup at a revision with a committed snapshot, of a namespace holding a sample
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v02", "src", 2, &["scylla-role"]);
    let (backup, sha) = backup_of(harness, "dropped", &source).await;
    // the keyspace goes, with every table and view in it
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!("DROP KEYSPACE {}", harness.conf.thorium.namespace),
            (),
        )
        .await
        .unwrap();
    // the restore runs against nothing
    let report = backup.parent().unwrap().join("report");
    let restored = restore(harness, &backup, &report);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the keyspace holds the snapshot's objects, and the state tables, but no object no release
    // reads
    let snapshot = RevisionSnapshot::committed("2026-10-v02").unwrap().unwrap();
    let mut expected: BTreeSet<String> = snapshot
        .objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .map(|object| object.name.clone())
        .filter(|name| !UNUSED.contains(&name.as_str()))
        .collect();
    expected.extend([
        "schema_migrations".to_owned(),
        "schema_migration_issues".to_owned(),
    ]);
    assert_eq!(objects(harness).await, expected);
    // every view is built, and the API serves the sample it reads through a view
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert!(stderr.contains("no API restart is needed"), "{stderr}");
    let sample = harness.client().await.files.get(&sha).await;
    assert!(sample.is_ok(), "{sample:?}");
    // the backup's record came back with it
    let after = record::read(harness).await;
    let revision = after
        .get(MigrationRecordKind::Revision, "deployment")
        .unwrap();
    assert_eq!(revision.revision.as_deref(), Some("2026-10-v02"));
    let mut expected = source.clone();
    expected.sort();
    assert_eq!(after.issues, expected.issues);
}
