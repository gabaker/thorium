//! Restore's confirmation when no terminal is attached

mod common;

use common::harness::Harness;
use common::names::Names;
use common::seed;
use common::state::target_state;
use std::process::{Output, Stdio};

/// Runs `thoradm backup restore` with stdin, stdout, and stderr detached from any terminal
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `backup` - The backup to restore
/// * `extra` - Extra arguments for `backup restore`
fn restore(harness: &Harness, backup: &std::path::Path, extra: &[&str]) -> Output {
    // detach stdin so no prompt could be answered
    harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "-b"])
        .arg(backup)
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm")
}

/// Makes the target differ from a backup of it in every store a restore writes: the sample's
/// rows and object are gone and a key the backup doesn't hold is added
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn diverge(harness: &Harness) {
    // empty the tables and bucket the backup holds the sample in
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for table in ["samples_list", "s3_ids"] {
        scylla
            .query_unpaged(format!("TRUNCATE {ns}.{table}"), &[])
            .await
            .unwrap();
    }
    common::harness::empty_bucket(
        &harness.s3(),
        &common::harness::bucket_for(&harness.conf, "files"),
    )
    .await
    .unwrap();
    // add a Redis key the backup doesn't hold, which a flush would remove
    let pool = common::harness::redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(format!("{ns}:confirm_marker"))
        .arg("kept")
        .query_async(&mut *conn)
        .await
        .unwrap();
}

/// Without a terminal and without `--yes` restore refuses before writing anything, naming the
/// flag; with `--yes` it proceeds
#[tokio::test]
async fn restore_without_a_terminal_needs_yes() {
    // seed a sample and back up its table
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("vesta");
    seed::group(&client, &group).await;
    seed::sample(
        &client,
        &group,
        "vesta.bin",
        b"restored only when confirmed",
    )
    .await;
    let backup = harness.scratch("confirm-backup");
    let taken = harness
        .thoradm()
        .args([
            "backup",
            "new",
            "samples-list,s3-ids,s3-ids-objects,redis",
            "-o",
        ])
        .arg(&backup)
        .output()
        .unwrap();
    assert_eq!(
        taken.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&taken.stderr)
    );
    // change the target so any write the restore made before refusing would show
    diverge(harness).await;
    let before = target_state(harness).await;
    assert_eq!(before.rows["samples_list"], 0);
    assert_eq!(before.objects["files"], Vec::<String>::new());
    // a restore without --yes and without a terminal refuses with exit 1, naming the flag
    let refused = restore(harness, &backup, &[]);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(stderr.contains("--yes"), "stderr:\n{stderr}");
    // nothing in the target changed
    assert_eq!(target_state(harness).await, before);
    // with --yes the same restore proceeds and completes
    let confirmed = restore(harness, &backup, &["--yes"]);
    let stderr = String::from_utf8_lossy(&confirmed.stderr);
    assert_eq!(confirmed.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        stderr.contains("Restoring samples list"),
        "stderr:\n{stderr}"
    );
    // the confirmed restore wrote the rows and the object back, so the refused one could have
    let after = target_state(harness).await;
    assert!(after.rows["samples_list"] > 0, "{after:?}");
    assert!(!after.objects["files"].is_empty(), "{after:?}");
}
