//! The truncate snapshots a `--wipe` restore leaves, and what its plan says about them
//!
//! Three tests: one read-only test that the queries the wipe reads the node's `auto_snapshot`
//! setting and its `system.snapshots` rows with return rows of the shape it expects, and the two
//! branches of the real wipe. A job can't restart its Scylla or change its `auto_snapshot`, so the
//! wipe tests read the setting from the node by CQL and run the branch that matches it: with it on,
//! the plan and the final report list the snapshots with their size, say they hold the wiped data
//! and its disk, and print the clear command, and the wipe never clears them; with it off, the plan
//! says nothing is kept. The branch that doesn't match prints `SKIPPED: <test>: auto_snapshot is
//! <on|off>` and appends the line to `skipped.txt` in `THORADM_REPORT_DIR` when it is set, so a job
//! can list it. Run the binary against a node with the other setting to run the other branch.
//! The wipe tests restore, so each holds the binary's restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::harness::Harness;
use common::names::Names;
use common::seed;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Write;
use std::process::Stdio;
use thoradm::backup::wipe::{ScyllaSnapshots, SnapshotSource, clear_snapshots_command};

/// The components the backup holds: samples with their objects, tags, and Redis
const COMPONENTS: &str = "samples-list,s3-ids,s3-ids-objects,tags,redis";

/// Returns whether Scylla's `auto_snapshot` is on for every node, read the way the wipe reads it
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn auto_snapshot(harness: &Harness) -> Option<bool> {
    // the wipe's own source reads the setting from each node's config table
    let scylla = harness.scylla().await;
    ScyllaSnapshots { scylla: &scylla }.auto_snapshot().await
}

/// Says a test branch was skipped: prints a `SKIPPED` line and appends it to `skipped.txt` in the
/// report directory when `THORADM_REPORT_DIR` is set
///
/// # Arguments
///
/// * `test` - The test's name
/// * `setting` - What the node's `auto_snapshot` is
fn skip(test: &str, setting: &str) {
    // libtest can't skip at run time, so the skip is visible in the output and in the job's list
    let line = format!("SKIPPED: {test}: auto_snapshot is {setting}");
    println!("{line}");
    if let Some(dir) = std::env::var_os("THORADM_REPORT_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::path::Path::new(&dir).join("skipped.txt"))
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
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

/// What a `--wipe` restore printed and recorded
struct Wiped {
    /// Everything logged before the wipe was confirmed: the plan
    plan: String,
    /// Everything logged from the confirmation on
    after: String,
    /// The `wipe` section of the restore's summary
    report: Value,
    /// The tables the wipe truncated
    tables: Vec<String>,
    /// The `(table, snapshot)` pairs Scylla held before the wipe
    before: BTreeSet<(String, String)>,
}

/// Seeds a backed-up sample, then runs a `--wipe` restore of it over a target that still holds it
///
/// The caller holds the restore guard.
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn wipe_restore(harness: &Harness) -> Wiped {
    // a sample in a group of its own, backed up, and still in the target
    let ns = harness.conf.thorium.namespace.clone();
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
    // the wipe restore runs without a terminal under --confirm
    let report_dir = dir.join("report");
    let restored = harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
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
    let report = summary["wipe"].clone();
    let tables: Vec<String> = report["tables"]
        .as_array()
        .unwrap_or_else(|| panic!("no wipe in the summary: {summary:#}"))
        .iter()
        .map(|table| table["table"].as_str().unwrap().to_owned())
        .collect();
    assert!(tables.contains(&"samples_list".to_owned()), "{report:#}");
    Wiped {
        plan: plan.to_owned(),
        after: after.to_owned(),
        report,
        tables,
        before,
    }
}

/// The queries the wipe reads the node's `auto_snapshot` setting and its `system.snapshots` rows
/// with return rows of the shape it expects, whatever the setting is: the setting reads back, and
/// listing the snapshots of a keyspace that doesn't exist is empty
///
/// It changes nothing, so it catches a Scylla version that changes the system tables without
/// running a wipe.
#[tokio::test]
async fn a_snapshot_query_has_the_shape_the_wipe_reads() {
    // the wipe's own source, over the harness's session
    let harness = Harness::get().await;
    let scylla = harness.scylla().await;
    let source = ScyllaSnapshots { scylla: &scylla };
    // the setting reads back as on or off, on every node
    assert!(source.auto_snapshot().await.is_some());
    // the listing of a keyspace nobody created is empty, and its rows decode
    let absent = format!("{}_absent", harness.conf.thorium.namespace);
    let listed = source.list(&absent, &["samples_list"]).await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");
}

/// With `auto_snapshot` on, a `--wipe` restore on a non-empty target prints a plan naming the
/// truncate snapshot of every table it wipes with its size, that they hold the wiped data and its
/// disk, and the clear command to run once `verify --cluster` passes; the final report lists every
/// snapshot the wipe left with its size and the same command; and the snapshots are still there
/// afterwards
///
/// It runs only on a node with the setting on, and prints a `SKIPPED` line otherwise.
#[tokio::test]
async fn a_wipe_with_auto_snapshot_on_lists_its_snapshots() {
    // the node's own setting decides whether this branch can run
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    if auto_snapshot(harness).await != Some(true) {
        skip("a_wipe_with_auto_snapshot_on_lists_its_snapshots", "off");
        return;
    }
    let ns = harness.conf.thorium.namespace.clone();
    let Wiped {
        plan,
        after,
        report,
        tables,
        before,
    } = wipe_restore(harness).await;
    let left = report["snapshots"].as_array().cloned().unwrap_or_default();
    let command = clear_snapshots_command(&ns);
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

/// With `auto_snapshot` off, a `--wipe` restore's plan says the truncates leave no snapshots and
/// gives no clear command, and the report lists none
///
/// It runs only on a node with the setting off, and prints a `SKIPPED` line otherwise.
#[tokio::test]
async fn a_wipe_with_auto_snapshot_off_says_nothing_is_kept() {
    // the node's own setting decides whether this branch can run
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    if auto_snapshot(harness).await != Some(false) {
        skip("a_wipe_with_auto_snapshot_off_says_nothing_is_kept", "on");
        return;
    }
    let ns = harness.conf.thorium.namespace.clone();
    let Wiped { plan, report, .. } = wipe_restore(harness).await;
    // the plan says nothing is kept, and the report lists nothing
    assert!(plan.contains("auto_snapshot is off"), "{plan}");
    assert!(
        !plan.contains(&clear_snapshots_command(&ns)),
        "the plan names a clear command:\n{plan}"
    );
    let left = report["snapshots"].as_array().cloned().unwrap_or_default();
    assert!(left.is_empty(), "{report:#}");
}
