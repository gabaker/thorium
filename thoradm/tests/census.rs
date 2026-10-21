//! A census that logs errors finishes with failures instead of reporting success, and a census
//! of tables without partitions counts nothing on every Scylla version

mod common;

use common::harness::Harness;
use common::names::Names;
use std::process::Output;
use thoradm::args::CensusKinds;
use thoradm::census::expected_members;
use thoradm::shared::retry::RetryPolicy;

/// Runs `thoradm census new tags-case-insensitive` with one worker
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
fn census_tag_view(harness: &Harness) -> Output {
    // the case-insensitive tag census splits the ring into few ranges, so one worker is quick
    harness
        .thoradm()
        .args(["--workers", "1", "census", "new", "tags-case-insensitive"])
        .output()
        .expect("failed to run thoradm")
}

/// A census that logs a row it can't read exits 2, and the same census without that row exits 0
#[tokio::test]
async fn a_census_that_logs_errors_exits_2() {
    // write a tag row whose kind no thoradm knows
    let harness = Harness::get().await;
    let scylla = harness.scylla().await;
    let names = Names::new();
    let group = names.group("vesta");
    let ns = &harness.conf.thorium.namespace;
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, \
                 key_lower, value_lower) VALUES ('Mystery', ?, 2026, 1, 'hearth', 'fire', \
                 toTimestamp(now()), 'item', 'hearth', 'fire')"
            ),
            (group.as_str(),),
        )
        .await
        .unwrap();
    // the census logs the row and finishes with failures
    let output = census_tag_view(harness);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("census finished after 1 errors"),
        "stderr:\n{stderr}"
    );
    // without the row the census is complete
    scylla
        .query_unpaged(
            format!(
                "DELETE FROM {ns}.tags WHERE type = 'Mystery' AND group = ? AND year = 2026 \
                 AND bucket = 1 AND key = 'hearth' AND value = 'fire'"
            ),
            (group.as_str(),),
        )
        .await
        .unwrap();
    let output = census_tag_view(harness);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
}

/// A census of empty tables counts nothing and logs no errors, even when Scylla returns null rows
#[tokio::test]
async fn a_census_of_empty_tables_counts_nothing() {
    // a fresh namespace has no repos or commitishes
    let harness = Harness::get().await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    // the members a census of each empty table calls for are none
    for kind in [CensusKinds::Repos, CensusKinds::Commitishes] {
        let members = expected_members(kind, &scylla, ns, RetryPolicy::default())
            .await
            .unwrap_or_else(|error| panic!("{kind} members failed: {error}"));
        assert!(members.is_empty(), "{kind} members: {members:?}");
    }
    // a census of the empty tables, read a token range at a time, finishes without errors
    let output = harness
        .thoradm()
        .args(["--workers", "1", "census", "new", "repos", "commitishes"])
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(!stderr.contains("Census failure"), "stderr:\n{stderr}");
}
