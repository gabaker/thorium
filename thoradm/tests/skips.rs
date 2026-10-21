//! Rows thoradm can't decode are skipped and recorded, never silently dropped

mod common;

use common::harness::Harness;
use common::names::Names;
use common::seed;
use std::path::Path;
use std::process::Output;
use thoradm::backup::failures::{FAILURES_FILE, FailureKind, Operation, read_records};
use thoradm::backup::manifest::{MANIFEST_FILE, Manifest};
use thorium::models::{Buffer, CommentRequest, OnDiskFile, SampleRequest};

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

/// A comment row's group, sha256, upload time, id, and attachments column
type CommentRow = (
    String,
    String,
    chrono::DateTime<chrono::Utc>,
    uuid::Uuid,
    Option<String>,
);

/// A comment whose attachments column isn't JSON is skipped and recorded under its own name, and
/// the attachments of the archive's other comments are still backed up
#[tokio::test]
async fn a_malformed_attachments_column_skips_only_its_comment() {
    // a sample with two comments in one group, each with an attachment
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("minerva");
    seed::group(&client, &group).await;
    let data = Buffer::new(format!("a sample for {}", names.suffix).into_bytes()).name("owl.bin");
    let sample = client
        .files
        .create(SampleRequest::new_buffer(data, vec![group.as_str()]))
        .await
        .unwrap()
        .sha256;
    let seeds = harness.scratch("skips-comments-seed");
    for name in ["good.txt", "bad.txt"] {
        std::fs::write(seeds.join(name), name.as_bytes()).unwrap();
        let comment = CommentRequest::new(&sample, format!("comment with {name}"))
            .file(OnDiskFile::new(seeds.join(name)).trim_prefix(&seeds));
        client.files.comment(comment).await.unwrap();
    }
    // make the attachments column of the second comment something that isn't JSON
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let rows = scylla
        .query_unpaged(
            format!("SELECT group, sha256, uploaded, id, files FROM {ns}.comments"),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let rows: Vec<CommentRow> = rows
        .rows::<CommentRow>()
        .unwrap()
        .map(Result::unwrap)
        .filter(|row| row.0 == group)
        .collect();
    let by_file = |file: &str| {
        rows.iter()
            .find(|row| row.4.as_deref().unwrap_or_default().contains(file))
            .unwrap()
    };
    let (good, bad) = (by_file("good.txt"), by_file("bad.txt"));
    scylla
        .query_unpaged(
            format!(
                "UPDATE {ns}.comments SET files = 'not json' \
                 WHERE group = ? AND sha256 = ? AND uploaded = ? AND id = ?"
            ),
            (&bad.0, &bad.1, bad.2, bad.3),
        )
        .await
        .unwrap();
    // back up the comments and their attachments
    let out = harness.scratch("skips-comments-out");
    let output = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            "comments,comment-attachments",
            "-m",
            "2",
            "-o",
        ])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // the run finished with a recorded skip, not a failed archive read
    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let skip = &records[0];
    assert_eq!(skip.kind, FailureKind::Skipped, "{skip:?}");
    assert_eq!(skip.component, "comment-attachments", "{skip:?}");
    assert_eq!(
        skip.referrer,
        Some(format!("comments:{}/{}", bad.1, bad.3)),
        "{skip:?}"
    );
    // the manifest counts the skip, and the comment rows themselves were all handled
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(out.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert_eq!(manifest.skipped, 1);
    assert!(manifest.tables["comments"].rows >= 2);
    // the valid comment's attachment is in the backup and the malformed one's is not
    let index = std::fs::read_to_string(out.join("comments").join("objects.index.jsonl"))
        .unwrap_or_default();
    assert!(
        index.contains(&format!("{}/{}", good.1, good.3)),
        "the valid attachment is missing from the index:\n{index}"
    );
    assert!(!index.contains(&format!("{}/{}", bad.1, bad.3)), "{index}");
}
