//! A census that logs errors finishes with failures instead of reporting success

mod common;

use common::harness::Harness;
use common::names::Names;
use std::process::Output;

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
