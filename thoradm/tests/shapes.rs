//! The shapes: structural cases survive backup, wipe, and restore
//!
//! One shared run backs up and restores the empty namespace, then seeds every shape into it (in a
//! namespace with one-second time partitions), backs it up, lets a short-lived notification
//! expire, and restores. Each test checks one resource kind's share, so a kind that doesn't
//! restore yet is ignored on its own, naming the finding that blocks it.

mod common;

use common::harness::Harness;
use common::kinds::{Outcome, run_once};
use common::oracle::Snapshot;
use common::shapes::{notification_ttl, one_second_partitions};
use std::time::Duration;
use tokio::sync::OnceCell;

/// The shared run, done once per binary
static SHAPES: OnceCell<Result<Outcome, String>> = OnceCell::const_new();

/// Returns the shared run's outcome, running it on first use
async fn outcome() -> &'static Outcome {
    // a failed run is reported by every test that needs it, without running it again
    match SHAPES.get_or_init(|| run_once(run())).await {
        Ok(outcome) => outcome,
        Err(err) => panic!("the shared round trip failed: {err}"),
    }
}

/// Runs the shared round trips of the empty namespace and of the shapes
async fn run() -> Outcome {
    // the empty namespace: nothing but what a fresh deployment holds
    let harness = Harness::get_with(one_second_partitions).await;
    let mut outcome = Outcome::default();
    outcome.census(harness, "of the empty namespace");
    let empty = Snapshot::take(harness).await;
    let empty_dir = harness.scratch("empty");
    outcome.backup(harness, &empty_dir, "empty");
    Box::pin(outcome.restore(harness, &empty_dir, "empty", &empty)).await;
    // the shapes and a profile icon, with an expiring notification and one that expires before
    // the restore
    common::shapes::seed(harness).await;
    common::seed::icon(&harness.api_url, &harness.token).await;
    notification_ttl(harness, 0, 86_400).await;
    outcome.census(harness, "of the shapes");
    notification_ttl(harness, 1, 2).await;
    let dir = harness.scratch("shapes");
    outcome.backup(harness, &dir, "shapes");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let source = Snapshot::take(harness).await;
    Box::pin(outcome.restore(harness, &dir, "shapes", &source)).await;
    outcome
}

/// Checks that one resource kind came back unchanged from the empty namespace and the shapes
///
/// # Arguments
///
/// * `kind` - The resource kind
async fn assert_kind(kind: &str) {
    // run the shared round trips once and check this kind's share
    outcome().await.assert_kind(kind);
}

/// Every difference the shapes' round trips find belongs to one of the kinds
#[tokio::test]
async fn every_difference_belongs_to_a_kind() {
    // differences outside every kind would go unchecked
    let orphans = outcome().await.orphans();
    assert!(
        orphans.is_empty(),
        "differences outside every kind:\n{}",
        orphans.join("\n")
    );
}

/// Backing up the empty namespace and the shapes skips and fails nothing beyond what known
/// findings explain, whichever kind the object belongs to
#[tokio::test]
async fn backups_record_only_explained_objects() {
    // every record either backup made
    let unexpected = outcome().await.unexpected_backup_records();
    assert!(
        unexpected.is_empty(),
        "{} unexplained backup records:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
}

/// An expiring notification keeps a TTL no longer than it had left, and one that expired before
/// the restore stays gone
#[tokio::test]
async fn notification_ttls() {
    // only the TTL differences and rows the restore brought back
    let outcome = outcome().await;
    let found: Vec<String> = outcome
        .cycles
        .iter()
        .flat_map(|(_, differences)| differences)
        .filter(|difference| difference.entry == "notifications")
        .filter(|difference| {
            difference.location.ends_with("ttl()") || difference.expected.is_none()
        })
        .map(ToString::to_string)
        .collect();
    assert!(
        found.is_empty(),
        "notification TTLs differ:\n{}",
        found.join("\n")
    );
}

/// Samples come back with their view
#[tokio::test]
async fn samples() {
    // compare the samples list and its view
    assert_kind("samples").await;
}

/// S3 ids come back with their view
#[tokio::test]
async fn s3_ids() {
    // compare the s3 ids and their view
    assert_kind("s3_ids").await;
}

/// Comments come back with their view
#[tokio::test]
async fn comments() {
    // compare the comments and their view
    assert_kind("comments").await;
}

/// Tags come back with their views and recomputed lower-case columns
#[tokio::test]
async fn tags() {
    // compare the tags and their views
    assert_kind("tags").await;
}

/// Repos come back with their view
#[tokio::test]
async fn repos() {
    // compare the repo data, the repo list, and its view
    assert_kind("repos").await;
}

/// Commitishes come back with their view
#[tokio::test]
async fn commitishes() {
    // compare the commitishes, their list, and its view
    assert_kind("commitishes").await;
}

/// Network policies come back with their views
#[tokio::test]
async fn network_policies() {
    // compare the network policies and their views
    assert_kind("network_policies").await;
}

/// Results come back, with the columns format 1 doesn't carry left null
#[tokio::test]
async fn results() {
    // compare the results
    assert_kind("results").await;
}

/// The results stream comes back with its views, with the columns format 1 doesn't carry left null
#[tokio::test]
async fn results_stream() {
    // compare the results stream and its views
    assert_kind("results_stream").await;
}

/// Nodes come back registered, with their reset fields at their reset values
#[tokio::test]
async fn nodes() {
    // compare the nodes
    assert_kind("nodes").await;
}

/// Entities come back with their views
#[tokio::test]
async fn entities() {
    // compare the entities and their views
    assert_kind("entities").await;
}

/// Associations come back
#[tokio::test]
async fn associations() {
    // compare the associations
    assert_kind("associations").await;
}

/// Notifications come back, with their TTLs bounded
#[tokio::test]
async fn notifications() {
    // compare the notifications
    assert_kind("notifications").await;
}

/// File and repo objects come back byte for byte
#[tokio::test]
async fn objects() {
    // compare the files and repos buckets
    assert_kind("objects").await;
}

/// Comment attachments and result files come back byte for byte
#[tokio::test]
async fn attachments() {
    // compare the attachments and results buckets
    assert_kind("attachments").await;
}

/// Users, their tokens, and their groups come back
#[tokio::test]
async fn users() {
    // compare the user families
    assert_kind("users").await;
}

/// Groups, their members, and their network-policy sets come back
#[tokio::test]
async fn groups() {
    // compare the group families
    assert_kind("groups").await;
}

/// The system settings come back
#[tokio::test]
async fn settings() {
    // compare the settings
    assert_kind("settings").await;
}

/// Graphics objects come back byte for byte
#[tokio::test]
async fn graphics() {
    // compare the graphics bucket
    assert_kind("graphics").await;
}

/// A census rebuilt after the restore matches one rebuilt on the source
#[tokio::test]
async fn census_streams() {
    // compare the census streams
    assert_kind("census").await;
}

/// Excluded runtime tables and buckets are unchanged by the round trip
#[tokio::test]
async fn excluded() {
    // compare the excluded tables and buckets
    assert_kind("excluded").await;
}
