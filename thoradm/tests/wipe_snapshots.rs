//! The truncate snapshots a `--wipe` restore leaves: on a Scylla with `auto_snapshot` on, the plan
//! and the final report list them with their size, say they hold the wiped data and its disk, and
//! print the `nodetool clearsnapshot` command, and the wipe never clears them
//!
//! Run this binary against a Scylla with `auto_snapshot` on, as Scylla's default has it. Against
//! one with it off the test fails, since it can't check the snapshots, unless
//! `THORADM_TEST_AUTO_SNAPSHOT_OFF` is set: then it checks only that the plan says the truncates
//! leave no snapshots. The test restores, so it holds the binary's restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::harness::Harness;
use common::names::Names;
use common::seed;
use serde_json::Value;
use std::collections::BTreeSet;
use std::process::Stdio;

/// The components the backup holds: samples with their objects, tags, and Redis
const COMPONENTS: &str = "samples-list,s3-ids,s3-ids-objects,tags,redis";

/// Set when the test Scylla runs with `auto_snapshot` off on purpose
const AUTO_SNAPSHOT_OFF: &str = "THORADM_TEST_AUTO_SNAPSHOT_OFF";

/// Returns whether Scylla's `auto_snapshot` is on for the node the harness's session asks
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn auto_snapshot(harness: &Harness) -> bool {
    // Scylla's node-local config table
    let rows = harness
        .scylla()
        .await
        .query_unpaged(
            "SELECT value FROM system.config WHERE name = 'auto_snapshot'",
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let (value,): (String,) = rows.first_row().unwrap();
    value.trim_matches('"') == "true"
}

/// Lists the snapshots Scylla holds of the namespace's tables, as `(table, snapshot)`
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn snapshots(harness: &Harness) -> BTreeSet<(String, String)> {
    // Scylla's node-local listing, the one `nodetool listsnapshots` prints
    let rows = harness
        .scylla()
        .await
        .query_unpaged(
            "SELECT table_name, snapshot_name FROM system.snapshots WHERE keyspace_name = ?",
            (&harness.conf.thorium.namespace,),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    rows.rows::<(String, String)>()
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// A `--wipe` restore on a non-empty target prints a plan naming the truncate snapshot of every
/// table it wipes with its size, that they hold the wiped data and its disk, and the
/// `nodetool clearsnapshot <keyspace>` command to run once `verify --cluster` passes; the final
/// report lists every snapshot the wipe left with its size and the same command; and the
/// snapshots are still there afterwards
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_wipe_lists_the_truncate_snapshots_it_leaves_and_never_clears_them() {
    // a sample in a group of its own, backed up, and still in the target
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a Scylla without auto_snapshot leaves nothing to check, unless that is what the run wants
    let auto = auto_snapshot(harness).await;
    assert!(
        auto || std::env::var_os(AUTO_SNAPSHOT_OFF).is_some(),
        "the test Scylla runs with auto_snapshot off, so the snapshots a wipe leaves can't be \
         checked; run this test against a Scylla with auto_snapshot on, or set \
         {AUTO_SNAPSHOT_OFF}=1 to check only that the plan says nothing is kept"
    );
    wipe(harness).await;
    let client = harness.client().await;
    client.system.init().await.unwrap();
    let names = Names::new();
    let group = names.group("pallas");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "pallas.bin", names.suffix.as_bytes()).await;
    let dir = harness.scratch("snapshots");
    let backup = dir.join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", COMPONENTS, "-o"])
        .arg(&backup)
        .output()
        .unwrap();
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // the snapshots Scylla held before the wipe
    let before = snapshots(harness).await;
    // the wipe restore runs without a terminal under --yes
    let report_dir = dir.join("report");
    let restored = harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--yes", "-b"])
        .arg(&backup)
        .arg("--report-dir")
        .arg(&report_dir)
        .args(["--wipe", ns.as_str()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr).into_owned();
    // the plan is everything logged before the wipe was confirmed
    let confirmed = stderr
        .find("confirmed by --wipe")
        .unwrap_or_else(|| panic!("no confirmation:\n{stderr}"));
    let (plan, after) = stderr.split_at(confirmed);
    let summary: Value =
        serde_json::from_slice(&std::fs::read(report_dir.join("summary.json")).unwrap()).unwrap();
    let report = &summary["wipe"];
    let tables: Vec<String> = report["tables"]
        .as_array()
        .unwrap_or_else(|| panic!("no wipe in the summary: {summary:#}"))
        .iter()
        .map(|table| table["table"].as_str().unwrap().to_owned())
        .collect();
    assert!(tables.contains(&"samples_list".to_owned()), "{report:#}");
    let left = report["snapshots"].as_array().cloned().unwrap_or_default();
    let command = format!("nodetool clearsnapshot {ns}");
    if !auto {
        // without auto_snapshot the plan says nothing is kept, and the report lists nothing
        assert!(plan.contains("auto_snapshot is off"), "{plan}");
        assert!(left.is_empty(), "{report:#}");
        return;
    }
    // the plan names the snapshot of every wiped table with a size, what they hold, and the
    // command to clear them once verify --cluster passes
    for table in &tables {
        let line = plan
            .lines()
            .find(|line| {
                line.contains("snapshot of table") && line.contains(&format!("{ns}.{table}:"))
            })
            .unwrap_or_else(|| panic!("the plan names no snapshot of {table}:\n{plan}"));
        assert!(
            line.contains("bytes") || line.contains("size unknown"),
            "{line}"
        );
    }
    for text in [
        "hold the wiped data and its disk",
        &command,
        "verify --cluster",
    ] {
        assert!(plan.contains(text), "the plan lacks {text:?}:\n{plan}");
    }
    // the report lists every snapshot the wipe left, each with its size
    let after_wipe = snapshots(harness).await;
    let made: BTreeSet<(String, String)> = after_wipe
        .difference(&before)
        .filter(|(table, _)| tables.contains(table))
        .cloned()
        .collect();
    assert!(
        !made.is_empty(),
        "the wipe left no snapshot: {after_wipe:?}"
    );
    let listed: BTreeSet<(String, String)> = left
        .iter()
        .map(|snapshot| {
            (
                snapshot["table"].as_str().unwrap().to_owned(),
                snapshot["snapshot"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(listed, made, "{report:#}");
    let samples = left
        .iter()
        .find(|snapshot| snapshot["table"] == "samples_list")
        .unwrap_or_else(|| panic!("no snapshot of samples_list: {report:#}"));
    // a node with more than one shard can leave a snapshot without rows written just before the
    // truncate, so only the size fields are required, not a size
    assert!(samples["total"].is_u64(), "{samples}");
    assert!(samples["live"].is_u64(), "{samples}");
    assert!(samples["node"].is_string(), "{samples}");
    // the final log lists them with their size and the command
    for (table, snapshot) in &made {
        assert!(
            after.lines().any(|line| line.contains("restore summary")
                && line.contains(snapshot.as_str())
                && line.contains(&format!("{ns}.{table}"))
                && line.contains("bytes")),
            "the summary doesn't list {snapshot} of {table}:\n{after}"
        );
    }
    assert!(
        after
            .lines()
            .any(|line| line.contains("restore summary") && line.contains(&command)),
        "{after}"
    );
    // nothing cleared them
    let still = snapshots(harness).await;
    assert!(made.is_subset(&still), "{made:?} not in {still:?}");
}
