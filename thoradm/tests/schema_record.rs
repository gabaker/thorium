//! A backup carries the deployment's schema record: every row of both migration-state tables but
//! the Job results, as `schema.json` beside the archives, listed in the manifest
//!
//! The tests that change the state tables hold the binary's restore guard, since a test that backs
//! up under the shared guard reads them.

mod common;

use common::cycle::{describe, verify};
use common::harness::Harness;
use common::record::{self, Rows};
use data_encoding::HEXLOWER;
use sha2::{Digest, Sha256};
use std::path::Path;
use thoradm::backup::manifest::Manifest;
use thoradm::backup::schema::{BackupSchemaRecord, RECORD_FORMAT, SCHEMA_FILE};
use thorium::models::schema::MigrationRecordKind;

/// The components these tests back up: every table component and Redis, without objects
const COMPONENTS: &str = "no-s3";

/// Backs up the namespace's tables and Redis into a directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
fn backup(harness: &Harness, name: &str) -> std::path::PathBuf {
    // a small backup of every table
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            COMPONENTS,
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

/// The rows of a backup's record, as table rows sorted by key
///
/// # Arguments
///
/// * `backup` - The backup root
async fn recorded(backup: &Path) -> Rows {
    // the record's rows, sorted the way a read of the tables is
    let record = record::load(backup).await;
    let mut rows = Rows {
        migrations: record.migrations,
        issues: record.issues,
    };
    rows.sort();
    rows
}

/// A backup records the deployment's schema fingerprint and layout versions from
/// `schema_migrations`, and `schema.json` holds no `job_result` row
#[tokio::test]
async fn a_backup_records_the_fingerprint_and_layouts_and_no_job_results() {
    // a deployment's rows in the namespace, a Job result among them
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = record::deployment("2026-10-v02", "src", 2, &["scylla-role", "finalize"]);
    record::set(harness, &seeded).await;
    let out = backup(harness, "fingerprints");
    // the record holds the deployment fingerprint, the revision, and the Redis layouts as seeded
    let loaded = record::load(&out).await;
    assert_eq!(loaded.format, RECORD_FORMAT);
    assert_eq!(
        loaded.thoradm_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(loaded.deployment_fingerprint(), Some("fp-deployment-src"));
    assert_eq!(loaded.revision(), Some("2026-10-v02"));
    let redis = loaded
        .row(MigrationRecordKind::Backend, "redis")
        .expect("the redis backend row");
    assert_eq!(redis.fingerprint.as_deref(), Some("fp-redis-src"));
    assert_eq!(redis.layouts.get("users"), Some(&2));
    assert_eq!(redis.layouts.get("groups"), Some(&1));
    // no Job result is in the file, whatever its text
    let text = std::fs::read_to_string(out.join(SCHEMA_FILE)).unwrap();
    assert!(
        !text.contains("job_result") && !text.contains("uid-src"),
        "{text}"
    );
    assert!(
        loaded
            .migrations
            .iter()
            .all(|row| row.kind != MigrationRecordKind::JobResult)
    );
}

/// A backup writes every `schema_migrations` and `schema_migration_issues` row (revision, history,
/// run rows, layouts, the data-shaping record, consumed codes, archived history, worker and issue
/// rows) into `schema.json`, each cell as the table holds it
#[tokio::test]
async fn a_backup_writes_every_state_row() {
    // every kind of row, with every cell a test checks set
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = record::deployment(
        "2026-10-v02",
        "all",
        2,
        &["scylla-role", "finalize", "last"],
    );
    record::set(harness, &seeded).await;
    let out = backup(harness, "every-row");
    // the record holds the table's rows except the Job result, unchanged
    let mut expected = seeded.clone();
    expected
        .migrations
        .retain(|row| row.kind != MigrationRecordKind::JobResult);
    expected.sort();
    let found = recorded(&out).await;
    assert_eq!(found.ids(), expected.ids());
    assert_eq!(found, expected);
    // the history is the three entries the deployment applied, oldest first by id
    let history = found.kind(MigrationRecordKind::History);
    assert_eq!(history.len(), 3);
    assert!(history.windows(2).all(|pair| pair[0].id < pair[1].id));
    // the issue table's worker and issue rows are both there
    assert_eq!(found.issues.len(), 2, "{:?}", found.issues);
}

/// The manifest lists the record as a sibling with its length and sha256, and verify passes
#[tokio::test]
async fn the_manifest_lists_the_record_and_verify_passes() {
    // a backup of a deployment with a record
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::set(
        harness,
        &record::deployment("2026-10-v02", "list", 2, &["finalize"]),
    )
    .await;
    let out = backup(harness, "listed");
    // the sibling entry matches the file
    let bytes = std::fs::read(out.join(SCHEMA_FILE)).unwrap();
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    let sibling = manifest
        .siblings
        .iter()
        .find(|sibling| sibling.name == SCHEMA_FILE)
        .expect("the manifest doesn't list schema.json");
    assert_eq!(sibling.len, bytes.len() as u64);
    assert_eq!(sibling.sha256, HEXLOWER.encode(&Sha256::digest(&bytes)));
    // the file is private to the user, as it holds the files password's hash
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(out.join(SCHEMA_FILE))
            .unwrap()
            .permissions(),
    );
    assert_eq!(
        mode & 0o077,
        0,
        "schema.json is readable by others: {mode:o}"
    );
    // verify checks it and finds nothing wrong
    let checked = verify(harness, &out);
    assert_eq!(checked.status.code(), Some(0), "{}", describe(&checked));
}

/// A deployment whose keyspace holds no record table backs up without a `schema.json`, and the
/// manifest lists no sibling
#[tokio::test]
async fn a_keyspace_without_a_record_table_backs_up_without_a_record() {
    // drop both state tables, as a keyspace that never recorded its schema lacks them
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for table in ["schema_migrations", "schema_migration_issues"] {
        scylla
            .query_unpaged(format!("DROP TABLE IF EXISTS {ns}.{table}"), ())
            .await
            .unwrap();
    }
    let out = backup(harness, "no-record");
    // no record is written, and the manifest lists no sibling
    assert!(!out.join(SCHEMA_FILE).exists(), "a schema.json was written");
    assert_eq!(BackupSchemaRecord::load(&out).await.unwrap(), None);
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    assert!(manifest.siblings.is_empty(), "{:?}", manifest.siblings);
    // put the tables back for the other tests of this binary
    thorium::models::schema::apply(
        &scylla,
        &harness.conf,
        &thorium::models::schema::ApplyOptions {
            target: thorium::models::schema::ApplyTarget::Declared,
            dry_run: false,
        },
        "",
        &|_| (),
    )
    .await
    .expect("failed to recreate the state tables");
}

/// An empty record table backs up as a record with no rows, which restores as an empty record
#[tokio::test]
async fn empty_state_tables_back_up_as_an_empty_record() {
    // both tables exist and hold nothing
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::clear(harness).await;
    let out = backup(harness, "empty-record");
    let loaded = record::load(&out).await;
    assert!(
        loaded.migrations.is_empty() && loaded.issues.is_empty(),
        "{loaded:?}"
    );
    assert_eq!(loaded.revision(), None);
}

/// A backup carries the rows of a kind this thoradm doesn't know, such as a later release's audit
/// row, raw in `schema.json` beside the rows it knows, and a read of the tables doesn't fail on
/// them; revision and history rows written with raw CQL are carried as typed rows
#[tokio::test]
async fn a_backup_carries_rows_of_kinds_it_doesnt_know() {
    // a deployment's rows, plus raw rows of two kinds this build has no variant for
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::set(
        harness,
        &record::deployment("2026-10-v02", "raw", 2, &["finalize"]),
    )
    .await;
    record::insert_raw(
        harness,
        "legacy_drop",
        "2026-10-02T00:00:00Z/scylla:events",
        "dropped",
    )
    .await;
    record::insert_raw(harness, "zzz_future", "one", "later").await;
    record::insert_raw(harness, "revision", "extra", "typed").await;
    let out = backup(harness, "unknown-kinds");
    // the file holds the unknown rows raw, under `other`, with their cells
    let text = std::fs::read_to_string(out.join(SCHEMA_FILE)).unwrap();
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    let other = file["other"]
        .as_array()
        .expect("the record has an `other` list");
    let mut found: Vec<(String, String, String)> = other
        .iter()
        .map(|row| {
            (
                row["kind"].as_str().unwrap().to_owned(),
                row["id"].as_str().unwrap().to_owned(),
                row["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [(
            "zzz_future".to_owned(),
            "one".to_owned(),
            "later".to_owned()
        )]
    );
    // the rows it knows are still there: the raw revision row, and the legacy drop's audit row,
    // whose kind this build knows
    let loaded = record::load(&out).await;
    assert!(
        loaded
            .row(
                MigrationRecordKind::LegacyDrop,
                "2026-10-02T00:00:00Z/scylla:events"
            )
            .is_some()
    );
    assert_eq!(loaded.revision(), Some("2026-10-v02"));
    assert!(loaded.row(MigrationRecordKind::Revision, "extra").is_some());
    assert!(
        loaded
            .row(
                MigrationRecordKind::History,
                &record::history_row(&record::step("2026-10-v02", "finalize", 2)).id
            )
            .is_some()
    );
}

/// An applied-key an earlier writer recorded unredacted never reaches `schema.json`: the files
/// password's hash segment is cut out of it
#[tokio::test]
async fn a_backup_never_copies_the_password_segment_of_an_apply_key() {
    // a deployment row whose key still holds the unsalted hash
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let hash = "ab".repeat(32);
    let mut rows = record::deployment("2026-10-v02", "key", 2, &["finalize"]);
    for row in &mut rows.migrations {
        if row.kind == MigrationRecordKind::Backend && row.id == "deployment" {
            row.apply_key = Some(format!("image=thorium:1;password={hash};size=1"));
        }
    }
    record::set(harness, &rows).await;
    let out = backup(harness, "redacted");
    let text = std::fs::read_to_string(out.join(SCHEMA_FILE)).unwrap();
    assert!(!text.contains(&hash), "the hash is in schema.json");
    let loaded = record::load(&out).await;
    let key = loaded
        .row(MigrationRecordKind::Backend, "deployment")
        .and_then(|row| row.apply_key.clone());
    assert_eq!(key.as_deref(), Some("image=thorium:1;size=1"));
}

/// The record is read before the data is scanned, so it never claims a newer revision than the
/// archives hold: a revision recorded while the backup runs isn't in the file
#[tokio::test]
async fn the_record_is_read_before_the_data() {
    // pause the backup once it starts on a table, then move the recorded revision
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    record::set(
        harness,
        &record::deployment("2026-10-v02", "pause", 2, &["finalize"]),
    )
    .await;
    let out = harness.scratch("paused").join("backup");
    let mut command = harness.thoradm();
    command
        .args(["--workers", "2", "backup", "new", "tags", "-m", "2", "-o"])
        .arg(&out);
    let mut command = tokio::process::Command::from(command);
    command
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let pid = i32::try_from(child.id().unwrap()).unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut lines = tokio::io::BufReader::new(stderr);
    let mut line = String::new();
    let mut moved = false;
    while tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line)
        .await
        .unwrap()
        > 0
    {
        if !moved && line.contains("Backing up tags") {
            // SAFETY: the signal only stops and continues the child process
            unsafe { libc::kill(pid, libc::SIGSTOP) };
            let scylla = harness.scylla().await;
            scylla
                .query_unpaged(
                    format!(
                        "UPDATE {}.schema_migrations SET revision = '2026-10-v99' \
                         WHERE kind = 'revision' AND id = 'deployment'",
                        harness.conf.thorium.namespace
                    ),
                    (),
                )
                .await
                .unwrap();
            // SAFETY: as above
            unsafe { libc::kill(pid, libc::SIGCONT) };
            moved = true;
        }
        line.clear();
    }
    assert_eq!(child.wait().await.unwrap().code(), Some(0));
    assert!(moved, "the backup never logged that it started on tags");
    // the file holds the revision recorded when the backup started
    assert_eq!(record::load(&out).await.revision(), Some("2026-10-v02"));
}
