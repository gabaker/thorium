//! A backup restores across revisions by the documented path: the restore converges the keyspace
//! to the backup's revision before it loads, so a column a later contract step dropped is back for
//! the rows that carry it, and the record names the older revision for the newer image's upgrade to
//! run from; a thoradm too old for the backup's archive format refuses before writing anything
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::describe;
use common::harness::Harness;
use common::record::{self, backup_of, restore};
use thorium::models::schema::MigrationRecordKind;
use thorium::models::upgrades::{AppliedStep, Catalog, RevisionId, UpgradeState, latest};

/// The revision the backup is taken at
const BACKED_UP: &str = "2026-10-v02";

/// The names of a table's columns in the test namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `table` - The table
async fn columns(harness: &Harness, table: &str) -> Vec<String> {
    // the schema tables list them
    let scylla = harness.scylla().await;
    let mut names = scylla
        .query_unpaged(
            "SELECT column_name FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
            (harness.conf.thorium.namespace.as_str(), table),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Build the upgrade state a deployment's rows describe, as the planner reads it
///
/// # Arguments
///
/// * `rows` - The state rows
fn state_of(rows: &record::Rows) -> UpgradeState {
    // the revision row and the history entries
    let revision = rows
        .get(MigrationRecordKind::Revision, "deployment")
        .and_then(|row| row.revision.clone())
        .expect("a revision row");
    let mut state = UpgradeState::new(revision.parse::<RevisionId>().unwrap(), "test");
    state.applied = rows
        .kind(MigrationRecordKind::History)
        .iter()
        .map(|row| row.entry.clone().expect("a history row holds an entry").0)
        .collect::<Vec<AppliedStep>>();
    state
}

/// A backup taken before a contract step dropped a column restores into the deployment past that
/// step: the restore re-adds the column from the backup's revision, loads every row, records the
/// older revision, and the next plan from it holds the steps of the later revisions, which re-run
#[tokio::test]
async fn a_backup_from_before_a_dropped_column_restores_by_converging_down() {
    // a backup at the second revision of a namespace holding a sample
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment(BACKED_UP, "old", 1, &["scylla-role"]);
    let (backup, sha) = backup_of(harness, "across", &source).await;
    // the deployment past a later contract step holds no data, and the step dropped a column the
    // backup's rows carry
    record::empty_target(harness).await;
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!(
                "ALTER TABLE {}.nodes DROP health",
                harness.conf.thorium.namespace
            ),
            (),
        )
        .await
        .unwrap();
    assert!(
        !columns(harness, "nodes")
            .await
            .contains(&"health".to_owned())
    );
    // the deployment is past it: its record names the latest revision
    let target = record::deployment(latest().as_str(), "new", 2, &[]);
    record::set(harness, &target).await;
    // the restore, run with the backup's thoradm, converges to the backup's revision first
    let report = backup.parent().unwrap().join("report");
    let restored = restore(harness, &backup, &report);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the column is back, and every row reads: the API serves the sample through its view
    assert!(
        columns(harness, "nodes")
            .await
            .contains(&"health".to_owned())
    );
    let sample = harness.client().await.files.get(&sha).await;
    assert!(sample.is_ok(), "{sample:?}");
    // the record names the older revision, so the newer image's upgrade starts from there
    let after = record::read(harness).await;
    let revision = after
        .get(MigrationRecordKind::Revision, "deployment")
        .unwrap();
    assert_eq!(revision.revision.as_deref(), Some(BACKED_UP));
    let plan = Catalog::PRODUCTION
        .plan(&state_of(&after), &latest())
        .unwrap();
    let steps = plan
        .iter()
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    assert!(steps.contains(&"schema-apply"), "{steps:?}");
}

/// A thoradm that doesn't read the backup's archive format refuses before writing anything, naming
/// the version to use
#[tokio::test]
async fn a_backup_in_a_newer_archive_format_is_refused_before_writing() {
    // a backup, and a target holding its own record
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment(BACKED_UP, "old", 1, &["scylla-role"]);
    let (backup, _) = backup_of(harness, "newer", &source).await;
    let target = record::deployment(latest().as_str(), "new", 2, &["a"]);
    record::set(harness, &target).await;
    let before = record::read(harness).await;
    // the manifest says a newer thoradm wrote the archives in a format this one can't read
    let path = backup.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["format_version"] = serde_json::json!(99);
    manifest["thoradm_version"] = serde_json::json!("9.9.9");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    // the restore refuses, naming the version that reads it
    let report = backup.parent().unwrap().join("report");
    let refused = restore(harness, &backup, &report);
    assert_ne!(refused.status.code(), Some(0), "{}", describe(&refused));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("use thoradm 9.9.9"), "{stderr}");
    // nothing was written: the target's record is as it was
    let after = record::read(harness).await;
    assert_eq!(after.ids(), before.ids());
    let revision = after
        .get(MigrationRecordKind::Revision, "deployment")
        .unwrap();
    assert_eq!(revision.revision.as_deref(), Some(latest().as_str()));
}
