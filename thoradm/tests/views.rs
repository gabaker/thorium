//! Restore drops the declared materialized views of the tables it loads and leaves the legacy view
//! alone, and its `schema apply` creates the dropped views once the tables are loaded, so the API
//! serves view-backed routes again without a restart
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::harness::Harness;
use common::names::Names;
use common::seed;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thoradm::backup::coverage::{Registry, Store};

/// The components these tests back up: every table component and Redis, without objects
const COMPONENTS: &str = "no-s3";

/// Returns the names of the namespace's materialized views, sorted
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn views(harness: &Harness) -> Vec<String> {
    // the schema lists every view of the keyspace
    let scylla = harness.scylla().await;
    let mut names: Vec<String> = scylla
        .query_unpaged(
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
            (harness.conf.thorium.namespace.as_str(),),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect();
    names.sort();
    names
}

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

/// Seeds a sample and backs up every table and Redis, returning the backup and the sample
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
async fn seeded_backup(harness: &Harness, name: &str) -> (PathBuf, String) {
    // one sample in a group of its own, in a target no other test left data in
    empty_target(harness).await;
    // a group with one sample in it
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("minerva");
    seed::group(&client, &group).await;
    let sha = seed::sample(&client, &group, "owl.bin", names.suffix.as_bytes()).await;
    // back up every table and Redis
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", COMPONENTS, "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    (out, sha)
}

/// Restores a backup without a terminal, optionally with its report in a named directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup
/// * `report_dir` - The report directory to name, if any
fn restore(harness: &Harness, backup: &Path, report_dir: Option<&Path>) -> Output {
    // confirm with --confirm
    let mut command = harness.thoradm();
    command
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(backup)
        .stdin(Stdio::null());
    if let Some(dir) = report_dir {
        command.arg("--report-dir").arg(dir);
    }
    command.output().expect("failed to run thoradm")
}

/// A restore drops every declared view of the tables it loads and its `schema apply` creates
/// them again, so the same views exist afterwards, no view statements are saved, and the API
/// serves a sample without a restart
#[tokio::test]
async fn declared_views_are_dropped_and_recreated_by_the_restore() {
    // a sample, and the namespace's views before the restore
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, sha) = seeded_backup(harness, "recreated").await;
    let before = views(harness).await;
    let (legacy, declared): (Vec<_>, Vec<_>) = Registry::backed_up(Store::ScyllaView)
        .map(|entry| (entry.name, entry.legacy))
        .partition(|(_, legacy)| *legacy);
    let declared: Vec<&str> = declared.into_iter().map(|(name, _)| name).collect();
    let legacy: Vec<&str> = legacy.into_iter().map(|(name, _)| name).collect();
    // a fresh install holds exactly the declared views, and no view is legacy today
    assert_eq!(before.len(), 13, "{before:?}");
    assert_eq!(declared.len(), 13, "{declared:?}");
    assert!(legacy.is_empty(), "{legacy:?}");
    // restore into the emptied target, the API running all along
    empty_target(harness).await;
    let report = backup.parent().unwrap().join("report");
    let restored = restore(harness, &backup, Some(&report));
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr);
    // the same views exist, and every declared one was dropped
    assert_eq!(views(harness).await, before);
    let ns = &harness.conf.thorium.namespace;
    for view in &declared {
        assert!(
            stderr.contains(&format!("Dropped view {ns}.{view}")),
            "{view} wasn't dropped:\n{stderr}"
        );
    }
    // no statements were saved
    assert!(
        !report.join("views.cql").exists(),
        "a views.cql was written"
    );
    // the API serves the sample, which it reads through a view, without a restart
    let client = harness.client().await;
    let sample = client.files.get(&sha).await;
    assert!(sample.is_ok(), "{sample:?}");
    assert!(stderr.contains("no API restart is needed"), "{stderr}");
}

/// A restore into a keyspace an older release created treats the re-declared `entities_by_name`
/// view like every other declared view: the older keyspace's copy has the declared shape, so the
/// restore drops and recreates it and the view exists afterwards
#[tokio::test]
async fn the_re_declared_view_is_recreated_in_older_keyspaces() {
    // a backup of a fresh install, which declares the view
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "legacy-view").await;
    let view = "entities_by_name".to_owned();
    assert!(views(harness).await.contains(&view));
    // shape the target like a keyspace an older release created, which already holds the view
    empty_target(harness).await;
    common::legacy::create_entities_by_name(harness).await;
    let restored = restore(harness, &backup, None);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the view was dropped and recreated like every declared view
    let after = views(harness).await;
    assert!(after.contains(&view), "{after:?}");
    let stderr = String::from_utf8_lossy(&restored.stderr);
    let ns = &harness.conf.thorium.namespace;
    assert!(
        stderr.contains(&format!("Dropped view {ns}.{view}")),
        "the re-declared view wasn't dropped:\n{stderr}"
    );
}
