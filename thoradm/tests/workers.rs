//! Controllers whose only worker dies return promptly with the worker's error

mod common;

use common::corrupt;
use common::faulty_sink::{FaultySink, SinkFault};
use common::harness::Harness;
use common::names::Names;
use common::seed;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thoradm::args::{BackupComponents, CensusKinds, NewBackup, NewCensus, ScrubBackup};
use thoradm::backup::controllers::restore::RestoreController;
use thoradm::backup::controllers::{backup, scrub};
use thoradm::backup::failures::{FailureLog, FailurePolicy};
use thorium::CtlConf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// How long a controller may take to give up once its only worker has died
const PROMPT: Duration = Duration::from_secs(10);

/// A file factory whose failures file can never be written, so the first record a worker
/// makes ends that worker with an error
fn unrecordable() -> Arc<FaultySink> {
    // a full disk for the failures file only
    let sink = FaultySink::new();
    sink.add_fault("*failures.jsonl", SinkFault::NoSpaceAfter(0));
    Arc::new(sink)
}

/// Takes a backup of the samples list and leaves it with several maps and no data files
///
/// Every map the restore or scrub worker takes fails to open, and copies of the map make sure
/// orders remain queued after the first one kills the worker.
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `name` - The scratch directory's name
async fn backup_without_data(harness: &Harness, name: &str) -> PathBuf {
    // seed a sample and back up its table
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("janus");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "janus.bin", b"two faces, no data").await;
    let out = harness.scratch(name);
    let taken = harness
        .thoradm()
        .args(["backup", "new", "samples-list", "-o"])
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(
        taken.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&taken.stderr)
    );
    // copy each map a few times and delete every data file
    let table = out.join("samples_list");
    let maps: Vec<PathBuf> = list(&table.join("maps"));
    assert!(!maps.is_empty(), "the backup holds no maps");
    for map in &maps {
        let bytes = std::fs::read(map).unwrap();
        for copy in 0..3 {
            let name = format!("{}-{copy}.thoriummap", uuid::Uuid::new_v4());
            corrupt::add_file(&table.join("maps").join(name), &bytes).unwrap();
        }
    }
    for data in list(&table.join("data")) {
        corrupt::delete(&data).unwrap();
    }
    out
}

/// Lists the files in a directory
///
/// # Arguments
///
/// * `dir` - The directory to list
fn list(dir: &Path) -> Vec<PathBuf> {
    // read every entry's path
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

/// Asserts a run ended with an error naming its worker's failure, within the prompt deadline
///
/// # Arguments
///
/// * `started` - When the run started
/// * `result` - What the run returned
/// * `expected` - Text the error must contain
fn assert_prompt_failure<T: std::fmt::Debug>(
    started: Instant,
    result: Result<T, thoradm::Error>,
    expected: &str,
) {
    // the run gave up quickly and ended in an error, which the binary turns into exit 1
    assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
    let error = result
        .expect_err("the run should end in an error")
        .to_string();
    assert!(error.contains(expected), "{error}");
}

/// A backup whose only worker dies returns at once with the worker's error
#[tokio::test]
async fn backup_returns_when_its_only_worker_dies() {
    // add a notification row thoradm can't decode, so the worker must record a skip
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {}.notifications (kind, key, created, id, msg, level) \
                 VALUES ('Mystery', 'janus', toTimestamp(now()), uuid(), 'undecodable', 'Info')",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap();
    // back up notifications with one worker and an unwritable failures file
    let args = NewBackup {
        components: vec![BackupComponents::Notifications],
        output: harness.scratch("dead-backup-worker"),
        multiplier: 50,
        retries: 5,
        s3_concurrency: 64,
        abort_on_failure: false,
        allow_version_mismatch: false,
        prune_dangling: false,
        accept_broken_links: false,
    };
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let started = Instant::now();
    let run = Box::pin(backup::run(
        &harness.conf,
        ctl_conf,
        &scylla,
        1,
        &args,
        unrecordable(),
    ));
    let result = tokio::time::timeout(PROMPT, run)
        .await
        .expect("the backup hung");
    // the worker's failure to record its skip ended the run
    assert_prompt_failure(started, result, "No space left on device");
}

/// A restore whose only worker dies returns at once with the worker's error
///
/// The restore step runs through the controller, past the preflight, which would refuse a backup
/// whose data files are gone before any worker took it.
#[tokio::test]
async fn restore_returns_when_its_only_worker_dies() {
    // restore a backup whose data files are gone, with one worker and an unwritable failures file
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let backup = backup_without_data(harness, "dead-restore-worker").await;
    let reports = harness.scratch("dead-restore-worker-report");
    let log = Arc::new(
        FailureLog::open(&reports, FailurePolicy::Continue, unrecordable().as_ref())
            .await
            .unwrap(),
    );
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let scylla = harness.scylla().await;
    let mut controller = RestoreController::new(&harness.conf, ctl_conf, &scylla, 1);
    let started = Instant::now();
    let run = Box::pin(controller.restore(&backup, &log));
    let result = tokio::time::timeout(PROMPT, run)
        .await
        .expect("the restore hung");
    // the worker's failure to record the missing data file ended the run
    assert_prompt_failure(started, result, "No space left on device");
}

/// A scrub whose only worker dies returns at once with the worker's error
#[tokio::test]
async fn scrub_returns_when_its_only_worker_dies() {
    // scrub a backup whose data files are gone, with one worker and an unwritable failures file
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let backup = backup_without_data(harness, "dead-scrub-worker").await;
    let args = ScrubBackup {
        backup,
        abort_on_failure: false,
    };
    let started = Instant::now();
    let result = tokio::time::timeout(PROMPT, scrub::run(&args, 1, unrecordable()))
        .await
        .expect("the scrub hung");
    // the worker's failure to record the missing data file ended the run
    assert_prompt_failure(started, result, "No space left on device");
}

/// Starts a stand-in Redis that answers `PING` and refuses every other command, returning its
/// port
///
/// A census worker checks a pooled connection out with `PING` and then writes its counts, so
/// every write it makes fails with the refusal while connections stay healthy.
async fn refusing_redis() -> u16 {
    // listen on an ephemeral local port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // serve every connection the census opens until the test ends
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                // read the connection's commands a line at a time
                let (read, mut write) = stream.into_split();
                let mut lines = tokio::io::BufReader::new(read).lines();
                // each command is an array of bulk strings: `*N`, then `$len` and the text N times
                while let Ok(Some(header)) = lines.next_line().await {
                    // a line that doesn't start an array isn't a command
                    let Some(count) = header
                        .strip_prefix('*')
                        .and_then(|n| n.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    // skip each part's `$len` line and keep its text
                    let mut parts = Vec::with_capacity(count);
                    for _ in 0..count {
                        let _ = lines.next_line().await;
                        if let Ok(Some(part)) = lines.next_line().await {
                            parts.push(part);
                        }
                    }
                    // answer PING and refuse every other command
                    let reply: &[u8] = if parts
                        .first()
                        .is_some_and(|cmd| cmd.eq_ignore_ascii_case("PING"))
                    {
                        b"+PONG\r\n"
                    } else {
                        b"-ERR refused by the test\r\n"
                    };
                    // stop serving a connection the census closed
                    if write.write_all(reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// A census whose only worker can't write its counts to Redis returns at once with the error
///
/// A page that fails to arrive is retried rather than ending the worker, so the fault that ends
/// it is Redis refusing the worker's first write of counts.
#[tokio::test]
async fn census_returns_when_its_only_worker_dies() {
    // spread tag partitions over the ring, so the census's first ranges hold rows to count;
    // a range with nothing to count writes nothing
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let scylla = harness.scylla().await;
    let group = Names::new().group("ceres");
    let ns = &harness.conf.thorium.namespace;
    let insert = scylla
        .prepare(format!(
            "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, \
             key_lower, value_lower) VALUES ('Files', ?, 2026, ?, 'harvest', 'grain', \
             toTimestamp(now()), 'item', 'harvest', 'grain')"
        ))
        .await
        .unwrap();
    futures::future::join_all((0..1_000_i32).map(|bucket| {
        let (scylla, insert, group) = (&scylla, &insert, &group);
        async move {
            scylla
                .execute_unpaged(insert, (group.as_str(), bucket))
                .await
                .unwrap();
        }
    }))
    .await;
    // point a copy of the config at a Redis that refuses every write
    let mut conf = harness.conf.clone();
    "127.0.0.1".clone_into(&mut conf.redis.host);
    conf.redis.port = refusing_redis().await;
    conf.redis.username = None;
    conf.redis.password = None;
    // take a tag census with one worker that writes its counts
    let census = NewCensus {
        census_kinds: vec![CensusKinds::Tags],
        multiplier: 100,
        dry_run: false,
    };
    let started = Instant::now();
    let task = tokio::spawn(async move { thoradm::census::new(&census, &conf, 1).await });
    // the census gives up with the worker's error instead of waiting for its orders forever
    let result = tokio::time::timeout(PROMPT, task)
        .await
        .expect("the census hung")
        .expect("the census task panicked");
    assert_prompt_failure(started, result, "census");
}
