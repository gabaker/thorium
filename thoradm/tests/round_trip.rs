//! The deep round trip: the value-class fixture survives backup, wipe, and restore, twice
//!
//! One shared run seeds the fixture, snapshots it, and takes it through two backup, wipe, and
//! restore cycles, snapshotting after each. Each test then checks one resource kind's entries, so
//! a kind that doesn't restore yet is ignored on its own, naming the finding that blocks it.

mod common;

use common::fixture;
use common::harness::Harness;
use common::kinds::{Outcome, run_once};
use common::oracle::Snapshot;
use tokio::sync::OnceCell;

/// The shared run, done once per binary
static ROUND_TRIP: OnceCell<Result<Outcome, String>> = OnceCell::const_new();

/// Returns the shared run's outcome, running it on first use
async fn outcome() -> &'static Outcome {
    // a failed run is reported by every test that needs it, without running it again
    match ROUND_TRIP.get_or_init(|| run_once(run())).await {
        Ok(outcome) => outcome,
        Err(err) => panic!("the shared round trip failed: {err}"),
    }
}

/// Runs the shared round trip: two cycles of backup, wipe, and restore, each compared with the
/// source
async fn run() -> Outcome {
    // seed the fixture, whose graphics and entity blobs a disaster would lose too
    let harness = Harness::get().await;
    fixture::broad(harness).await;
    let _ = Box::pin(fixture::value_classes(harness)).await;
    // rebuild the source's census so it counts what the tables hold
    let mut outcome = Outcome::default();
    outcome.census(harness, "of the source");
    let source = Snapshot::take(harness).await;
    outcome.check_members(&source);
    // back up the source, restore it, then back up and restore the result again
    let first_dir = harness.scratch("first");
    let first = outcome.backup(harness, &first_dir, "first");
    Box::pin(outcome.restore(harness, &first_dir, "first", &source)).await;
    let second_dir = harness.scratch("second");
    let second = outcome.backup(harness, &second_dir, "second");
    Box::pin(outcome.restore(harness, &second_dir, "second", &source)).await;
    // both backups count the same objects in every component
    outcome.compare_counts(&first, &second);
    // and their manifests agree on every count
    outcome.compare_manifests(&first_dir, &second_dir).await;
    outcome
}

/// Checks that one resource kind came back unchanged after both cycles
///
/// # Arguments
///
/// * `kind` - The resource kind
async fn assert_kind(kind: &str) {
    // run the shared round trip once and check this kind's share
    outcome().await.assert_kind(kind);
}

/// Every difference the round trip finds belongs to one of the kinds
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

/// Backing up the fixture skips and fails nothing beyond what known findings explain, whichever
/// kind the object belongs to, so losses in kinds whose checks are ignored still fail
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

/// Every group member the fixture seeds is a user, as the API requires, so the user and group
/// kinds check states the API can reach
#[tokio::test]
async fn fixture_group_members_are_users() {
    // members without a user record
    let strays = &outcome().await.non_user_members;
    assert!(
        strays.is_empty(),
        "{} group members aren't users:\n{}",
        strays.len(),
        strays
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
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

/// Results come back, with the columns format 1 doesn't carry left null, except their children,
/// which `KNOWN_LOSSES` names
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

/// Comment attachments, result files, and entity blobs come back byte for byte
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

/// Images and pipelines come back with their bans and recomputed users
#[tokio::test]
async fn images_and_pipelines() {
    // compare the image and pipeline families
    assert_kind("images_and_pipelines").await;
}

/// The system settings come back
#[tokio::test]
async fn settings() {
    // compare the settings
    assert_kind("settings").await;
}

/// User icons and entity images come back byte for byte
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
