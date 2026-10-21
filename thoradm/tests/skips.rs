//! Rows thoradm can't decode are skipped and recorded, never silently dropped

mod common;

use common::harness::Harness;
use common::names::Names;
use std::path::Path;
use std::process::Output;
use thoradm::backup::failures::{FAILURES_FILE, FailureKind, Operation, read_records};

/// Runs `thoradm backup new notifications` with one worker and one token range
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `out` - The output directory
/// * `extra` - Extra arguments for `backup new`
fn backup_notifications(harness: &Harness, out: &Path, extra: &[&str]) -> Output {
    // one worker reading one range keeps every row in the same range
    harness
        .thoradm()
        .args([
            "--workers",
            "1",
            "backup",
            "new",
            "notifications",
            "-m",
            "1",
            "-o",
        ])
        .arg(out)
        .args(extra)
        .output()
        .expect("failed to run thoradm")
}

/// An undecodable notification is skipped and recorded with its table, range, and key, every
/// other row is archived, and the run exits 2, or 1 under `--abort-on-failure`
#[tokio::test]
async fn undecodable_rows_are_skipped_and_recorded() {
    // write three valid notifications and one whose kind no thoradm knows
    let harness = Harness::get().await;
    let scylla = harness.scylla().await;
    let names = Names::new();
    let image = names.group("apollo");
    let ns = &harness.conf.thorium.namespace;
    for (kind, msg) in [
        ("Images", "first"),
        ("Images", "second"),
        ("Pipelines", "third"),
        ("Mystery", "undecodable"),
    ] {
        scylla
            .query_unpaged(
                format!(
                    "INSERT INTO {ns}.notifications (kind, key, created, id, msg, level) \
                     VALUES (?, ?, toTimestamp(now()), uuid(), ?, 'Info')"
                ),
                (kind, image.as_str(), msg),
            )
            .await
            .unwrap();
    }
    // back up notifications
    let out = harness.scratch("skips-continue");
    let output = backup_notifications(harness, &out, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // the run finished with a recorded skip
    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("notifications: 3 handled, 1 skipped, 0 failed"),
        "stderr:\n{stderr}"
    );
    // the record names the table, the token range, and the readable key
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let skip = &records[0];
    assert_eq!(skip.kind, FailureKind::Skipped);
    assert_eq!(skip.operation, Operation::DeserializeRow);
    assert_eq!(skip.component, "notifications");
    assert_eq!(skip.table.as_deref(), Some("notifications"));
    assert_eq!(skip.token_range, Some((i64::MIN, i64::MAX)));
    let key = skip.key.as_deref().unwrap_or_default();
    assert!(key.contains("Mystery") && key.contains(&image), "{key}");
    // under --abort-on-failure the same skip is recorded and the run stops with exit 1
    let stopped = harness.scratch("skips-abort");
    let output = backup_notifications(harness, &stopped, &["--abort-on-failure"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    let records = read_records(&stopped.join(FAILURES_FILE)).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].kind, FailureKind::Skipped);
    assert_eq!(records[0].key, skip.key);
}
