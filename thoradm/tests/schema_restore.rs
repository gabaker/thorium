//! A restore runs `schema apply` around the load and writes the backup's schema record back,
//! keeping the history it replaces and every consumed approval code
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::describe;
use common::harness::Harness;
use common::record::{self, Rows, backup_of, restore};
use common::state::target_state;
use std::path::Path;
use thoradm::backup::failures::{FAILURES_FILE, Operation, read_records};
use thorium::models::schema::{MigrationRecordKind, hash_files_password};
use thorium::models::upgrades::{AppliedStep, Catalog, RevisionId, UpgradeState, latest};

/// The history the source deployments of these tests record
const STEPS: [&str; 2] = ["scylla-role", "finalize"];

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

/// Runs a restore that must succeed, and returns its report directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup
/// * `name` - The report directory's name under the backup's scratch directory
fn restored(harness: &Harness, backup: &Path, name: &str) -> std::path::PathBuf {
    // restore with the report beside the backup
    let report = backup.parent().unwrap().join(name);
    let output = restore(harness, backup, &report);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    report
}

/// The entries of a history, parsed from the rows, oldest first
///
/// # Arguments
///
/// * `rows` - The state rows
fn history(rows: &Rows) -> Vec<AppliedStep> {
    // each history row holds one entry
    rows.kind(MigrationRecordKind::History)
        .iter()
        .map(|row| row.entry.clone().expect("a history row holds an entry").0)
        .collect()
}

/// Restoring writes every row of the backup's record back unchanged: the revision, the backends'
/// fingerprints and layouts, the run rows, the worker and issue rows, and the consumed and archived
/// history, with the history ending in a `Restored` entry; the restoring cluster's own
/// data-shaping settings stand, and a Job result the target held stays
#[tokio::test]
async fn restore_writes_the_backups_record_back() {
    // a backup of one deployment, restored into a target holding another's record
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v02", "src", 2, &STEPS);
    let (backup, _) = backup_of(harness, "write-back", &source).await;
    let target = record::deployment("2026-10-v02", "tgt", 2, &["scylla-role"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    // every row of the source that isn't a replaced kind or a setting is back as it was
    for row in &source.migrations {
        match row.kind {
            MigrationRecordKind::Backend
            | MigrationRecordKind::Run
            | MigrationRecordKind::Revision => {
                assert_eq!(
                    after.get(row.kind, &row.id),
                    Some(row),
                    "{}/{}",
                    row.kind.as_str(),
                    row.id
                );
            }
            _ => (),
        }
    }
    let mut sorted = source.clone();
    sorted.sort();
    assert_eq!(after.issues, sorted.issues);
    // the history is the source's, then the Restored entry
    let entries = history(&after);
    assert_eq!(entries.len(), STEPS.len() + 1, "{entries:?}");
    assert_eq!(entries[..STEPS.len()], history(&source)[..]);
    assert_eq!(entries.last().unwrap().step, "restored");
    // the restoring cluster's setting stands, not the backup's recorded 7777
    let size = after
        .get(MigrationRecordKind::Config, "partition_size.files")
        .unwrap();
    assert_eq!(
        size.value,
        Some(harness.conf.thorium.files.partition_size.to_string())
    );
    // the target's Job result is still there
    assert!(
        after
            .get(MigrationRecordKind::JobResult, "uid-tgt")
            .is_some()
    );
}

/// Restoring a backup taken at an older revision into a deployment at a newer one sets the revision
/// row to the older revision and replaces the history with the backup's
#[tokio::test]
async fn restoring_an_older_revision_replaces_the_revision_and_history() {
    // a backup at v01, restored into a deployment at v02 with three steps recorded
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v01", "old", 1, &["first"]);
    let (backup, _) = backup_of(harness, "older", &source).await;
    let target = record::deployment("2026-10-v02", "new", 2, &["a", "b", "c"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    // the revision is the backup's, and so are the fingerprint and the layouts
    let revision = after
        .get(MigrationRecordKind::Revision, "deployment")
        .unwrap();
    assert_eq!(revision.revision.as_deref(), Some("2026-10-v01"));
    let deployment = after
        .get(MigrationRecordKind::Backend, "deployment")
        .unwrap();
    assert_eq!(deployment.fingerprint.as_deref(), Some("fp-deployment-old"));
    let redis = after.get(MigrationRecordKind::Backend, "redis").unwrap();
    assert_eq!(redis.layouts.get("users"), Some(&1));
    // the history holds the backup's entry and the Restored entry, none of the target's
    let steps: Vec<String> = history(&after)
        .into_iter()
        .map(|entry| entry.step)
        .collect();
    assert_eq!(steps, ["first", "restored"], "{steps:?}");
}

/// The history a restore replaces is kept as `archived_history` rows keyed `<restore time>/<id>`,
/// and the restored history ends with a `Restored` entry naming the backup, its revision, the
/// replaced revision, and thoradm's version and OS user; a backup of the restored deployment
/// carries the archived rows
#[tokio::test]
async fn a_restore_archives_the_replaced_history_and_appends_a_restored_entry() {
    // a backup at v01, restored into a deployment at v02
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v01", "old", 1, &["first"]);
    let (backup, _) = backup_of(harness, "archive", &source).await;
    let target = record::deployment("2026-10-v02", "new", 2, &["a", "b", "c"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    // each replaced entry is archived under the restore's time and its original id
    let originals = target.kind(MigrationRecordKind::History);
    let archived: Vec<_> = after
        .kind(MigrationRecordKind::ArchivedHistory)
        .into_iter()
        .filter(|row| {
            originals
                .iter()
                .any(|original| row.id.ends_with(&format!("/{}", original.id)))
        })
        .collect();
    assert_eq!(archived.len(), originals.len(), "{:?}", after.ids());
    for original in &originals {
        let row = archived
            .iter()
            .find(|row| row.id.ends_with(&format!("/{}", original.id)))
            .unwrap_or_else(|| panic!("{} wasn't archived", original.id));
        assert_eq!(row.entry, original.entry);
        // the prefix is one restore time, an RFC 3339 timestamp
        let prefix = row.id.strip_suffix(&format!("/{}", original.id)).unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(prefix).is_ok(),
            "{prefix}"
        );
    }
    // the archived history the backup already held is still there
    assert!(
        after
            .get(MigrationRecordKind::ArchivedHistory, "old-restore/old")
            .is_some()
    );
    // the Restored entry names the backup, both revisions, and who restored
    let entries = history(&after);
    let restored_entry = entries.last().unwrap();
    assert_eq!(restored_entry.step, "restored");
    assert_eq!(restored_entry.revision, "2026-10-v01");
    assert_eq!(
        restored_entry.backup.as_deref(),
        Some(backup.to_str().unwrap())
    );
    let message = restored_entry.message.clone().unwrap();
    assert!(
        message.contains("2026-10-v01") && message.contains("2026-10-v02"),
        "{message}"
    );
    assert!(message.contains(backup.to_str().unwrap()), "{message}");
    let by = restored_entry.by.clone().unwrap();
    assert!(
        by.contains(env!("CARGO_PKG_VERSION")) && by.contains("thoradm"),
        "{by}"
    );
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    assert!(
        user.is_empty() || by.contains(&user),
        "{by} doesn't name {user}"
    );
    // a backup of the restored deployment carries the archived rows and the Restored entry
    let again = harness.scratch("archive-again").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "no-s3", "-m", "2", "-o"])
        .arg(&again)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    let carried = record::load(&again).await;
    let carried_archived = carried
        .migrations
        .iter()
        .filter(|row| row.kind == MigrationRecordKind::ArchivedHistory)
        .count();
    // the replaced entries, the one the backup already held, and the one the target already held
    assert_eq!(carried_archived, archived.len() + 2);
    assert!(
        carried
            .migrations
            .iter()
            .any(|row| row.kind == MigrationRecordKind::History
                && row.id.ends_with("/restored/done"))
    );
}

/// A code consumed after the backup was taken stays consumed, and the backup's codes come back:
/// consumed-code rows are merged, never replaced
#[tokio::test]
async fn consumed_codes_are_merged() {
    // the backup holds one consumed code and the target another
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v01", "src", 1, &["first"]);
    let (backup, _) = backup_of(harness, "codes", &source).await;
    let target = record::deployment("2026-10-v02", "tgt", 2, &["a"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    // both rows are there, each with its entry
    let own = after
        .get(MigrationRecordKind::ConsumedCode, "CODE-tgt")
        .expect("the target's code");
    let theirs = after
        .get(MigrationRecordKind::ConsumedCode, "CODE-src")
        .expect("the backup's code");
    assert_eq!(
        own.entry,
        target
            .get(MigrationRecordKind::ConsumedCode, "CODE-tgt")
            .unwrap()
            .entry
    );
    assert_eq!(
        theirs.entry,
        source
            .get(MigrationRecordKind::ConsumedCode, "CODE-src")
            .unwrap()
            .entry
    );
}

/// Builds the upgrade state a deployment's rows describe, as the planner reads it
///
/// # Arguments
///
/// * `rows` - The state rows
fn state_of(rows: &Rows) -> UpgradeState {
    // the revision row, the history entries, and the fingerprints of the backend rows
    let revision = rows
        .get(MigrationRecordKind::Revision, "deployment")
        .and_then(|row| row.revision.clone())
        .expect("a revision row");
    let mut state = UpgradeState::new(revision.parse::<RevisionId>().unwrap(), "test");
    state.applied = history(rows);
    state
}

/// Restoring a backup whose record holds older layouts writes them into `schema_migrations`, and
/// the next plan includes the migrations those layouts need
#[tokio::test]
async fn restored_older_layouts_make_the_next_plan_include_their_migrations() {
    // a backup at the first revision, restored into a deployment at the latest
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let latest = latest();
    let source = record::deployment("2026-10-v01", "old", 1, &[]);
    let (backup, _) = backup_of(harness, "plan", &source).await;
    let target = record::deployment(latest.as_str(), "new", 2, &[]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    // the deployment at the latest revision has nothing left to plan
    let before = Catalog::PRODUCTION
        .plan(&state_of(&target), &latest)
        .unwrap();
    assert!(before.is_empty(), "{before:?}");
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    // the older layout is in the table, and the plan from it includes the reindex migration
    let redis = after.get(MigrationRecordKind::Backend, "redis").unwrap();
    assert_eq!(redis.layouts.get("users"), Some(&1));
    let plan = Catalog::PRODUCTION
        .plan(&state_of(&after), &latest)
        .unwrap();
    let ids: Vec<&str> = plan.iter().map(|step| step.step.as_str()).collect();
    assert!(ids.contains(&"elastic-reindex-keyword-mappings"), "{ids:?}");
}

/// Restoring into a cluster with a different files password is refused before anything is
/// written, naming the setting and printing neither password; the same password restores
#[tokio::test]
async fn a_different_files_password_is_refused_before_any_write() {
    // a backup whose record holds another password's identity
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let other = "another-files-password";
    let mut source = record::deployment("2026-10-v02", "src", 2, &STEPS);
    let mut identity = record::blank(MigrationRecordKind::Config, "files.password");
    identity.value = Some(hash_files_password(other).unwrap());
    source.migrations.push(identity.clone());
    let (backup, _) = backup_of(harness, "password", &source).await;
    // the target is empty and holds its own record
    let target = record::deployment("2026-10-v02", "tgt", 2, &["a"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    let before = (target_state(harness).await, record::read(harness).await);
    let report = backup.parent().unwrap().join("refused");
    let refused = restore(harness, &backup, &report);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    // the preflight says why, without either password or the hash
    let records = read_records(&report.join(FAILURES_FILE)).unwrap();
    let refusal = records
        .iter()
        .find(|record| record.operation == Operation::Preflight)
        .unwrap_or_else(|| panic!("no preflight record: {records:?}"));
    assert_eq!(refusal.object.as_deref(), Some("schema"), "{refusal:?}");
    assert!(refusal.error.contains("files.password"), "{refusal:?}");
    let identity_text = identity.value.unwrap();
    let configured = &harness.conf.thorium.files.password;
    for secret in [other, identity_text.as_str(), configured.as_str()] {
        assert!(
            !refusal.error.contains(secret),
            "the refusal prints a secret: {refusal:?}"
        );
    }
    // nothing changed
    assert_eq!(target_state(harness).await, before.0);
    assert_eq!(record::read(harness).await, before.1);
    // the cluster's own password verifies, and the restore goes ahead
    let mut matching = record::deployment("2026-10-v02", "src", 2, &STEPS);
    let mut own = record::blank(MigrationRecordKind::Config, "files.password");
    own.value = Some(hash_files_password(configured).unwrap());
    matching.migrations.push(own);
    let (backup, _) = backup_of(harness, "password-ok", &matching).await;
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
}

/// A backup without a record restores with no record written: the target's revision and history
/// are left alone and no Restored entry appears
#[tokio::test]
async fn a_backup_without_a_record_restores_with_none() {
    // a backup of a keyspace that holds no state tables
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::empty_target(harness).await;
    record::drop_tables(harness).await;
    let out = harness.scratch("none").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "no-s3", "-m", "2", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    assert!(!out.join("schema.json").exists());
    // restore into the same keyspace without its state tables, which the restore's apply creates
    record::empty_target(harness).await;
    record::drop_tables(harness).await;
    restored(harness, &out, "report");
    let after = record::read(harness).await;
    // no revision, no history, and no Restored entry
    assert!(
        after.kind(MigrationRecordKind::Revision).is_empty(),
        "{:?}",
        after.ids()
    );
    assert!(
        after.kind(MigrationRecordKind::History).is_empty(),
        "{:?}",
        after.ids()
    );
}

/// Restore recreates every view it dropped through `schema apply`, saves no statements, and the
/// API serves a sample, which it reads through a view, without a restart
#[tokio::test]
async fn views_are_recreated_by_the_restores_apply() {
    // a sample, and the namespace's views before the restore
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v02", "src", 2, &STEPS);
    let (backup, sha) = backup_of(harness, "views", &source).await;
    let before = views(harness).await;
    record::empty_target(harness).await;
    let report = restored(harness, &backup, "report");
    // the same views exist, and no view statements were saved
    assert_eq!(views(harness).await, before);
    assert!(
        !report.join("views.cql").exists(),
        "a views.cql was written"
    );
    // the API serves the sample without a restart
    let sample = harness.client().await.files.get(&sha).await;
    assert!(sample.is_ok(), "{sample:?}");
}

/// A view an interrupted restore left dropped comes back from the next restore's apply, with no
/// earlier statements to find
#[tokio::test]
async fn a_view_an_interrupted_restore_dropped_comes_back() {
    // a backup, and a target missing the samples view
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v02", "src", 2, &STEPS);
    let (backup, _) = backup_of(harness, "interrupted", &source).await;
    record::empty_target(harness).await;
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!(
                "DROP MATERIALIZED VIEW {}.samples",
                harness.conf.thorium.namespace
            ),
            (),
        )
        .await
        .unwrap();
    assert!(!views(harness).await.contains(&"samples".to_owned()));
    // the restore creates it again
    restored(harness, &backup, "report");
    assert!(views(harness).await.contains(&"samples".to_owned()));
}

/// The restore's summary reports the record it restored: the revision, the revision it replaced,
/// and how many rows and history entries it wrote and archived
#[tokio::test]
async fn the_summary_reports_the_restored_record() {
    // a backup at v01 restored into a deployment at v02
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v01", "old", 1, &["first"]);
    let (backup, _) = backup_of(harness, "summary", &source).await;
    let target = record::deployment("2026-10-v02", "new", 2, &["a", "b"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    let report = restored(harness, &backup, "report");
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    let schema = &summary["schema_record"];
    assert_eq!(schema["revision"], "2026-10-v01", "{summary}");
    assert_eq!(schema["replaced_revision"], "2026-10-v02", "{summary}");
    assert_eq!(schema["archived"], 2, "{summary}");
    assert!(schema["rows"].as_u64().unwrap() > 5, "{summary}");
}

/// The rows of kinds this thoradm doesn't know come back with the record, merged with the ones the
/// target holds, so an audit row of a later release survives a restore
#[tokio::test]
async fn rows_of_unknown_kinds_are_restored_and_merged() {
    // a backup holding an audit row and a future row; a target holding another audit row
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let source = record::deployment("2026-10-v02", "src", 2, &STEPS);
    record::empty_target(harness).await;
    record::set(harness, &source).await;
    record::insert_raw(harness, "legacy_drop", "from-backup", "kept").await;
    record::insert_raw(harness, "zzz_future", "from-backup", "kept").await;
    let backup = record::backup_now(harness, "unknown-restore");
    record::empty_target(harness).await;
    record::set(
        harness,
        &record::deployment("2026-10-v02", "tgt", 2, &["a"]),
    )
    .await;
    record::insert_raw(harness, "legacy_drop", "from-target", "also kept").await;
    restored(harness, &backup, "report");
    // both audit rows are there, and the future row came back
    let drops = record::raw_values(harness, "legacy_drop").await;
    assert_eq!(
        drops.get("from-backup"),
        Some(&Some("kept".to_owned())),
        "{drops:?}"
    );
    assert_eq!(
        drops.get("from-target"),
        Some(&Some("also kept".to_owned())),
        "{drops:?}"
    );
    let future = record::raw_values(harness, "zzz_future").await;
    assert_eq!(
        future.get("from-backup"),
        Some(&Some("kept".to_owned())),
        "{future:?}"
    );
}

/// Backs up some components of the namespace as it stands
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
/// * `components` - The components to back up
fn backup_of_components(harness: &Harness, name: &str, components: &str) -> std::path::PathBuf {
    // a small backup of just those components
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            components,
            "-m",
            "2",
            "-o",
        ])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    out
}

/// The warnings a restore's summary lists
///
/// # Arguments
///
/// * `report` - The restore's report directory
fn warnings(report: &Path) -> String {
    // the summary's warnings, joined
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    summary["warnings"].to_string()
}

/// A backup of some tables restored onto a deployment at a newer revision leaves the target's
/// record alone, since the backup says nothing about the rest: its revision, history, and
/// fingerprints stay, nothing is archived, no Restored entry appears, and the summary says why
#[tokio::test]
async fn a_partial_backup_leaves_the_targets_record_alone() {
    // a backup of the entities table at v01, restored onto a deployment at v02
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::empty_target(harness).await;
    record::set(
        harness,
        &record::deployment("2026-10-v01", "old", 1, &["first"]),
    )
    .await;
    let backup = backup_of_components(harness, "partial", "entities");
    let target = record::deployment("2026-10-v02", "new", 2, &["a", "b", "c"]);
    record::set(harness, &target).await;
    let report = restored(harness, &backup, "report");
    let after = record::read(harness).await;
    // the revision, every history row, and every fingerprint are the target's
    assert_eq!(
        after.get(MigrationRecordKind::Revision, "deployment"),
        target.get(MigrationRecordKind::Revision, "deployment")
    );
    assert_eq!(
        after.kind(MigrationRecordKind::History),
        target.kind(MigrationRecordKind::History)
    );
    assert_eq!(
        after.kind(MigrationRecordKind::Backend),
        target.kind(MigrationRecordKind::Backend)
    );
    assert_eq!(
        after.kind(MigrationRecordKind::ArchivedHistory),
        target.kind(MigrationRecordKind::ArchivedHistory)
    );
    // the summary says the record wasn't restored, and why
    let warnings = warnings(&report);
    assert!(
        warnings.contains("schema record") && warnings.contains("not restored"),
        "{warnings}"
    );
}

/// A backup whose record names no revision doesn't delete the target's revision or history
#[tokio::test]
async fn a_record_without_a_revision_leaves_the_targets_record_alone() {
    // a whole backup of rows that name no revision, restored onto a deployment with a history
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let mut source = record::deployment("2026-10-v01", "nr", 1, &["first"]);
    source
        .migrations
        .retain(|row| row.kind != MigrationRecordKind::Revision);
    let (backup, _) = backup_of(harness, "no-revision", &source).await;
    let target = record::deployment("2026-10-v02", "new", 2, &["a", "b"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    let report = restored(harness, &backup, "report");
    let after = record::read(harness).await;
    assert_eq!(
        after.get(MigrationRecordKind::Revision, "deployment"),
        target.get(MigrationRecordKind::Revision, "deployment")
    );
    assert_eq!(
        after.kind(MigrationRecordKind::History),
        target.kind(MigrationRecordKind::History)
    );
    assert!(
        warnings(&report).contains("revision"),
        "{}",
        warnings(&report)
    );
}

/// A backup of tables that hold no files restores into a cluster with another files password,
/// since no stored file depends on it
#[tokio::test]
async fn another_files_password_doesnt_refuse_a_backup_without_files() {
    // a record holding another password's identity, backed up with the tags table alone
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::empty_target(harness).await;
    let mut rows = record::deployment("2026-10-v02", "pw", 2, &STEPS);
    let mut identity = record::blank(MigrationRecordKind::Config, "files.password");
    identity.value = Some(hash_files_password("some-other-password").unwrap());
    rows.migrations.push(identity);
    record::set(harness, &rows).await;
    let backup = backup_of_components(harness, "no-files", "tags");
    record::empty_target(harness).await;
    restored(harness, &backup, "report");
}

/// A restore that fails while it writes the record leaves the target's old record whole
#[tokio::test]
async fn a_failed_record_write_leaves_the_old_record_intact() {
    // a target with a record, and a backup record holding a row no write can take: its kind is
    // empty, which is no partition key
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::empty_target(harness).await;
    let target = record::deployment("2026-10-v02", "old", 2, &["a", "b"]);
    record::set(harness, &target).await;
    let source = record::deployment("2026-10-v01", "new", 1, &["first"]);
    let mut backup = thoradm::backup::schema::BackupSchemaRecord {
        format: thoradm::backup::schema::RECORD_FORMAT,
        thoradm_version: None,
        migrations: source.migrations.clone(),
        issues: source.issues.clone(),
        other: vec![thoradm::backup::schema::RawRow {
            kind: String::new(),
            id: "x".to_owned(),
            fingerprint: None,
            apply_key: None,
            layouts: None,
            revision: None,
            version: None,
            entry: None,
            result: None,
            value: Some("v".to_owned()),
            status: None,
            attempt: None,
            lease: None,
            started: None,
            finished: None,
            job: None,
            backup: None,
            counts: None,
            report: None,
            updated: None,
        }],
    };
    backup
        .migrations
        .retain(|row| row.kind != MigrationRecordKind::JobResult);
    let before = record::read(harness).await;
    let origin = thoradm::backup::schema_restore::Origin {
        reference: "/backups/failing",
        by: "test",
        at: chrono::Utc::now(),
        stamp: "2026-10-19T08:00:00.000000000Z",
        baseline: false,
    };
    let scylla = harness.scylla().await;
    let failed = thoradm::backup::schema_restore::restore_record(
        &scylla,
        &harness.conf.thorium.namespace,
        &backup,
        &origin,
    )
    .await;
    assert!(failed.is_err(), "the write of an empty kind succeeded");
    // nothing of the old record changed
    assert_eq!(record::read(harness).await, before);
}

/// A restore run again over an interrupted one writes the same archive keys, so no replaced
/// history entry is archived twice
#[tokio::test]
async fn a_second_run_archives_under_the_same_keys() {
    // a target with a history, restored from a record twice with the same stamp
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::empty_target(harness).await;
    let target = record::deployment("2026-10-v02", "old", 2, &["a", "b", "c"]);
    let source = record::deployment("2026-10-v01", "new", 1, &["first"]);
    let backup = thoradm::backup::schema::BackupSchemaRecord {
        format: thoradm::backup::schema::RECORD_FORMAT,
        thoradm_version: None,
        migrations: source.migrations.clone(),
        issues: source.issues.clone(),
        other: Vec::new(),
    };
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for (run, at) in [
        chrono::Utc::now(),
        chrono::Utc::now() + chrono::Duration::seconds(5),
    ]
    .into_iter()
    .enumerate()
    {
        // the first run starts from the target; the interrupted run left its own history in place
        // for the second, with the archive rows the first wrote still beside it
        if run == 0 {
            record::set(harness, &target).await;
        } else {
            let history = record::Rows {
                migrations: target
                    .kind(MigrationRecordKind::History)
                    .into_iter()
                    .cloned()
                    .collect(),
                issues: Vec::new(),
            };
            record::insert(&scylla, ns, &history).await;
        }
        let origin = thoradm::backup::schema_restore::Origin {
            reference: "/backups/twice",
            by: "test",
            at,
            stamp: "2026-10-19T08:00:00.000000000Z",
            baseline: false,
        };
        thoradm::backup::schema_restore::restore_record(&scylla, ns, &backup, &origin)
            .await
            .unwrap();
    }
    // each original entry is archived once, however often the restore ran
    let after = record::read(harness).await;
    let archived = after.kind(MigrationRecordKind::ArchivedHistory);
    for original in target.kind(MigrationRecordKind::History) {
        let copies = archived
            .iter()
            .filter(|row| row.id.ends_with(&format!("/{}", original.id)))
            .count();
        assert_eq!(copies, 1, "{} was archived {copies} times", original.id);
    }
}

/// The revision a backup without a record is at: the first Helm revision
const BASELINE: &str = "2026-10-v02";

/// Backs up some components of a keyspace that holds no state tables, as a deployment from before
/// revisions were recorded does, so the backup holds no `schema.json`
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
/// * `components` - The components to back up
async fn recordless_backup(harness: &Harness, name: &str, components: &str) -> std::path::PathBuf {
    // a target holding one sample and no state tables
    record::empty_target(harness).await;
    let client = harness.client().await;
    let names = common::names::Names::new();
    let group = names.group("athena");
    common::seed::group(&client, &group).await;
    common::seed::sample(&client, &group, "old.bin", names.suffix.as_bytes()).await;
    record::drop_tables(harness).await;
    let out = backup_of_components(harness, name, components);
    assert!(!out.join("schema.json").exists());
    out
}

/// The `<revision> <step>` of every step the planner plans from a record up to a revision
///
/// # Arguments
///
/// * `rows` - The record's rows
/// * `to` - The revision to plan up to
fn planned(rows: &Rows, to: &RevisionId) -> Vec<String> {
    // plan from the state the record describes, as the operator does
    Catalog::PRODUCTION
        .plan(&state_of(rows), to)
        .unwrap()
        .iter()
        .map(|step| format!("{} {}", step.revision, step.step))
        .collect()
}

/// The `<revision> <step>` of every step of every edge from one revision up to another, sorted,
/// since the planner may interleave the steps of later edges
///
/// # Arguments
///
/// * `from` - The revision to start from
/// * `to` - The revision to reach
fn every_step(from: &str, to: &RevisionId) -> Vec<String> {
    // a deployment that just reached `from` has every later step left
    let from = from.parse::<RevisionId>().unwrap();
    let state = Catalog::PRODUCTION.reached(&from, "test");
    Catalog::PRODUCTION
        .path(&from, to)
        .unwrap()
        .into_iter()
        .flat_map(|edge| {
            Catalog::PRODUCTION
                .edge_steps(&state, edge)
                .into_iter()
                .map(move |step| format!("{} {}", edge.id, step.id))
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A whole backup without a record, restored into a deployment that records a newer revision,
/// records the first Helm revision its data is at: the target's history is archived and replaced
/// by a Restored entry saying so, the target's fingerprints stay, the operator's next plan holds
/// every step after the baseline (the users conversion included), `migrate plan` from the
/// recorded revision lists them, and the summary says what was recorded and what runs next
#[tokio::test]
async fn a_backup_without_a_record_records_the_baseline_under_a_newer_revision() {
    // a backup without a record, restored into a fresh install at the latest revision
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let backup = recordless_backup(harness, "baseline", "no-s3").await;
    let latest = latest();
    let target = record::deployment(latest.as_str(), "fresh", 2, &["schema-apply", "finalize"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    assert_eq!(planned(&target, &latest), Vec::<String>::new());
    // the plan logged in place of the prompt names the revision the restore records
    let report = backup.parent().unwrap().join("report");
    let output = restore(harness, &backup, &report);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let logged = String::from_utf8_lossy(&output.stderr);
    assert!(
        logged.contains(&format!(
            "records revision {BASELINE} in place of the target's"
        )),
        "{}",
        describe(&output)
    );
    let after = record::read(harness).await;
    // the baseline is recorded, under a history of one Restored entry, with the target's
    // history archived and its fingerprints kept
    let revision = after
        .get(MigrationRecordKind::Revision, "deployment")
        .unwrap();
    assert_eq!(revision.revision.as_deref(), Some(BASELINE));
    let entries = history(&after);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(
        (entries[0].step.as_str(), entries[0].revision.as_str()),
        ("restored", BASELINE)
    );
    assert!(
        entries[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("no schema record")),
        "{entries:?}"
    );
    assert_eq!(after.kind(MigrationRecordKind::ArchivedHistory).len(), 3);
    assert_eq!(
        after.kind(MigrationRecordKind::Backend),
        target.kind(MigrationRecordKind::Backend)
    );
    // the next plan runs every step after the baseline, the users conversion included
    let mut plan = planned(&after, &latest);
    plan.sort();
    assert_eq!(plan, every_step(BASELINE, &latest));
    assert!(
        plan.contains(&"2026-10-v03 users-settings-theme".to_owned()),
        "{plan:?}"
    );
    // and so does `migrate plan` from the recorded revision
    let listed = harness
        .thoradm()
        .args(["migrate", "plan", "--from", BASELINE])
        .output()
        .expect("failed to run thoradm");
    assert_eq!(listed.status.code(), Some(0), "{}", describe(&listed));
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("users-settings-theme"), "{listed}");
    // the summary says what was recorded and what the operator runs next
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    let schema = &summary["schema_record"];
    assert_eq!(schema["revision"], BASELINE, "{summary}");
    assert_eq!(schema["replaced_revision"], latest.as_str(), "{summary}");
    assert_eq!(schema["baseline"], true, "{summary}");
    let mut listed_in_summary: Vec<String> =
        serde_json::from_value(schema["planned"].clone()).unwrap();
    listed_in_summary.sort();
    assert_eq!(
        listed_in_summary,
        every_step(BASELINE, &latest),
        "{summary}"
    );
    assert!(
        summary["not_done"].to_string().contains(BASELINE),
        "{summary}"
    );
}

/// A backup without a record restored into a deployment at the third revision, which is where a
/// backup of a deployment from before revisions lands on the first release that records them,
/// gets that revision's users conversion planned; a user still in the flat layout is converted
/// when the step's migration runs as the operator's Job runs it, and the plan then leaves it out
#[tokio::test]
async fn a_restored_user_in_the_flat_layout_converts_when_the_planned_step_runs() {
    // a backup without a record, restored into a fresh install at the third revision
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let backup = recordless_backup(harness, "flat-user", "no-s3").await;
    let third = "2026-10-v03".parse::<RevisionId>().unwrap();
    let target = record::deployment(third.as_str(), "third", 2, &["schema-apply"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    assert_eq!(
        planned(&after, &third),
        [
            "2026-10-v03 schema-apply",
            "2026-10-v03 users-settings-theme"
        ]
    );
    // a user whose theme is in its own field, as the first layout keeps it
    let ns = &harness.conf.thorium.namespace;
    let name = format!("flat-{}", common::names::Names::new().suffix);
    let key = format!("{ns}:user_data:{name}");
    let pool = common::harness::redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let () = redis::pipe()
        .cmd("HSET")
        .arg(&key)
        .arg("username")
        .arg(&name)
        .arg("theme")
        .arg("\"Crab\"")
        .ignore()
        .cmd("SADD")
        .arg(format!("{ns}:users"))
        .arg(&name)
        .ignore()
        .query_async(&mut *conn)
        .await
        .unwrap();
    // the step's migration converts it, as the operator's Job runs it once the step is approved
    let ran = harness
        .thoradm()
        .args(["migrate", "run", "--id", "users-settings-theme"])
        .output()
        .expect("failed to run thoradm");
    let fields: std::collections::BTreeMap<String, String> = redis::cmd("HGETALL")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    // the user is removed before asserting, so no other test reads a user the API can't load
    let () = redis::pipe()
        .cmd("DEL")
        .arg(&key)
        .ignore()
        .cmd("SREM")
        .arg(format!("{ns}:users"))
        .arg(&name)
        .ignore()
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ran.status.code(), Some(0), "{}", describe(&ran));
    assert!(!fields.contains_key("theme"), "{fields:?}");
    let settings: serde_json::Value = serde_json::from_str(&fields["settings"]).unwrap();
    assert_eq!(settings["theme"], "Crab", "{fields:?}");
    // the run recorded the step, so the plan leaves it out
    let after = record::read(harness).await;
    assert_eq!(planned(&after, &third), ["2026-10-v03 schema-apply"]);
}

/// A backup without a record leaves the target's record alone when the target records no
/// revision newer than the baseline, and when the backup lacks some of the deployment's state,
/// whose summary then says why the record stayed
#[tokio::test]
async fn a_backup_without_a_record_leaves_an_older_target_or_a_partial_restore_alone() {
    // a whole backup without a record, restored into a deployment at the baseline
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let backup = recordless_backup(harness, "at-baseline", "no-s3").await;
    let target = record::deployment(BASELINE, "base", 2, &["scylla-role", "finalize"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    restored(harness, &backup, "report");
    let after = record::read(harness).await;
    assert_eq!(
        after.get(MigrationRecordKind::Revision, "deployment"),
        target.get(MigrationRecordKind::Revision, "deployment")
    );
    assert_eq!(
        after.kind(MigrationRecordKind::History),
        target.kind(MigrationRecordKind::History)
    );
    // a backup of one table without a record, restored into a deployment at the latest revision
    let partial = recordless_backup(harness, "partial-recordless", "entities").await;
    let latest = latest();
    let newer = record::deployment(latest.as_str(), "newer", 2, &["finalize"]);
    record::empty_target(harness).await;
    record::set(harness, &newer).await;
    let report = restored(harness, &partial, "report");
    let after = record::read(harness).await;
    assert_eq!(
        after.get(MigrationRecordKind::Revision, "deployment"),
        newer.get(MigrationRecordKind::Revision, "deployment")
    );
    assert_eq!(
        after.kind(MigrationRecordKind::History),
        newer.kind(MigrationRecordKind::History)
    );
    let warnings = warnings(&report);
    assert!(
        warnings.contains("no schema record") && warnings.contains(BASELINE),
        "{warnings}"
    );
}
