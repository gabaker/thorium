//! A restore takes the namespace's writer lease around each of its two schema applies: it is
//! refused while another writer holds the lease, releases the lease after each apply, and refuses
//! when Redis can't be reached
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::describe;
use common::harness::{Harness, redis_pool, write_cluster_conf};
use common::record::{self, Rows, backup_of, restore};
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thorium::models::schema::{MigrationRecordKind, WriterLease, lease_key};

/// The history the source deployments of these tests record
const STEPS: [&str; 2] = ["scylla-role", "finalize"];

/// The holder name another writer's lease carries, which no restore of these tests is named
const RIVAL: &str = "rival-migration-job";

/// Reads the value of the namespace's writer lease key
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn lease_value(harness: &Harness) -> Option<String> {
    // the key the lease lives under, read from the same Redis the restore uses
    let pool = redis_pool(&harness.conf).await.expect("Redis answers");
    let mut conn = pool.get().await.expect("a Redis connection");
    redis::cmd("GET")
        .arg(lease_key(&harness.conf.thorium.namespace))
        .query_async(&mut *conn)
        .await
        .expect("the lease key reads")
}

/// Records every change to the namespace's writer lease key through Redis keyspace notifications,
/// so a test counts how often the lease was taken and given up however briefly it was held
struct LeaseEvents {
    /// The events seen so far, such as `set`, `expire` and `del`, in order
    seen: Arc<Mutex<Vec<String>>>,
    /// The task collecting them
    task: tokio::task::JoinHandle<()>,
    /// The notification setting Redis had before, restored by [`LeaseEvents::finish`]
    previous: String,
    /// The harness's Redis, for restoring the setting
    harness: &'static Harness,
}

impl LeaseEvents {
    /// Starts recording the lease key's events
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the namespace and its Redis
    async fn start(harness: &'static Harness) -> LeaseEvents {
        // enable keyspace notifications for every event, remembering the setting to put back
        let pool = redis_pool(&harness.conf).await.expect("Redis answers");
        let mut conn = pool.get().await.expect("a Redis connection");
        let current: Vec<String> = redis::cmd("CONFIG")
            .arg("GET")
            .arg("notify-keyspace-events")
            .query_async(&mut *conn)
            .await
            .expect("the setting reads");
        let previous = current.get(1).cloned().unwrap_or_default();
        let _: () = redis::cmd("CONFIG")
            .arg("SET")
            .arg("notify-keyspace-events")
            .arg("KEA")
            .query_async(&mut *conn)
            .await
            .expect("the setting is written");
        // subscribe to the lease key's channel on every database
        let redis = &harness.conf.redis;
        let url = match &redis.password {
            Some(password) => format!(
                "redis://{}:{password}@{}:{}/",
                redis.username.as_deref().unwrap_or("default"),
                redis.host,
                redis.port
            ),
            None => format!("redis://{}:{}/", redis.host, redis.port),
        };
        let client = redis::Client::open(url).expect("a Redis client");
        let mut pubsub = client.get_async_pubsub().await.expect("a subscription");
        let channel = format!(
            "__keyspace@*__:{}",
            lease_key(&harness.conf.thorium.namespace)
        );
        pubsub
            .psubscribe(channel)
            .await
            .expect("the channel subscribes");
        // collect each event's name until the task is stopped
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        let task = tokio::spawn(async move {
            let mut messages = pubsub.on_message();
            while let Some(message) = messages.next().await {
                let event: String = message.get_payload().unwrap_or_default();
                kept.lock().unwrap().push(event);
            }
        });
        LeaseEvents {
            seen,
            task,
            previous,
            harness,
        }
    }

    /// Stops recording and puts Redis's notification setting back
    ///
    /// # Returns
    ///
    /// The events recorded
    async fn finish(self) -> Vec<String> {
        // let the last notifications arrive, then stop collecting
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.task.abort();
        let pool = redis_pool(&self.harness.conf).await.expect("Redis answers");
        let mut conn = pool.get().await.expect("a Redis connection");
        let _: () = redis::cmd("CONFIG")
            .arg("SET")
            .arg("notify-keyspace-events")
            .arg(&self.previous)
            .query_async(&mut *conn)
            .await
            .expect("the setting is restored");
        self.seen.lock().unwrap().clone()
    }
}

/// Sets a source deployment's backup and a different target deployment in the namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
///
/// # Returns
///
/// The backup root and the rows the target holds
async fn backup_and_target(harness: &Harness, name: &str) -> (std::path::PathBuf, Rows) {
    // a backup of one deployment, and a target holding another's record
    let source = record::deployment("2026-10-v02", "src", 2, &STEPS);
    let (backup, _) = backup_of(harness, name, &source).await;
    let target = record::deployment("2026-10-v02", "tgt", 2, &["scylla-role"]);
    record::empty_target(harness).await;
    record::set(harness, &target).await;
    (backup, target)
}

/// A restore started while another writer holds the lease is refused before it writes anything,
/// naming the holder, and the lease stays the other writer's
#[tokio::test]
async fn a_restore_is_refused_while_another_writer_holds_the_lease() {
    // another writer takes the lease before the restore starts
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, target) = backup_and_target(harness, "lease-held").await;
    let before = record::read(harness).await;
    let pool = redis_pool(&harness.conf).await.expect("Redis answers");
    let namespace = harness.conf.thorium.namespace.clone();
    let rival = WriterLease::acquire(&pool, &namespace, RIVAL, false)
        .await
        .expect("the lease is free");
    // the restore refuses, naming the holder
    let report = backup.parent().unwrap().join("report");
    let output = restore(harness, &backup, &report);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(RIVAL), "{stderr}");
    assert!(stderr.contains("writer lease"), "{stderr}");
    // a restore has no flag to break the lease, so its refusal doesn't offer one
    assert!(!stderr.contains("--force-break-lease"), "{stderr}");
    // the failure log records the refused apply as a step before any row is restored
    let failures = std::fs::read_to_string(report.join("failures.jsonl")).expect("failures read");
    let applies = failures
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a failure record"))
        .filter(|record| record["key"] == "schema apply")
        .collect::<Vec<_>>();
    assert_eq!(applies.len(), 1, "{failures}");
    assert_eq!(applies[0]["operation"], "pre-restore", "{failures}");
    // the other writer still holds the lease
    let value = lease_value(harness).await.expect("the lease is still held");
    assert!(value.starts_with(RIVAL), "{value}");
    rival.release().await;
    // nothing was changed: the record is the target's, with no history entry from a restore
    let after = record::read(harness).await;
    assert_eq!(after, before);
    assert_eq!(
        after
            .kind(MigrationRecordKind::History)
            .iter()
            .filter(|row| row
                .entry
                .as_ref()
                .is_some_and(|entry| entry.0.step == "restored"))
            .count(),
        0
    );
    assert!(
        after
            .get(MigrationRecordKind::JobResult, "uid-tgt")
            .is_some(),
        "{target:?}"
    );
}

/// With the lease free, a restore holds it during each of its two schema applies and gives it up
/// after each, so the key is gone when the restore ends and a second restore isn't refused
#[tokio::test]
async fn a_restore_holds_the_lease_for_each_apply_and_releases_it() {
    // a free lease, and a restore that a watcher observes through Redis
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = backup_and_target(harness, "lease-free").await;
    assert_eq!(lease_value(harness).await, None);
    let report = backup.parent().unwrap().join("report");
    let mut command = harness.thoradm();
    command
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(&backup)
        .arg("--report-dir")
        .arg(&report)
        .stdin(std::process::Stdio::null());
    let events = LeaseEvents::start(harness).await;
    let mut child = command.spawn().expect("thoradm runs");
    // sample the key until the restore ends, noting who holds it
    let mut holders = Vec::new();
    while child.try_wait().expect("the child is polled").is_none() {
        if let Some(value) = lease_value(harness).await {
            holders.push(value);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let events = events.finish().await;
    let status = child.wait().expect("the child ends");
    assert_eq!(status.code(), Some(0));
    // the lease was taken once for each of the two applies and given up after each, however
    // briefly it was held, never by the rival's name, and is released once the restore ends
    let count = |name: &str| events.iter().filter(|event| *event == name).count();
    assert_eq!(count("set"), 2, "{events:?}");
    assert_eq!(count("del"), 2, "{events:?}");
    assert!(holders.iter().all(|value| !value.starts_with(RIVAL)));
    assert_eq!(lease_value(harness).await, None);
    // nothing holds the lease, so a second restore of the same backup, merged into the data the
    // first left, isn't refused
    let again = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "restore",
            "--confirm",
            "--merge",
            "-b",
        ])
        .arg(&backup)
        .arg("--report-dir")
        .arg(backup.parent().unwrap().join("again"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("thoradm runs");
    assert_eq!(again.status.code(), Some(0), "{}", describe(&again));
    assert_eq!(lease_value(harness).await, None);
}

/// A restore whose Redis can't be reached refuses with a message naming Redis and the lease it
/// needs, before it writes anything
#[tokio::test]
async fn a_restore_refuses_when_redis_is_unreachable() {
    // a config whose Redis port nothing listens on
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = backup_and_target(harness, "lease-redis").await;
    let before = record::read(harness).await;
    let mut conf = harness.conf.clone();
    conf.redis.port = 1;
    let path = harness.scratch("lease-redis-conf").join("cluster.yml");
    write_cluster_conf(&conf, &path).expect("the config is written");
    let report = backup.parent().unwrap().join("report");
    let output = harness
        .thoradm_with(&path)
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(&backup)
        .arg("--report-dir")
        .arg(&report)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("thoradm runs");
    // the restore stops with a message about Redis, and the record is as it was
    assert_ne!(output.status.code(), Some(0), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Redis can't be reached"), "{stderr}");
    assert!(stderr.contains("127.0.0.1:1"), "{stderr}");
    assert!(stderr.contains("writer lease"), "{stderr}");
    assert_eq!(record::read(harness).await, before);
}

/// Restores into a target whose declared view `results_auth` is a table of the same name and checks
/// that the restore stops within a minute with an error naming the view instead of waiting for a
/// build that never starts, and that it gives the writer lease up
///
/// # Arguments
///
/// * `name` - The scratch directory's name
/// * `flags` - The flags to restore with besides the confirmation
async fn restore_into_table_held_view(name: &str, flags: &[&str]) {
    // a target whose declared view `results_auth` is a table of the same name
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = backup_and_target(harness, name).await;
    let scylla = harness.scylla().await;
    let namespace = harness.conf.thorium.namespace.clone();
    scylla
        .query_unpaged(
            format!("DROP MATERIALIZED VIEW IF EXISTS {namespace}.results_auth"),
            (),
        )
        .await
        .expect("the view is dropped");
    scylla
        .query_unpaged(
            format!("CREATE TABLE IF NOT EXISTS {namespace}.results_auth (id uuid PRIMARY KEY)"),
            (),
        )
        .await
        .expect("the table is created");
    // restore with output captured to files, so a hung restore can be killed without blocking
    let scratch = harness.scratch(&format!("{name}-out"));
    let stdout = std::fs::File::create(scratch.join("stdout")).expect("stdout file");
    let stderr = std::fs::File::create(scratch.join("stderr")).expect("stderr file");
    let report = backup.parent().unwrap().join("report");
    let mut child = harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--confirm"])
        .args(flags)
        .arg("-b")
        .arg(&backup)
        .arg("--report-dir")
        .arg(&report)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("thoradm runs");
    // wait up to a minute for the restore to end, killing it when it doesn't
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child is polled") {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(60) {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    // put the declared view back so later tests find the schema they expect
    scylla
        .query_unpaged(format!("DROP TABLE IF EXISTS {namespace}.results_auth"), ())
        .await
        .expect("the table is dropped");
    let stderr = std::fs::read_to_string(scratch.join("stderr")).expect("stderr reads");
    // the restore failed on its own, naming the view, and nothing holds the lease afterwards
    let status =
        status.unwrap_or_else(|| panic!("the restore was still running after 60s: {stderr}"));
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("results_auth"), "{stderr}");
    assert_eq!(lease_value(harness).await, None);
}

/// A default restore into a target where a table holds a declared view's name fails with the view
/// named and gives the lease up
#[tokio::test]
async fn a_failing_restore_returns_promptly_and_releases_the_lease() {
    // the default restore drops the views before loading, which fails on the table
    restore_into_table_held_view("lease-table-view", &[]).await;
}

/// A merge restore, which leaves views alone while loading, is refused by the closing schema apply
/// instead of waiting forever for the view, and gives the lease up
#[tokio::test]
async fn a_failing_merge_restore_returns_promptly_and_releases_the_lease() {
    // the merge restore's final apply meets the table
    restore_into_table_held_view("lease-table-view-merge", &["--merge"]).await;
}
