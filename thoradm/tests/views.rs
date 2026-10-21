//! Restore saves the statements of the materialized views it drops, drops exactly the views the
//! registry lists, and recreates them once the tables are loaded, so the API serves view-backed
//! routes again without a restart
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::harness::Harness;
use common::names::Names;
use common::seed;
use common::state::target_state;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thoradm::backup::coverage::{Registry, Store};
use thoradm::backup::failures::{FAILURES_FILE, Operation, read_records};
use thoradm::backup::views::{ViewStatements, view_status};

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

/// Returns the `CREATE MATERIALIZED VIEW` statement Scylla gives for a view
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `view` - The view
async fn view_statement(harness: &Harness, view: &str) -> String {
    // server-side DESCRIBE returns the statement in its create_statement column
    let scylla = harness.scylla().await;
    let rows = scylla
        .query_unpaged(
            format!(
                "DESCRIBE MATERIALIZED VIEW {}.{view}",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let (_, _, _, statement) = rows
        .first_row::<(String, String, String, String)>()
        .unwrap();
    statement
}

/// Drops a view, as an interrupted restore leaves it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `view` - The view
async fn drop_view(harness: &Harness, view: &str) {
    // drop it outright
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!(
                "DROP MATERIALIZED VIEW {}.{view}",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap();
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

/// A restore drops every registered view of the tables it loads, both entity views included,
/// saves their statements to `views.cql`, and recreates them, so the same views exist afterwards
/// and the API serves a sample without a restart
#[tokio::test]
async fn views_are_saved_dropped_and_recreated_by_the_restore() {
    // a sample, and the namespace's views before the restore
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, sha) = seeded_backup(harness, "recreated").await;
    let before = views(harness).await;
    let registered: Vec<&str> = Registry::backed_up(Store::ScyllaView)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(before.len(), 13, "{before:?}");
    assert_eq!(registered.len(), 13, "{registered:?}");
    // restore into the emptied target, the API running all along
    empty_target(harness).await;
    let report = backup.parent().unwrap().join("report");
    let restored = restore(harness, &backup, Some(&report));
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr);
    // the same views exist, and every registered one was dropped during the restore
    assert_eq!(views(harness).await, before);
    let ns = &harness.conf.thorium.namespace;
    for view in &registered {
        assert!(
            stderr.contains(&format!("Dropped view {ns}.{view}")),
            "{view} wasn't dropped:\n{stderr}"
        );
        // and its build was reported finished, with how long it took
        assert!(
            stderr.contains(&format!("View {ns}.{view} built after")),
            "{view}'s build wasn't reported:\n{stderr}"
        );
    }
    // views.cql holds every view's statement
    let saved = std::fs::read_to_string(report.join("views.cql")).unwrap();
    for view in &registered {
        assert!(
            saved.contains(&format!("CREATE MATERIALIZED VIEW {ns}.{view} ")),
            "views.cql lacks {view}:\n{saved}"
        );
    }
    // the API serves the sample, which it reads through a view, without a restart
    let client = harness.client().await;
    let sample = client.files.get(&sha).await;
    assert!(sample.is_ok(), "{sample:?}");
    assert!(stderr.contains("no API restart is needed"), "{stderr}");
}

/// A view an interrupted restore left dropped comes back from the newest earlier restore's
/// `views.cql`, which the re-run copies into its own; without such a file the preflight refuses,
/// naming the view and the fixes, and changes nothing
#[tokio::test]
async fn a_view_an_earlier_restore_dropped_comes_back_from_its_views_cql() {
    // a backup, and the samples view's statement as the earlier restore saved it
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "earlier").await;
    let statement = view_statement(harness, "samples").await;
    let earlier = backup.join("reports").join("restore-20261001T000000.000Z");
    std::fs::create_dir_all(&earlier).unwrap();
    std::fs::write(earlier.join("views.cql"), format!("{statement}\n")).unwrap();
    // the earlier restore was interrupted after dropping the view
    empty_target(harness).await;
    drop_view(harness, "samples").await;
    // the re-run takes the statement from the earlier file and recreates the view
    let report = backup.parent().unwrap().join("rerun-report");
    let restored = restore(harness, &backup, Some(&report));
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert!(views(harness).await.contains(&"samples".to_owned()));
    assert!(
        stderr.contains(&earlier.join("views.cql").display().to_string()),
        "the re-run doesn't name the file it took the statement from:\n{stderr}"
    );
    let saved = std::fs::read_to_string(report.join("views.cql")).unwrap();
    let ns = &harness.conf.thorium.namespace;
    assert!(
        saved.contains(&format!("CREATE MATERIALIZED VIEW {ns}.samples ")),
        "{saved}"
    );
    // with no earlier file, the preflight refuses, naming the view and both fixes
    std::fs::remove_dir_all(backup.join("reports")).unwrap();
    std::fs::remove_dir_all(&report).unwrap();
    empty_target(harness).await;
    drop_view(harness, "samples").await;
    let before = target_state(harness).await;
    let refused = restore(harness, &backup, Some(&report));
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let records = read_records(&report.join(FAILURES_FILE)).unwrap();
    let refusal = records
        .iter()
        .find(|record| record.operation == Operation::Preflight)
        .unwrap_or_else(|| panic!("no preflight record: {records:?}"));
    assert_eq!(refusal.object.as_deref(), Some("views"), "{refusal:?}");
    assert!(
        refusal.error.contains(&format!("{ns}.samples")),
        "{refusal:?}"
    );
    assert!(refusal.error.contains("views.cql"), "{refusal:?}");
    assert!(refusal.error.contains("API pod"), "{refusal:?}");
    assert_eq!(target_state(harness).await, before);
    // bring the view back the way the refusal says, so later tests have it
    let _ = Box::pin(thorium::utils::Shared::new(harness.conf.clone())).await;
    assert!(views(harness).await.contains(&"samples".to_owned()));
}

/// A missing view is taken only from an earlier `views.cql` statement for this namespace's
/// keyspace: a newer file another namespace's restore of the same backup left is skipped, and
/// with only that file the preflight refuses and the other namespace's statement never runs
#[tokio::test]
async fn a_missing_view_never_comes_from_another_namespaces_views_cql() {
    // an earlier restore of this backup into this namespace, then a newer one into another
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "foreign").await;
    let ns = harness.conf.thorium.namespace.clone();
    let statement = view_statement(harness, "samples").await;
    let foreign = statement.replace(&format!("{ns}."), &format!("{ns}_elsewhere."));
    let own = backup.join("reports").join("restore-20261001T000000.000Z");
    let other = backup.join("reports").join("restore-20261002T000000.000Z");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("views.cql"), format!("{statement}\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("views.cql"), format!("{foreign}\n")).unwrap();
    // this namespace's view is missing, as an interrupted restore leaves it
    empty_target(harness).await;
    drop_view(harness, "samples").await;
    // the re-run takes this namespace's statement, from the older file
    let report = backup.parent().unwrap().join("foreign-report");
    let restored = restore(harness, &backup, Some(&report));
    let stderr = String::from_utf8_lossy(&restored.stderr);
    let saved = std::fs::read_to_string(report.join("views.cql")).unwrap_or_default();
    assert!(
        !stderr.contains(&other.join("views.cql").display().to_string())
            && !saved.contains("_elsewhere"),
        "the restore took another namespace's statement:\n{saved}\n{}",
        describe(&restored)
    );
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    assert!(views(harness).await.contains(&"samples".to_owned()));
    assert!(
        saved.contains(&format!("CREATE MATERIALIZED VIEW {ns}.samples ")),
        "{saved}"
    );
    // with only the other namespace's file, the preflight refuses
    std::fs::remove_dir_all(&own).unwrap();
    std::fs::remove_dir_all(&report).unwrap();
    for dir in std::fs::read_dir(backup.join("reports")).unwrap() {
        let dir = dir.unwrap().path();
        if dir != other {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    empty_target(harness).await;
    drop_view(harness, "samples").await;
    let refused = restore(harness, &backup, Some(&report));
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let records = read_records(&report.join(FAILURES_FILE)).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.operation == Operation::Preflight
                && record.object.as_deref() == Some("views")),
        "{records:?}"
    );
    // bring the view back, so later tests have it
    let _ = Box::pin(thorium::utils::Shared::new(harness.conf.clone())).await;
    assert!(views(harness).await.contains(&"samples".to_owned()));
}

/// One view whose statement fails doesn't leave the views after it dropped, and a view another
/// client created first counts as recreated
#[tokio::test]
async fn one_failing_view_leaves_the_others_recreated() {
    // the statements of two views, both dropped
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = harness.conf.thorium.namespace.clone();
    let samples = view_statement(harness, "samples").await;
    let tags = view_statement(harness, "tags_by_item").await;
    drop_view(harness, "samples").await;
    drop_view(harness, "tags_by_item").await;
    // the first statement fails, the second is sound
    let mut statements = ViewStatements::default();
    statements.statements.insert(
        "samples".to_owned(),
        "CREATE MATERIALIZED VIEW broken".to_owned(),
    );
    statements
        .statements
        .insert("tags_by_item".to_owned(), tags.clone());
    let result = statements.recreate(&scylla, &ns).await;
    let existing = views(harness).await;
    // put both back whatever happened
    if !existing.contains(&"tags_by_item".to_owned()) {
        scylla.query_unpaged(tags.as_str(), &[]).await.unwrap();
    }
    scylla.query_unpaged(samples.as_str(), &[]).await.unwrap();
    assert!(
        existing.contains(&"tags_by_item".to_owned()),
        "one failing statement left tags_by_item dropped: {result:?}"
    );
    let result = format!("{result:?}");
    assert!(result.contains("samples"), "{result}");
    // a view that exists by the time its statement runs, as an API pod's schema setup can make
    // it, counts as recreated
    let mut statements = ViewStatements::default();
    statements
        .statements
        .insert("samples".to_owned(), samples.clone());
    let raced = statements
        .recreate_missing(&scylla, &ns, &std::collections::BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(raced.views, vec!["samples".to_owned()], "{raced:?}");
    assert!(raced.failed.is_empty(), "{raced:?}");
    // a view that exists with another definition than the saved statement's is recorded
    let mut statements = ViewStatements::default();
    statements.statements.insert(
        "samples".to_owned(),
        tags.replace("tags_by_item", "samples"),
    );
    let differing = statements
        .recreate_missing(&scylla, &ns, &std::collections::BTreeSet::new())
        .await
        .unwrap();
    assert!(differing.views.is_empty(), "{differing:?}");
    assert_eq!(differing.failed.len(), 1, "{differing:?}");
    assert_eq!(differing.failed[0].0, "samples", "{differing:?}");
    assert!(
        differing.failed[0].1.contains("another definition"),
        "{differing:?}"
    );
}

/// A restore's wait for the views names each view as it finishes and sums up once all are built
#[tokio::test]
async fn the_wait_for_views_logs_the_views_it_finished() {
    // restore a backup of one sample into an emptied target
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = seeded_backup(harness, "progress").await;
    empty_target(harness).await;
    let restored = restore(harness, &backup, None);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr);
    // the summary says every view is built, naming each
    let ns = &harness.conf.thorium.namespace;
    let summary = stderr
        .lines()
        .find(|line| line.contains("views built after"))
        .unwrap_or_else(|| panic!("no summary line:\n{stderr}"));
    assert!(summary.contains("All 13 views built"), "{summary}");
    assert!(summary.contains("samples built"), "{summary}");
    // every node reports each view's build as succeeded, from the one read of its rows
    let status = view_status(&*harness.scylla().await, ns, "samples")
        .await
        .unwrap();
    assert!(status.built(), "{status:?}");
    assert_eq!(status.succeeded, status.reported);
    assert!(status.reported >= 1);
}
