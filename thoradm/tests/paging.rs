//! Reading a token range survives failed pages: backups and censuses resume the range instead of
//! losing the rest of it, and a range that never reads is recorded

mod common;

use common::archives;
use common::cycle::{describe, verify};
use common::harness::Harness;
use common::names::Names;
use common::proxy::{FaultProxy, ProxyAction, ProxyRule};
use futures::stream::{self, StreamExt};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use thoradm::args::{BackupComponents, CensusKinds, NewBackup, NewCensus};
use thoradm::backup::controllers::backup;
use thoradm::backup::failures::{FAILURES_FILE, FailureKind, Operation, read_records};
use thoradm::backup::manifest::Manifest;
use thoradm::backup::sink::TokioFs;
use thoradm::backup::tables::Tag;
use thoradm::outcome::RunOutcome;
use thorium::{Conf, CtlConf};
use tokio::sync::OnceCell;

/// How many tag partitions the seeded group holds
const PARTITIONS: i32 = 1_500;

/// How many rows each seeded partition holds; 137 doesn't divide the driver's default page of
/// 5,000 rows, so pages end inside partitions
const ROWS_PER_PARTITION: i32 = 137;

/// The group every seeded tag row belongs to, seeded once per binary
static SEEDED: OnceCell<String> = OnceCell::const_new();

/// Seeds 205,500 tag rows in 1,500 partitions, once per binary, and returns their group
///
/// Each partition holds 137 rows, so pages of the driver's default size end in the middle of a
/// partition.
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn seed_tags(harness: &Harness) -> String {
    SEEDED
        .get_or_init(|| async {
            // one group for every row, named for this binary's run
            let group = Names::new().group("vesta");
            let scylla = harness.scylla().await;
            let ns = &harness.conf.thorium.namespace;
            let insert = scylla
                .prepare(format!(
                    "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, \
                     item, key_lower, value_lower) VALUES ('Files', ?, 2026, ?, 'hearth', 'fire', \
                     '2026-02-28 12:00:00+0000', ?, 'hearth', 'fire')"
                ))
                .await
                .expect("failed to prepare the tag insert");
            // insert every row with many writes in flight
            let rows = (0..PARTITIONS)
                .flat_map(|bucket| (0..ROWS_PER_PARTITION).map(move |item| (bucket, item)));
            stream::iter(rows)
                .map(|(bucket, item)| {
                    let (scylla, insert, group) = (&scylla, &insert, &group);
                    async move {
                        scylla
                            .execute_unpaged(
                                insert,
                                (group.as_str(), bucket, format!("item-{item:03}")),
                            )
                            .await
                            .expect("failed to insert a tag row");
                    }
                })
                .buffer_unordered(256)
                .collect::<()>()
                .await;
            group
        })
        .await
        .clone()
}

/// Starts a proxy in front of Scylla and returns it with a config that reaches Scylla only
/// through it
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn proxied(harness: &Harness) -> (FaultProxy, Conf) {
    // put a proxy in front of the node and point a copy of the config at it
    let node: SocketAddr = harness.conf.scylla.nodes[0].parse().unwrap();
    let proxy = FaultProxy::start(node).await.unwrap();
    let mut conf = harness.conf.clone();
    conf.scylla.nodes = vec![proxy.listen.to_string()];
    // route every connection a session makes through the proxy, whatever address the node
    // advertises
    let translator =
        common::proxy::scylla_translator(&*harness.scylla().await, node, proxy.listen).await;
    thoradm::shared::scylla::route_through(&proxy.listen.to_string(), Arc::new(translator));
    (proxy, conf)
}

/// Backs up the tags table into a directory with one worker reading the whole ring as one range
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `conf` - The config whose Scylla nodes the backup reads
/// * `out` - The backup directory
async fn backup_tags(harness: &Harness, conf: &Conf, out: &Path) -> RunOutcome {
    // build the session the backup reads with from the given config
    let scylla = Arc::new(
        thoradm::shared::scylla::get_client(conf)
            .await
            .expect("failed to open a session"),
    );
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).expect("failed to load the ctl config");
    let args = NewBackup {
        components: vec![BackupComponents::Tags],
        output: out.to_path_buf(),
        multiplier: 1,
        retries: 5,
        s3_concurrency: 64,
        abort_on_failure: false,
        allow_version_mismatch: false,
        prune_dangling: false,
        accept_broken_links: false,
    };
    Box::pin(backup::run(
        conf,
        ctl_conf,
        &scylla,
        1,
        &args,
        Arc::new(TokioFs),
    ))
    .await
    .expect("the backup failed to run")
}

/// Returns the (bucket, item) of every seeded row a backup's tag archives hold
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `group` - The seeded group
async fn backed_up(out: &Path, group: &str) -> Vec<(i32, String)> {
    // read every row of the tag archives and keep the seeded group's
    let rows = archives::rows::<Tag, _, _>(&out.join("tags"), |row| {
        (row.group.to_string(), row.bucket, row.item.to_string())
    })
    .await;
    rows.into_iter()
        .filter(|(row_group, _, _)| row_group == group)
        .map(|(_, bucket, item)| (bucket, item))
        .collect()
}

/// Asserts a backup holds every seeded row exactly once
///
/// # Arguments
///
/// * `rows` - The rows the backup holds
fn assert_every_row_once(rows: &[(i32, String)]) {
    // every row is there, and none twice
    let expected = usize::try_from(PARTITIONS * ROWS_PER_PARTITION).unwrap();
    let unique: BTreeSet<&(i32, String)> = rows.iter().collect();
    assert_eq!(unique.len(), expected, "distinct rows backed up");
    assert_eq!(rows.len(), expected, "rows backed up, duplicates included");
}

/// A backup whose connections are reset after the first pages, and stalled for longer than the
/// request timeout, still archives every row of the range and exits 0
#[tokio::test]
async fn reset_and_stalled_pages_resume_the_range() {
    let harness = Harness::get().await;
    let group = seed_tags(harness).await;
    let (proxy, conf) = proxied(harness).await;
    // reset two connections after the first pages, then stall one longer than the 30 s timeout;
    // the control connection (the first one) keeps the session's metadata working
    proxy
        .set_rules(vec![
            ProxyRule {
                from_connection: Some(1),
                after_bytes: 1_000_000,
                upstream: false,
                action: ProxyAction::Reset,
                times: Some(2),
            },
            ProxyRule {
                from_connection: Some(1),
                after_bytes: 3_000_000,
                upstream: false,
                action: ProxyAction::Stall(Duration::from_secs(35)),
                times: Some(1),
            },
        ])
        .await;
    let before = proxy.connections();
    let out = harness.scratch("reset-and-stall");
    let outcome = Box::pin(backup_tags(harness, &conf, &out)).await;
    // the faults happened: the reset connections were replaced
    assert!(
        proxy.connections() >= before + 2,
        "no connection was reset ({before} before, {} after)",
        proxy.connections()
    );
    // nothing was recorded and every row is in the backup once
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    assert!(records.is_empty(), "{records:?}");
    assert_eq!(outcome, RunOutcome::Complete);
    assert_every_row_once(&backed_up(&out, &group).await);
}

/// A range whose pages keep failing is recorded as a failed read of that range and the backup
/// exits 2; once the fault clears, a fresh backup exits 0 and passes `backup verify`
#[tokio::test]
async fn a_range_that_never_reads_is_recorded() {
    let harness = Harness::get().await;
    seed_tags(harness).await;
    let (proxy, conf) = proxied(harness).await;
    // reset every pooled connection after a little data, for as long as the rule is set
    proxy
        .set_rules(vec![ProxyRule {
            from_connection: Some(1),
            after_bytes: 200_000,
            upstream: false,
            action: ProxyAction::Reset,
            times: None,
        }])
        .await;
    let out = harness.scratch("never-reads");
    let outcome = Box::pin(backup_tags(harness, &conf, &out)).await;
    // the run finished with the range recorded
    assert_eq!(outcome, RunOutcome::CompletedWithFailures);
    let records = read_records(&out.join(FAILURES_FILE)).unwrap();
    let range = records
        .iter()
        .find(|record| record.operation == Operation::ReadRange)
        .unwrap_or_else(|| panic!("no read-range record in {records:?}"));
    assert_eq!(range.kind, FailureKind::Failed);
    assert_eq!(range.component, "tags");
    assert_eq!(range.table.as_deref(), Some("tags"));
    assert_eq!(range.token_range, Some((i64::MIN, i64::MAX)));
    assert!(range.attempts > 1, "the range was never retried: {range:?}");
    // once the fault clears, a fresh backup is complete and verifies
    proxy.set_rules(Vec::new()).await;
    let held = tag_rows(harness).await;
    let out = harness.scratch("never-reads-cleared");
    let outcome = Box::pin(backup_tags(harness, &conf, &out)).await;
    assert_eq!(outcome, RunOutcome::Complete);
    let verified = verify(harness, &out);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
    // and it holds every tag row the table held, which verify alone can't see
    let manifest = Manifest::read(&out)
        .await
        .unwrap()
        .expect("the cleared backup wrote no manifest");
    assert_eq!(manifest.tables["tags"].rows, held);
}

/// Counts the rows the tags table holds, read straight from Scylla
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn tag_rows(harness: &Harness) -> u64 {
    // count every row, giving the count of a few hundred thousand rows time to finish
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let mut count =
        scylla::statement::unprepared::Statement::new(format!("SELECT COUNT(*) FROM {ns}.tags"));
    count.set_request_timeout(Some(Duration::from_secs(300)));
    let (rows,) = scylla
        .query_unpaged(count, ())
        .await
        .expect("failed to count the tag rows")
        .into_rows_result()
        .unwrap()
        .single_row::<(i64,)>()
        .unwrap();
    u64::try_from(rows).unwrap()
}

/// A census whose connections are reset after the first pages resumes each range and finishes
/// without errors
#[tokio::test]
async fn a_census_resumes_a_range_after_a_reset() {
    let harness = Harness::get().await;
    seed_tags(harness).await;
    let (proxy, conf) = proxied(harness).await;
    // reset two pooled connections a little way into the census's reads
    proxy
        .set_rules(vec![ProxyRule {
            from_connection: Some(1),
            after_bytes: 20_000,
            upstream: false,
            action: ProxyAction::Reset,
            times: Some(2),
        }])
        .await;
    let census = NewCensus {
        census_kinds: vec![CensusKinds::Tags],
        multiplier: 100,
        dry_run: true,
    };
    let outcome = thoradm::census::new(&census, &conf, 1).await;
    // every range was counted, so the census is complete
    assert!(
        matches!(outcome, Ok(RunOutcome::Complete)),
        "the census didn't complete: {outcome:?}"
    );
}

/// The seeded partitions don't divide the driver's default page size, so reading the ring a page
/// at a time ends pages inside partitions, the case a resumed range has to get right
#[tokio::test]
async fn default_pages_end_inside_seeded_partitions() {
    let harness = Harness::get().await;
    let group = seed_tags(harness).await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let prepared = scylla
        .prepare(format!(
            "SELECT group, bucket FROM {ns}.tags WHERE token(type, group, year, bucket, key, \
             value) >= ? AND token(type, group, year, bucket, key, value) <= ?"
        ))
        .await
        .expect("failed to prepare the page read");
    // read the whole ring a page at a time, counting pages whose first row continues the
    // partition the previous page ended in
    let mut paging = scylla::response::PagingState::start();
    let mut last: Option<(String, i32)> = None;
    let (mut pages, mut split) = (0, 0);
    loop {
        let (result, response) = scylla
            .execute_single_page(&prepared, &(i64::MIN, i64::MAX), paging.clone())
            .await
            .expect("failed to read a page");
        let rows: Vec<(String, i32)> = result
            .into_rows_result()
            .unwrap()
            .rows::<(String, i32)>()
            .unwrap()
            .map(Result::unwrap)
            .collect();
        pages += 1;
        if let (Some(previous), Some(first)) = (&last, rows.first())
            && previous == first
            && previous.0 == group
        {
            split += 1;
        }
        last = rows.last().cloned();
        // stop after the last page
        match response.into_paging_control_flow() {
            std::ops::ControlFlow::Continue(next) => paging = next,
            std::ops::ControlFlow::Break(()) => break,
        }
    }
    assert!(split > 0, "none of {pages} pages ended inside a partition");
}
