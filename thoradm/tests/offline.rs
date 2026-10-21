//! Tests of thoradm's test harness that need no backing services

#![feature(variant_count)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;
use clap::Parser;
use common::corrupt;
use common::faulty_sink::{FaultySink, SinkCall, SinkFault};
use common::harness;
use common::kill;
use common::mock_s3::{MockS3, S3Behavior, S3Rule, glob_matches, percent_decode};
use common::proxy::{FaultProxy, ProxyAction, ProxyRule};
use scylla::deserialize::DeserializationError;
use scylla::errors::{
    ConnectionPoolError, DbError, ExecutionError, MetadataError, NextPageError, NextRowError,
    PagerExecutionError, PeersMetadataError, RequestAttemptError, RequestError,
};
use std::fmt::Write;
use std::net::SocketAddr;
use std::os::unix::process::ExitStatusExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thoradm::args::{Args, BackupComponents, BackupSubCommands, SubCommands};
use thoradm::backup::failures::{
    ComponentCounts, FAILURES_FILE, FailureLog, FailurePolicy, FailureRecord, Operation,
    read_records,
};
use thoradm::backup::manifest::{
    self, ArchiveEntry, FORMAT_VERSION, InProgressMarker, Layout, Manifest, PrunedEntry,
    RangeCount, RedisEntry, S3Entry, SiblingEntry, TableEntry, classify,
};
use thoradm::backup::sink::{SinkFactory, TokioFs};
use thoradm::backup::version::VersionMismatch;
use thoradm::outcome::RunOutcome;
use thoradm::shared::retry::{RetryPolicy, Transient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// Starts an upstream that counts the bytes each connection sends until it closes
///
/// Returns the address and a receiver for the first connection's byte count.
async fn counting_upstream() -> (SocketAddr, oneshot::Receiver<usize>) {
    // bind an ephemeral port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    // count the first connection's bytes until it closes or fails
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut total = 0;
        let mut buf = vec![0_u8; 4096];
        while let Ok(read) = socket.read(&mut buf).await {
            if read == 0 {
                break;
            }
            total += read;
        }
        let _ = tx.send(total);
    });
    (addr, rx)
}

/// Starts an upstream that sends a fixed payload to every connection, then closes it
///
/// # Arguments
///
/// * `payload` - The bytes to send
async fn sending_upstream(payload: Vec<u8>) -> SocketAddr {
    // bind an ephemeral port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // send the payload to each connection
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let payload = payload.clone();
            tokio::spawn(async move {
                let _ = socket.write_all(&payload).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

/// Starts an upstream that echoes every byte back
async fn echo_upstream() -> SocketAddr {
    // bind an ephemeral port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // echo each connection until it closes
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = socket.into_split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    addr
}

/// How long a test waits on one socket read before failing instead of hanging
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads from a stream until it ends or fails, returning the bytes and whether it failed
///
/// # Arguments
///
/// * `stream` - The stream to drain
async fn drain(stream: &mut TcpStream) -> (Vec<u8>, bool) {
    // read until a clean end or an error, failing the test if a read never returns
    let mut out = Vec::new();
    let mut buf = vec![0_u8; 4096];
    loop {
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buf))
            .await
            .expect("the connection neither ended nor failed");
        match read {
            Ok(0) => return (out, false),
            Ok(read) => out.extend_from_slice(&buf[..read]),
            Err(_) => return (out, true),
        }
    }
}

/// Builds deterministic test bytes of a length
///
/// # Arguments
///
/// * `len` - How many bytes to build
fn payload(len: usize) -> Vec<u8> {
    // cycle through every byte value
    (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

/// A rule that fires once on every connection
///
/// # Arguments
///
/// * `after_bytes` - The byte count to fire at
/// * `upstream` - The direction to count
/// * `action` - What to do
fn rule(after_bytes: u64, upstream: bool, action: ProxyAction) -> ProxyRule {
    // fire on every connection with no limit
    ProxyRule {
        from_connection: None,
        after_bytes,
        upstream,
        action,
        times: None,
    }
}

/// With no rules the proxy forwards bytes unchanged in both directions
#[tokio::test]
async fn proxy_forwards_unchanged_without_rules() {
    // send a megabyte through the proxy to an echo server
    let proxy = FaultProxy::start(echo_upstream().await).await.unwrap();
    let mut client = TcpStream::connect(proxy.listen).await.unwrap();
    let data = payload(1_048_576);
    let (mut read, mut write) = client.split();
    let send = async {
        write.write_all(&data).await.unwrap();
        write.shutdown().await.unwrap();
    };
    let mut echoed = Vec::new();
    let receive = read.read_to_end(&mut echoed);
    let ((), received) = tokio::join!(send, receive);
    received.unwrap();
    // every byte came back unchanged over one proxied connection
    assert_eq!(echoed.len(), data.len());
    assert!(
        echoed == data,
        "the echoed bytes differ from the bytes sent"
    );
    assert_eq!(proxy.connections(), 1);
}

/// A reset rule on the upstream direction lets exactly the configured bytes through
#[tokio::test]
async fn proxy_resets_after_exact_upstream_bytes() {
    // reset after 100 bytes sent upstream
    let (upstream, counted) = counting_upstream().await;
    let proxy = FaultProxy::start(upstream).await.unwrap();
    proxy
        .set_rules(vec![rule(100, true, ProxyAction::Reset)])
        .await;
    // send 1000 bytes in one write
    let mut client = TcpStream::connect(proxy.listen).await.unwrap();
    client.write_all(&payload(1000)).await.unwrap();
    // the upstream saw exactly 100 bytes and the client saw a reset
    let counted = tokio::time::timeout(READ_TIMEOUT, counted)
        .await
        .expect("the upstream connection never closed");
    assert_eq!(counted.unwrap(), 100);
    let (_, failed) = drain(&mut client).await;
    assert!(failed, "the client should see a connection reset");
}

/// A reset rule on the client direction lets exactly the configured bytes through
#[tokio::test]
async fn proxy_resets_after_exact_downstream_bytes() {
    // reset after 300 bytes sent back to the client
    let proxy = FaultProxy::start(sending_upstream(payload(1000)).await)
        .await
        .unwrap();
    proxy
        .set_rules(vec![rule(300, false, ProxyAction::Reset)])
        .await;
    // the client gets exactly the first 300 bytes, then a reset
    let mut client = TcpStream::connect(proxy.listen).await.unwrap();
    let (received, failed) = drain(&mut client).await;
    assert_eq!(received, payload(300));
    assert!(failed, "the client should see a connection reset");
}

/// A drop rule closes the connection cleanly after the configured bytes
#[tokio::test]
async fn proxy_drops_cleanly_after_exact_bytes() {
    // drop after 50 bytes sent back to the client
    let proxy = FaultProxy::start(sending_upstream(payload(1000)).await)
        .await
        .unwrap();
    proxy
        .set_rules(vec![rule(50, false, ProxyAction::Drop)])
        .await;
    // the client gets exactly 50 bytes and a clean end
    let mut client = TcpStream::connect(proxy.listen).await.unwrap();
    let (received, failed) = drain(&mut client).await;
    assert_eq!(received, payload(50));
    assert!(!failed, "a dropped connection ends cleanly");
}

/// A stall rule holds the connection for its delay and then forwards everything
#[tokio::test]
async fn proxy_stalls_then_forwards_everything() {
    // stall for half a second after 10 bytes sent back to the client
    let proxy = FaultProxy::start(sending_upstream(payload(1000)).await)
        .await
        .unwrap();
    proxy
        .set_rules(vec![rule(
            10,
            false,
            ProxyAction::Stall(Duration::from_millis(500)),
        )])
        .await;
    let mut client = TcpStream::connect(proxy.listen).await.unwrap();
    let start = Instant::now();
    // the first 10 bytes arrive at once
    let mut first = vec![0_u8; 10];
    tokio::time::timeout(READ_TIMEOUT, client.read_exact(&mut first))
        .await
        .expect("the first bytes never arrived")
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(400));
    // the rest arrive after the stall, unchanged
    let (rest, failed) = drain(&mut client).await;
    assert!(start.elapsed() >= Duration::from_millis(500));
    assert!(!failed);
    first.extend(rest);
    assert_eq!(first, payload(1000));
}

/// A refuse rule refuses exactly the configured connections
#[tokio::test]
async fn proxy_refuses_from_the_configured_connection() {
    // refuse one connection, starting with the third
    let proxy = FaultProxy::start(sending_upstream(payload(10)).await)
        .await
        .unwrap();
    proxy
        .set_rules(vec![ProxyRule {
            from_connection: Some(2),
            after_bytes: 0,
            upstream: true,
            action: ProxyAction::Refuse,
            times: Some(1),
        }])
        .await;
    // connections 0 and 1 work, 2 is refused, and 3 works again once the rule is spent
    let mut results = Vec::new();
    for _ in 0..4 {
        let mut client = TcpStream::connect(proxy.listen).await.unwrap();
        results.push(drain(&mut client).await.0.len());
    }
    assert_eq!(results, vec![10, 10, 0, 10]);
    assert_eq!(proxy.connections(), 4);
}

/// A rule with a firing limit stops firing once the limit is reached
#[tokio::test]
async fn proxy_rule_times_limits_firings() {
    // reset the first connection only
    let proxy = FaultProxy::start(sending_upstream(payload(100)).await)
        .await
        .unwrap();
    let mut once = rule(0, false, ProxyAction::Reset);
    once.times = Some(1);
    proxy.set_rules(vec![once]).await;
    // the first connection gets nothing, the second gets everything
    let mut first = TcpStream::connect(proxy.listen).await.unwrap();
    let (received, failed) = drain(&mut first).await;
    assert!(received.is_empty() && failed);
    let mut second = TcpStream::connect(proxy.listen).await.unwrap();
    assert_eq!(drain(&mut second).await, (payload(100), false));
}

/// The requests a fake S3 upstream received, as method and path
type UpstreamLog = Arc<Mutex<Vec<(String, String)>>>;

/// Starts a fake S3 upstream that serves every GET with a fixed body and records requests
///
/// # Arguments
///
/// * `body` - The bytes each GET returns
async fn fake_s3_upstream(body: Vec<u8>) -> (String, UpstreamLog) {
    // record each request's method and path and answer it
    let log: UpstreamLog = Arc::default();
    let handler_log = log.clone();
    let app = Router::new().fallback(move |request: Request| {
        let log = handler_log.clone();
        let body = body.clone();
        async move {
            log.lock().unwrap().push((
                request.method().to_string(),
                request
                    .uri()
                    .path_and_query()
                    .map_or_else(String::new, ToString::to_string),
            ));
            Response::builder()
                .status(200)
                .header("content-type", "application/x-test")
                .header("x-amz-meta-thorium", "kept")
                .header("content-length", body.len())
                .body(Body::from(body))
                .unwrap()
        }
    });
    // serve it on an ephemeral port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://127.0.0.1:{}", addr.port()), log)
}

/// Builds a client that never goes through a proxy from the environment
fn direct_client() -> reqwest::Client {
    // the mock runs on loopback; an environment proxy would only get in the way
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// `MockS3` forwards requests unchanged when no rule matches, keeping status, headers, and body
#[tokio::test]
async fn mock_s3_forwards_by_default() {
    // put the mock in front of a fake upstream
    let (upstream, log) = fake_s3_upstream(payload(1000)).await;
    let mock = MockS3::start(&upstream).await.unwrap();
    // fetch an object through the mock
    let response = direct_client()
        .get(format!("{}/bucket/a/b.bin?versionId=1", mock.endpoint))
        .send()
        .await
        .unwrap();
    // the upstream's status, headers, and body come back unchanged
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/x-test");
    assert_eq!(response.headers()["x-amz-meta-thorium"], "kept");
    assert_eq!(response.bytes().await.unwrap().to_vec(), payload(1000));
    // the upstream saw the same path and query, and the mock recorded the request
    assert_eq!(
        log.lock().unwrap().clone(),
        vec![("GET".to_owned(), "/bucket/a/b.bin?versionId=1".to_owned())]
    );
    let seen = mock.seen().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (
            seen[0].bucket.as_str(),
            seen[0].key.as_str(),
            seen[0].status
        ),
        ("bucket", "a/b.bin", 200)
    );
}

/// A status rule applies to exactly the nth matching request and no other
#[tokio::test]
async fn mock_s3_status_rule_hits_only_the_nth_match() {
    // fail the second GET of keys under a/ with a 503
    let (upstream, log) = fake_s3_upstream(payload(10)).await;
    let mock = MockS3::start(&upstream).await.unwrap();
    mock.set_rules(vec![S3Rule {
        method: Method::GET,
        key: "a/*".to_owned(),
        query: None,
        nth: Some(2),
        behavior: S3Behavior::Status {
            status: 503,
            code: "SlowDown".to_owned(),
        },
    }])
    .await;
    // fetch a/1 three times and b/1 once
    let client = direct_client();
    let mut statuses = Vec::new();
    for path in ["a/1", "a/1", "a/1", "b/1"] {
        let response = client
            .get(format!("{}/bucket/{path}", mock.endpoint))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        // the injected error names its S3 code
        if status == 503 {
            assert!(text.contains("<Code>SlowDown</Code>"), "{text}");
        }
        statuses.push(status);
    }
    // only the second matching request failed, and it never reached the upstream
    assert_eq!(statuses, vec![200, 503, 200, 200]);
    assert_eq!(log.lock().unwrap().len(), 3);
}

/// A truncating rule cuts the body short while keeping its full length header
#[tokio::test]
async fn mock_s3_truncates_bodies() {
    // cut every GET body after 100 bytes
    let (upstream, _) = fake_s3_upstream(payload(10_000)).await;
    let mock = MockS3::start(&upstream).await.unwrap();
    mock.set_rules(vec![S3Rule {
        method: Method::GET,
        key: "*".to_owned(),
        query: None,
        nth: None,
        behavior: S3Behavior::TruncateBody(100),
    }])
    .await;
    // the response promises the full length but the body fails partway
    let response = direct_client()
        .get(format!("{}/bucket/key", mock.endpoint))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-length"], "10000");
    assert!(
        response.bytes().await.is_err(),
        "a short body must fail to read"
    );
}

/// Rules can match on a query parameter, and other requests pass untouched
#[tokio::test]
async fn mock_s3_matches_query_parameters() {
    // fail only part uploads with a 500
    let (upstream, _) = fake_s3_upstream(Vec::new()).await;
    let mock = MockS3::start(&upstream).await.unwrap();
    mock.set_rules(vec![S3Rule {
        method: Method::PUT,
        key: "*".to_owned(),
        query: Some("partNumber".to_owned()),
        nth: None,
        behavior: S3Behavior::Status {
            status: 500,
            code: "InternalError".to_owned(),
        },
    }])
    .await;
    // a part upload fails and a plain put passes
    let client = direct_client();
    let part = client
        .put(format!(
            "{}/bucket/key?partNumber=1&uploadId=x",
            mock.endpoint
        ))
        .body("data")
        .send()
        .await
        .unwrap();
    let plain = client
        .put(format!("{}/bucket/key", mock.endpoint))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(
        (part.status().as_u16(), plain.status().as_u16()),
        (500, 200)
    );
}

/// A virtual-hosted request is recorded with its host and forwarded in path style
#[tokio::test]
async fn mock_s3_rewrites_virtual_hosted_requests() {
    // send a request whose Host names the bucket
    let (upstream, log) = fake_s3_upstream(payload(5)).await;
    let mock = MockS3::start(&upstream).await.unwrap();
    let port = mock.endpoint.rsplit(':').next().unwrap().to_owned();
    let response = direct_client()
        .get(format!("{}/some%20key", mock.endpoint))
        .header("host", format!("files.localhost:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // the upstream got a path-style request and the mock recorded the addressing
    assert_eq!(log.lock().unwrap()[0].1, "/files/some%20key");
    let seen = mock.seen().await;
    assert_eq!(seen[0].host, format!("files.localhost:{port}"));
    assert_eq!(
        (seen[0].bucket.as_str(), seen[0].key.as_str()),
        ("files", "some key")
    );
}

/// The key globs and percent decoding `MockS3` matches with behave as documented
#[test]
fn mock_s3_globs_and_decoding() {
    // globs anchor at both ends and match runs of any length
    assert!(glob_matches("*", ""));
    assert!(glob_matches("*/4f2a*", "ab/4f2a99"));
    assert!(glob_matches("a*c*e", "abcde"));
    assert!(!glob_matches("a*c*e", "abcdef"));
    assert!(!glob_matches("abc", "abcd"));
    // escapes decode to their bytes, including multi-byte UTF-8, and bad escapes stay
    assert_eq!(percent_decode("a%20b%E2%9C%93%zz%2"), "a b\u{2713}%zz%2");
}

/// Builds a fresh scratch directory for an offline test
///
/// # Arguments
///
/// * `name` - The test's name
fn scratch(name: &str) -> std::path::PathBuf {
    // name the directory by test and process so parallel runs never share it
    let dir = std::env::temp_dir().join(format!("thoradm-offline-{name}-{}", std::process::id()));
    // start from an empty directory
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `FaultySink` fails writes with `ENOSPC` at exactly the configured byte
#[tokio::test]
async fn faulty_sink_runs_out_of_space_at_the_configured_byte() {
    // fail writes past 10 bytes
    let dir = scratch("enospc");
    let sink = FaultySink::new();
    sink.add_fault("*/data", SinkFault::NoSpaceAfter(10));
    let mut file = sink.create(&dir.join("data"), 0o600).await.unwrap();
    // the first write fits, the second fails after filling the space
    file.write_all(&[1; 6]).await.unwrap();
    let error = file.write_all(&[2; 6]).await.unwrap_err();
    assert_eq!(error.raw_os_error(), Some(28));
    file.flush().await.unwrap();
    // exactly 10 bytes landed
    assert_eq!(
        std::fs::read(dir.join("data")).unwrap(),
        [[1; 6].as_slice(), &[2; 4]].concat()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `FaultySink` makes exactly one short write that reports success
#[tokio::test]
async fn faulty_sink_makes_one_short_write() {
    // cut the next write to 3 bytes
    let dir = scratch("short");
    let sink = FaultySink::new();
    sink.add_fault("*", SinkFault::ShortWrite(3));
    let mut file = sink.create(&dir.join("data"), 0o600).await.unwrap();
    // the short write reports success; the next write is whole
    file.write_all(b"abcdefghij").await.unwrap();
    file.write_all(b"wxyz").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(std::fs::read(dir.join("data")).unwrap(), b"abcwxyz");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `FaultySink` fails syncs, renames, and opens on demand, leaving the files untouched
#[tokio::test]
async fn faulty_sink_fails_sync_rename_and_open_on_demand() {
    // fail syncs and renames of one file and opens of another
    let dir = scratch("fail");
    let sink = FaultySink::new();
    sink.add_fault("*/a", SinkFault::FailSync);
    sink.add_fault("*/a", SinkFault::FailRename);
    sink.add_fault("*/full", SinkFault::TooManyFiles);
    let mut file = sink.create(&dir.join("a"), 0o600).await.unwrap();
    file.write_all(b"x").await.unwrap();
    // the sync and rename fail with EIO and nothing moves
    assert_eq!(file.sync().await.unwrap_err().raw_os_error(), Some(5));
    let renamed = sink
        .rename(&dir.join("a"), &dir.join("b"))
        .await
        .unwrap_err();
    assert_eq!(renamed.raw_os_error(), Some(5));
    assert!(dir.join("a").exists() && !dir.join("b").exists());
    // opening the other file fails with EMFILE either way
    assert_eq!(
        sink.create(&dir.join("full"), 0o600)
            .await
            .err()
            .unwrap()
            .raw_os_error(),
        Some(24)
    );
    assert_eq!(
        sink.append(&dir.join("full"))
            .await
            .err()
            .unwrap()
            .raw_os_error(),
        Some(24)
    );
    // with the faults cleared the same calls succeed
    sink.clear_faults();
    file.sync().await.unwrap();
    sink.rename(&dir.join("a"), &dir.join("b")).await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `FaultySink` records every call in the order it was made
#[tokio::test]
async fn faulty_sink_records_calls_in_order() {
    // make one of each call
    let dir = scratch("record");
    let sink = FaultySink::new();
    let (a, b) = (dir.join("a"), dir.join("b"));
    let mut file = sink.create(&a, 0o640).await.unwrap();
    file.write_all(b"hello").await.unwrap();
    file.flush().await.unwrap();
    file.sync().await.unwrap();
    drop(file);
    sink.rename(&a, &b).await.unwrap();
    sink.sync_dir(&dir).await.unwrap();
    let mut log = sink.append(&b).await.unwrap();
    log.write_all(b"!").await.unwrap();
    log.sync().await.unwrap();
    sink.remove(&b).await.unwrap();
    // the record lists them in order, with paths, modes, and byte counts
    assert_eq!(
        sink.calls(),
        vec![
            SinkCall::Create(a.clone(), 0o640),
            SinkCall::Write(a.clone(), 5),
            SinkCall::Flush(a.clone()),
            SinkCall::Sync(a.clone()),
            SinkCall::Rename(a, b.clone()),
            SinkCall::SyncDir(dir.clone()),
            SinkCall::Append(b.clone()),
            SinkCall::Write(b.clone(), 1),
            SinkCall::Sync(b.clone()),
            SinkCall::Remove(b),
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The kill helper stops a child as soon as it logs the chosen event
#[tokio::test]
async fn kill_helper_stops_the_child_at_the_event() {
    // a child that logs two events and then would run for a long time
    let mut command = std::process::Command::new("sh");
    command.args([
        "-c",
        "echo started >&2; echo checkpoint >&2; sleep 30; echo finished >&2",
    ]);
    let start = Instant::now();
    let outcome = kill::run_until(command, "checkpoint").await.unwrap();
    // the child was killed by SIGKILL right after the event and never finished
    assert!(outcome.killed);
    assert_eq!(outcome.status.signal(), Some(9));
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(outcome.stderr, vec!["started", "checkpoint"]);
}

/// The kill helper lets a child finish when the event never comes
#[tokio::test]
async fn kill_helper_lets_a_child_finish_without_the_event() {
    // a child that exits on its own
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "echo done >&2; exit 3"]);
    let outcome = kill::run_until(command, "never logged").await.unwrap();
    // it ran to completion with its own exit code
    assert!(!outcome.killed);
    assert_eq!(outcome.status.code(), Some(3));
}

/// The corruption helpers damage exactly what they name
#[test]
fn corruption_helpers_damage_what_they_name() {
    // flip one byte
    let dir = scratch("corrupt");
    let file = dir.join("archive");
    std::fs::write(&file, [0_u8, 1, 2, 3]).unwrap();
    corrupt::flip_byte(&file, 2).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), [0, 1, 0xfd, 3]);
    assert!(corrupt::flip_byte(&file, 4).is_err());
    // truncate by two bytes
    corrupt::truncate_by(&file, 2).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), [0, 1]);
    // add a file in a new directory and delete the original
    let extra = dir.join("maps").join("extra.thoriummap");
    corrupt::add_file(&extra, b"junk").unwrap();
    assert_eq!(std::fs::read(&extra).unwrap(), b"junk");
    corrupt::delete(&file).unwrap();
    assert!(!file.exists());
    // edit a field of a JSON document
    let manifest = dir.join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{"format_version":1,"tables":{"tags":{"rows":3}}}"#,
    )
    .unwrap();
    corrupt::edit_json(&manifest, "/tables/tags/rows", serde_json::json!(4)).unwrap();
    let edited: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(edited["tables"]["tags"]["rows"], 4);
    assert_eq!(edited["format_version"], 1);
    assert!(corrupt::edit_json(&manifest, "/missing", serde_json::json!(0)).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The guards refuse any keyspace or bucket that isn't a test one
#[test]
fn guards_refuse_non_test_names() {
    // an isolated namespace passes
    let conf = harness::namespace_conf("guard_check", 1);
    harness::guard(&conf).unwrap();
    assert_eq!(conf.thorium.namespace, "testing_guard_check");
    assert_eq!(conf.thorium.graphics.bucket, "testing-guard-check-graphics");
    assert_eq!(
        conf.thorium.reaction_cache.bucket,
        "testing-guard-check-reaction-cache"
    );
    // a production keyspace is refused, naming it
    let mut keyspace = conf.clone();
    keyspace.thorium.namespace = "thorium".to_owned();
    assert!(
        harness::guard(&keyspace)
            .unwrap_err()
            .contains("\"thorium\"")
    );
    // a test keyspace no loaded config could name, such as one with an uppercase letter, is
    // refused with the namespace rule
    let mut uppercase = conf.clone();
    uppercase.thorium.namespace = "testing_Guard_check".to_owned();
    let error = harness::guard(&uppercase).unwrap_err();
    assert!(
        error.contains("\"testing_Guard_check\"") && error.contains("lowercase"),
        "{error}"
    );
    // a production bucket is refused, naming it and its role
    let mut bucket = conf;
    bucket.thorium.graphics.bucket = "thorium-graphics".to_owned();
    let error = harness::guard(&bucket).unwrap_err();
    assert!(
        error.contains("thorium-graphics") && error.contains("graphics"),
        "{error}"
    );
}

/// The readiness wait names the backend that never came up
#[tokio::test]
async fn readiness_names_the_backend_that_is_down() {
    // point Scylla at a port nothing listens on
    let mut conf = harness::namespace_conf("ready_check", 1);
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    conf.scylla.nodes = vec![closed.to_string()];
    // the wait gives up after its deadline and names scylla
    let error = harness::wait_ready(&conf, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(error.starts_with("scylla never became ready"), "{error}");
}

/// Builds a failure record naming every optional field, with a newline in its error
fn full_record() -> FailureRecord {
    // name a download of one object in one token range
    FailureRecord::failed(
        "s3-ids-objects",
        Operation::Download,
        "body ended early\nafter 1048576 of 2101248 bytes",
    )
    .table("s3_ids")
    .bucket("testing-files")
    .token_range((i64::MIN, -9_100_000_000_000_000_000))
    .key("4f2a/4f2a")
    .object("4f2a")
    .attempts(5)
}

/// A failure record is one JSON line holding every documented field, even with a newline in
/// its error
#[tokio::test]
async fn failure_record_is_one_json_line_with_every_field() {
    // write one full record through a failure log
    let dir = scratch("record-line");
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    log.record(&full_record()).await.unwrap();
    // the file holds exactly one line
    let text = std::fs::read_to_string(dir.join(FAILURES_FILE)).unwrap();
    assert_eq!(text.matches('\n').count(), 1);
    assert!(text.ends_with('\n'));
    // the line has every documented field with its documented form
    let line: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    let mut fields: Vec<&str> = line
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec![
            "at",
            "attempts",
            "bucket",
            "component",
            "error",
            "key",
            "kind",
            "object",
            "operation",
            "table",
            "token_range"
        ]
    );
    assert_eq!(line["kind"], "failed");
    assert_eq!(line["operation"], "download");
    assert_eq!(
        line["token_range"],
        serde_json::json!([i64::MIN, -9_100_000_000_000_000_000_i64])
    );
    assert_eq!(
        line["error"],
        "body ended early\nafter 1048576 of 2101248 bytes"
    );
    assert!(line["at"].as_str().unwrap().ends_with('Z'));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The failure log appends, flushes, and syncs every line, and reads back in order
#[tokio::test]
async fn failure_log_syncs_every_line_and_reads_back_in_order() {
    // write three records through a recording file factory
    let dir = scratch("record-sync");
    let sink = FaultySink::new();
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &sink)
        .await
        .unwrap();
    let records = vec![
        full_record(),
        FailureRecord::skipped("notifications", "unknown variant `Mystery`")
            .table("notifications")
            .token_range((0, 10))
            .key("Ban:abc"),
        FailureRecord::failed("redis", Operation::Redis, "502 Bad Gateway"),
    ];
    for record in &records {
        log.record(record).await.unwrap();
    }
    // the file was opened once for appending, its directory was synced so the new entry is
    // durable, and each line was written and then synced
    let path = dir.join(FAILURES_FILE);
    let calls = sink.calls();
    assert_eq!(calls[0], SinkCall::Append(path.clone()));
    assert_eq!(calls[1], SinkCall::SyncDir(dir.clone()), "{calls:?}");
    let syncs = calls
        .iter()
        .filter(|call| **call == SinkCall::Sync(path.clone()))
        .count();
    assert_eq!(syncs, 3);
    for pair in calls[2..].chunks(2) {
        assert!(matches!(pair[0], SinkCall::Write(_, _)), "{calls:?}");
        assert_eq!(pair[1], SinkCall::Sync(path.clone()));
    }
    // a second log on the same directory appends rather than truncating
    let again = FailureLog::open(&dir, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    again.record(&records[0]).await.unwrap();
    // reading the file back gives the same records in order
    let mut expected = records.clone();
    expected.push(records[0].clone());
    assert_eq!(read_records(&path).unwrap(), expected);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A failed sync of the failures file is an error, never a silently lost record
#[tokio::test]
async fn failure_log_reports_a_failed_sync() {
    // fail every sync of the failures file
    let dir = scratch("record-fail");
    let sink = FaultySink::new();
    sink.add_fault("*failures.jsonl", SinkFault::FailSync);
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &sink)
        .await
        .unwrap();
    // recording returns the error and counts nothing
    assert!(log.record(&full_record()).await.is_err());
    assert_eq!(log.total(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A record an earlier run left half written is cut off when the log reopens, so the next
/// run's records read back
#[tokio::test]
async fn a_torn_last_record_is_dropped_when_the_log_reopens() {
    // a run that crashed part way through writing its second record
    let dir = scratch("record-torn-reopen");
    let path = dir.join(FAILURES_FILE);
    let first = FailureRecord::failed("samples-list", Operation::ReadRange, "first");
    let torn = FailureRecord::failed("samples-list", Operation::ReadRange, "torn");
    let mut text = serde_json::to_vec(&first).unwrap();
    text.push(b'\n');
    let torn_line = serde_json::to_vec(&torn).unwrap();
    text.extend_from_slice(&torn_line[..torn_line.len() / 2]);
    std::fs::write(&path, &text).unwrap();
    // the next run appends its own record
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    let second = FailureRecord::failed("tags", Operation::ReadRange, "second");
    log.record(&second).await.unwrap();
    // both whole records read back and the partial one is gone
    assert_eq!(read_records(&path).unwrap(), vec![first, second]);
    // a file holding only part of one record is emptied the same way
    std::fs::write(&path, &torn_line[..10]).unwrap();
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    log.record(&torn).await.unwrap();
    assert_eq!(read_records(&path).unwrap(), vec![torn]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Reopening a failures file whose whole last record lacks only its newline keeps that record
/// and ends it with a newline
#[tokio::test]
async fn an_unterminated_whole_record_is_kept_when_the_log_reopens() {
    // a run that stopped after writing its last record but before the newline
    let dir = scratch("record-unterminated-reopen");
    let path = dir.join(FAILURES_FILE);
    let first = FailureRecord::failed("samples-list", Operation::ReadRange, "first");
    let whole = FailureRecord::failed("tags", Operation::ReadRange, "whole but unterminated");
    let mut text = serde_json::to_vec(&first).unwrap();
    text.push(b'\n');
    text.extend_from_slice(&serde_json::to_vec(&whole).unwrap());
    std::fs::write(&path, &text).unwrap();
    // the next run appends its own record after it
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    let next = FailureRecord::failed("tags", Operation::ReadRange, "next");
    log.record(&next).await.unwrap();
    assert_eq!(read_records(&path).unwrap(), vec![first, whole, next]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Reading a failures file whose run crashed while writing its last record skips that partial
/// record and returns every whole one
#[test]
fn reading_skips_a_torn_last_record() {
    // one whole record and part of another
    let dir = scratch("record-torn-read");
    let path = dir.join(FAILURES_FILE);
    let whole = FailureRecord::failed("samples-list", Operation::ReadRange, "whole");
    let mut text = serde_json::to_vec(&whole).unwrap();
    text.push(b'\n');
    let last = full_record();
    let torn = serde_json::to_vec(&last).unwrap();
    text.extend_from_slice(&torn[..torn.len() / 2]);
    std::fs::write(&path, &text).unwrap();
    // the whole record reads back
    assert_eq!(read_records(&path).unwrap(), vec![whole.clone()]);
    // a whole last record without its newline still counts
    let mut text = serde_json::to_vec(&whole).unwrap();
    text.push(b'\n');
    text.extend_from_slice(&torn);
    std::fs::write(&path, &text).unwrap();
    assert_eq!(read_records(&path).unwrap(), vec![whole, last]);
    // a bad line before the last one is still an error
    std::fs::write(&path, b"{not json}\n{}\n").unwrap();
    assert!(read_records(&path).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// After a write of the failures file fails part way, the log refuses further records instead
/// of appending them to the partial line
#[tokio::test]
async fn a_failed_write_stops_further_records() {
    // run out of space part way through the first record
    let dir = scratch("record-torn-write");
    let sink = FaultySink::new();
    sink.add_fault("*failures.jsonl", SinkFault::NoSpaceAfter(10));
    let log = FailureLog::open(&dir, FailurePolicy::Continue, &sink)
        .await
        .unwrap();
    assert!(log.record(&full_record()).await.is_err());
    // even with space again, nothing more is written after the partial line
    sink.clear_faults();
    let writes = |calls: &[SinkCall]| {
        calls
            .iter()
            .filter(|call| matches!(call, SinkCall::Write(..)))
            .count()
    };
    let before = writes(&sink.calls());
    assert!(log.record(&full_record()).await.is_err());
    assert_eq!(writes(&sink.calls()), before);
    assert_eq!(log.total(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Run outcomes map to their documented exit codes
#[test]
fn run_outcomes_map_to_exit_codes() {
    // complete is 0, recorded failures 2, and an abort 1
    assert_eq!(RunOutcome::Complete.code(), 0);
    assert_eq!(RunOutcome::CompletedWithFailures.code(), 2);
    assert_eq!(RunOutcome::Aborted.code(), 1);
    assert_eq!(
        std::process::ExitCode::from(RunOutcome::Aborted),
        std::process::ExitCode::from(1)
    );
}

/// Under the default policy no number of records stops a run, and it finishes with exit 2
#[tokio::test]
async fn default_policy_never_stops_and_finishes_with_failures() {
    // a log with nothing recorded is complete
    let dir = scratch("policy-continue");
    let log = FailureLog::open(&dir, FailurePolicy::from_flag(false), &TokioFs)
        .await
        .unwrap();
    assert_eq!(RunOutcome::finished(&log), RunOutcome::Complete);
    // a hundred failed and skipped records never raise the stop signal
    for i in 0..100 {
        let record = if i % 2 == 0 {
            FailureRecord::failed("tags", Operation::ReadRange, "timeout")
        } else {
            FailureRecord::skipped("tags", "bad row")
        };
        log.record(&record).await.unwrap();
        assert!(!log.stopped() && !log.stop_signal().is_cancelled());
    }
    // the finished run maps to exit 2 and counts both kinds by component
    assert_eq!(
        RunOutcome::finished(&log),
        RunOutcome::CompletedWithFailures
    );
    assert_eq!(log.total(), 100);
    assert_eq!(
        log.counts("tags"),
        ComponentCounts {
            handled: 0,
            skipped: 50,
            failed: 50
        }
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Under `--abort-on-failure` the first record, failed or skipped, stops the run with exit 1
#[tokio::test]
async fn abort_policy_stops_at_the_first_record_of_either_kind() {
    // a skipped row is enough to stop the run
    for (name, record) in [
        (
            "abort-skip",
            FailureRecord::skipped("notifications", "unknown variant"),
        ),
        (
            "abort-fail",
            FailureRecord::failed("samples-list", Operation::WriteArchive, "EACCES"),
        ),
    ] {
        let dir = scratch(name);
        let log = FailureLog::open(&dir, FailurePolicy::from_flag(true), &TokioFs)
            .await
            .unwrap();
        let signal = log.stop_signal();
        assert!(!signal.is_cancelled());
        log.record(&record).await.unwrap();
        // the signal every worker holds is raised, and the run is an abort, never exit 2
        assert!(signal.is_cancelled() && log.stopped());
        assert_eq!(RunOutcome::finished(&log), RunOutcome::Aborted);
        // the record is durable before the stop
        assert_eq!(read_records(log.path()).unwrap(), vec![record]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// `--abort-on-failure` parses on new, restore, and scrub, and takes no value
#[test]
fn abort_on_failure_parses_as_a_plain_switch() {
    // the flag sets each command's switch
    let parse = |argv: &[&str]| Args::try_parse_from(argv.iter().copied());
    let new = parse(&["thoradm", "backup", "new", "--abort-on-failure"]).unwrap();
    let restore = parse(&[
        "thoradm",
        "backup",
        "restore",
        "-b",
        "x",
        "--abort-on-failure",
    ])
    .unwrap();
    let scrub = parse(&[
        "thoradm",
        "backup",
        "scrub",
        "-b",
        "x",
        "--abort-on-failure",
    ])
    .unwrap();
    assert!(
        matches!(new.cmd, SubCommands::Backup(BackupSubCommands::New(ref cmd)) if cmd.abort_on_failure)
    );
    assert!(
        matches!(restore.cmd, SubCommands::Backup(BackupSubCommands::Restore(ref cmd)) if cmd.abort_on_failure)
    );
    assert!(
        matches!(scrub.cmd, SubCommands::Backup(BackupSubCommands::Scrub(ref cmd)) if cmd.abort_on_failure)
    );
    // without it the switch is off
    let plain = parse(&["thoradm", "backup", "new"]).unwrap();
    assert!(
        matches!(plain.cmd, SubCommands::Backup(BackupSubCommands::New(ref cmd)) if !cmd.abort_on_failure)
    );
    // a value is a usage error, since there is no count or threshold
    assert!(parse(&["thoradm", "backup", "new", "--abort-on-failure=1"]).is_err());
}

/// Usage errors exit 1, so exit 2 always means recorded failures; help still exits 0
#[test]
fn usage_errors_exit_1_and_help_exits_0() {
    // an unknown flag is a usage error
    let unknown = std::process::Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args(["backup", "new", "--no-such-flag"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("--no-such-flag"));
    // a value for the abort switch is a usage error too
    let valued = std::process::Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args(["backup", "new", "--abort-on-failure=1"])
        .output()
        .unwrap();
    assert_eq!(valued.status.code(), Some(1));
    // help succeeds
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args(["backup", "new", "--help"])
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
}

/// Zero workers or a zero chunk multiplier is a usage error that names the flag, before any
/// config is read
#[test]
fn zero_workers_or_multiplier_is_a_usage_error() {
    for args in [
        &["--workers", "0", "backup", "new"][..],
        &["backup", "new", "--multiplier", "0"][..],
        &["backup", "new", "-m", "0"][..],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_thoradm"))
            .arg("--cluster-conf")
            .arg("/nonexistent/thorium.yml")
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        // clap refuses the value, so the run never reaches the config or a division by zero
        assert_eq!(output.status.code(), Some(1), "{args:?}:\n{stderr}");
        assert!(stderr.contains("invalid value '0'"), "{args:?}:\n{stderr}");
    }
}

/// Builds a manifest that sets every field, for the manifest tests
fn sample_manifest() -> Manifest {
    // one table, one S3 component, the Redis state, and a sibling
    let table = TableEntry {
        archives: vec![ArchiveEntry {
            uuid: uuid::Uuid::new_v4(),
            data_len: 4_096,
            entries: 3,
            rows: 12,
            map_sha256: "a".repeat(64),
        }],
        ranges: vec![
            RangeCount {
                start: i64::MIN,
                end: -1,
                rows: 5,
            },
            RangeCount {
                start: 0,
                end: i64::MAX,
                rows: 7,
            },
        ],
        rows: 12,
        skipped: 0,
        columns: vec!["group".to_owned(), "sha256".to_owned()],
    };
    let s3 = S3Entry {
        bucket_roles: vec!["files".to_owned(), "repos".to_owned()],
        objects: 2,
        bytes: 10,
        index: "s3_ids/objects.index.jsonl".to_owned(),
        index_sha256: "b".repeat(64),
    };
    Manifest {
        format_version: FORMAT_VERSION,
        thoradm_version: "1.8.1".to_owned(),
        thoradm_commit: Some("ae918c9".to_owned()),
        namespace: "testing_manifest".to_owned(),
        partition_sizes: [("files".to_owned(), 60_u32)].into_iter().collect(),
        buckets: [("files".to_owned(), "testing-files".to_owned())]
            .into_iter()
            .collect(),
        started: chrono::Utc::now(),
        finished: chrono::Utc::now(),
        components: vec![BackupComponents::S3Ids, BackupComponents::S3IdsObjects],
        failures: 0,
        skipped: 0,
        failures_sha256: None,
        tables: [("s3_ids".to_owned(), table)].into_iter().collect(),
        s3: [("s3-ids-objects".to_owned(), s3)].into_iter().collect(),
        redis: Some(RedisEntry {
            len: 3,
            sha256: "c".repeat(64),
            counts: [("users".to_owned(), 1_u64)].into_iter().collect(),
        }),
        siblings: vec![SiblingEntry {
            name: "schema.json".to_owned(),
            len: 2,
            sha256: "d".repeat(64),
        }],
        api_version_mismatch: Some(VersionMismatch {
            thoradm: "1.8.1".to_owned(),
            api: Some("1.8.0".to_owned()),
        }),
        pruned: Some(PrunedEntry {
            count: 2,
            sha256: "e".repeat(64),
        }),
    }
}

/// Builds a fresh scratch directory for an offline test
///
/// # Arguments
///
/// * `name` - The test's name for the directory
fn offline_scratch(name: &str) -> std::path::PathBuf {
    // name it by test and process so parallel runs never share it
    let dir = std::env::temp_dir().join(format!("thoradm-offline-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The manifest is written to a temporary file that is synced and renamed, the directory is
/// synced, and the in-progress marker is removed last
#[tokio::test]
async fn the_manifest_lands_through_a_synced_rename_and_the_marker_goes_last() {
    let dir = offline_scratch("manifest-order");
    let sink = FaultySink::new();
    InProgressMarker::new(chrono::Utc::now(), vec![BackupComponents::S3Ids])
        .write(&dir, &sink)
        .await
        .unwrap();
    let before = sink.calls().len();
    sample_manifest().write(&dir, &sink).await.unwrap();
    let calls: Vec<SinkCall> = sink.calls().split_off(before);
    let (tmp, manifest, marker) = (
        dir.join("manifest.json.tmp"),
        dir.join("manifest.json"),
        dir.join("backup-in-progress.json"),
    );
    // the order of the calls that matter
    let position = |call: &SinkCall| {
        calls
            .iter()
            .position(|seen| seen == call)
            .unwrap_or_else(|| panic!("no {call:?} in {calls:?}"))
    };
    let created = position(&SinkCall::Create(tmp.clone(), 0o600));
    let synced = position(&SinkCall::Sync(tmp.clone()));
    let renamed = position(&SinkCall::Rename(tmp.clone(), manifest.clone()));
    let dir_synced = position(&SinkCall::SyncDir(dir.clone()));
    let unmarked = position(&SinkCall::Remove(marker.clone()));
    assert!(
        created < synced && synced < renamed && renamed < dir_synced && dir_synced < unmarked,
        "{calls:?}"
    );
    assert_eq!(
        unmarked,
        calls.len() - 1,
        "the marker wasn't removed last: {calls:?}"
    );
    assert!(manifest.is_file() && !marker.exists() && !tmp.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A manifest whose write, sync, or rename fails leaves no manifest, no temporary file, and the
/// marker in place
#[tokio::test]
async fn a_manifest_that_cant_be_written_leaves_no_manifest() {
    for (name, fault) in [
        ("no-space", SinkFault::NoSpaceAfter(0)),
        ("no-sync", SinkFault::FailSync),
        ("no-rename", SinkFault::FailRename),
    ] {
        let dir = offline_scratch(&format!("manifest-{name}"));
        InProgressMarker::new(chrono::Utc::now(), vec![BackupComponents::S3Ids])
            .write(&dir, &TokioFs)
            .await
            .unwrap();
        let sink = FaultySink::new();
        sink.add_fault("*manifest.json*", fault);
        assert!(
            sample_manifest().write(&dir, &sink).await.is_err(),
            "{name}"
        );
        assert!(
            !dir.join("manifest.json").exists(),
            "{name}: a manifest exists"
        );
        assert!(
            !dir.join("manifest.json.tmp").exists(),
            "{name}: a temporary file exists"
        );
        assert!(
            dir.join("backup-in-progress.json").is_file(),
            "{name}: the marker is gone"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// A directory is empty, finished, interrupted, or legacy by what it holds
#[tokio::test]
async fn backup_directories_are_classified_by_what_they_hold() {
    let dir = offline_scratch("classify");
    // missing, and holding only empty directories, are both empty
    assert_eq!(classify(&dir.join("missing")).await.unwrap(), Layout::Empty);
    std::fs::create_dir_all(dir.join("empty").join("samples_list").join("data")).unwrap();
    assert_eq!(classify(&dir.join("empty")).await.unwrap(), Layout::Empty);
    // a marker is an interrupted backup, carrying what the marker says
    let marker = InProgressMarker::new(chrono::Utc::now(), vec![BackupComponents::Tags]);
    let interrupted = dir.join("interrupted");
    std::fs::create_dir_all(&interrupted).unwrap();
    marker.write(&interrupted, &TokioFs).await.unwrap();
    assert_eq!(
        classify(&interrupted).await.unwrap(),
        Layout::Interrupted(marker.clone())
    );
    // a manifest is a finished backup, with or without a marker beside it
    let finished = dir.join("finished");
    std::fs::create_dir_all(&finished).unwrap();
    marker.write(&finished, &TokioFs).await.unwrap();
    manifest::write_json_atomic(&finished, "manifest.json", &sample_manifest(), &TokioFs)
        .await
        .unwrap();
    assert_eq!(classify(&finished).await.unwrap(), Layout::Finished);
    // files and neither is legacy
    let legacy = dir.join("legacy");
    std::fs::create_dir_all(legacy.join("tags").join("maps")).unwrap();
    std::fs::write(
        legacy.join("tags").join("maps").join("a.thoriummap"),
        [0_u8; 96],
    )
    .unwrap();
    assert_eq!(classify(&legacy).await.unwrap(), Layout::Legacy);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A manifest of a format version newer than this thoradm's is refused, naming the version and
/// the thoradm that wrote it; one of this version reads back as written
#[tokio::test]
async fn a_newer_manifest_is_refused_and_a_current_one_reads_back() {
    let dir = offline_scratch("newer");
    // a manifest of this version reads back unchanged
    let current = sample_manifest();
    current.write(&dir, &TokioFs).await.unwrap();
    assert_eq!(Manifest::read(&dir).await.unwrap(), Some(current.clone()));
    // one version newer is refused before the rest is read
    let mut newer = serde_json::to_value(&current).unwrap();
    newer["format_version"] = serde_json::json!(FORMAT_VERSION + 1);
    newer["thoradm_version"] = serde_json::json!("9.9.9");
    newer["a_field_this_thoradm_doesnt_know"] = serde_json::json!(true);
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&newer).unwrap(),
    )
    .unwrap();
    let error = Manifest::read(&dir)
        .await
        .expect_err("a newer manifest was read")
        .to_string();
    assert!(
        error.contains(&format!("format version {}", FORMAT_VERSION + 1))
            && error.contains("9.9.9"),
        "{error}"
    );
    // a backup without a manifest has none to read
    assert_eq!(Manifest::read(&dir.join("missing")).await.unwrap(), None);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An object index whose last line was torn inside a multi-byte character reads its whole lines
/// and skips the torn one
#[tokio::test]
async fn an_index_torn_inside_a_character_keeps_its_whole_lines() {
    let dir = std::env::temp_dir().join(format!("thoradm-offline-torn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // one whole line, then a line cut inside the two bytes of an é
    let line = |key: &str| {
        serde_json::to_string(&manifest::ObjectIndexLine {
            bucket: "files".to_owned(),
            key: key.to_owned(),
            size: 10,
            etag: String::new(),
            sha256: "0".repeat(64),
        })
        .unwrap()
    };
    let whole = line("r\u{e9}sum\u{e9}-1.txt");
    let torn = line("r\u{e9}sum\u{e9}-2.txt");
    let cut = torn.find('\u{e9}').unwrap() + 1;
    let mut bytes = format!("{whole}\n").into_bytes();
    bytes.extend_from_slice(&torn.as_bytes()[..cut]);
    let path = dir.join("objects.index.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    // the whole line is read and the torn one skipped
    let index = manifest::read_index(&path)
        .await
        .expect("a torn line failed the read");
    let keys: Vec<&String> = index.keys().map(|(_, key)| key).collect();
    assert_eq!(keys, vec!["r\u{e9}sum\u{e9}-1.txt"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The environment variable naming the file the hashing child process hashes
const HASH_CHILD: &str = "THORADM_OFFLINE_HASH_CHILD";

/// Hashes the file the parent test names and prints the process's peak virtual memory; does
/// nothing unless run as the child of `hashing_a_file_holds_a_fixed_buffer_not_the_file`
#[tokio::test]
async fn hashing_child() {
    // only the parent test sets the file to hash
    let Ok(path) = std::env::var(HASH_CHILD) else {
        return;
    };
    let sha256 = manifest::file_sha256(std::path::Path::new(&path))
        .await
        .expect("the file couldn't be hashed");
    // report the hash and how much address space the process needed
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak = status
        .lines()
        .find_map(|line| line.strip_prefix("VmPeak:"))
        .unwrap()
        .trim()
        .trim_end_matches(" kB")
        .parse::<u64>()
        .unwrap();
    println!("sha256 {sha256} peak {peak}");
}

/// Runs the hashing child on a file under an address-space limit and returns its output, or
/// `None` when it failed
///
/// # Arguments
///
/// * `path` - The file to hash
/// * `limit` - The address-space limit in bytes, or `None` for no limit
fn run_hashing_child(path: &std::path::Path, limit: Option<u64>) -> Option<String> {
    use std::os::unix::process::CommandExt;
    // run this test binary again, filtered to the child test
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "hashing_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(HASH_CHILD, path)
        .env("MALLOC_ARENA_MAX", "2");
    if let Some(limit) = limit {
        // SAFETY: setrlimit only changes this child's limits between fork and exec
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &raw const limit) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let output = command.output().unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Hashing a file reads it through a fixed buffer: a process whose address space has room for
/// the buffer but not for the file still hashes it
#[test]
fn hashing_a_file_holds_a_fixed_buffer_not_the_file() {
    let dir = std::env::temp_dir().join(format!("thoradm-offline-hash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // a small file measures what the child needs without the file
    let small = dir.join("small");
    std::fs::write(&small, b"small").unwrap();
    let measured = run_hashing_child(&small, None).expect("the child failed with no limit");
    let peak_kb: u64 = measured
        .split_whitespace()
        .skip_while(|word| *word != "peak")
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    // a sparse 192 MiB file of zeros, with room for half of it on top of what the child needs
    let size: u64 = 192 * 1024 * 1024;
    let large = dir.join("large");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(size)
        .unwrap();
    let limit = peak_kb * 1024 + size / 2;
    let output = run_hashing_child(&large, Some(limit));
    std::fs::remove_dir_all(&dir).unwrap();
    // the child hashed the whole file within the limit
    let output = output.expect("hashing the file needed room for the whole file");
    let expected = data_encoding::HEXLOWER.encode(&{
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        let zeros = vec![0_u8; 1024 * 1024];
        for _ in 0..192 {
            hasher.update(&zeros);
        }
        hasher.finalize()
    });
    assert!(output.contains(&expected), "{output}");
}

/// A manifest round-trips through serde unchanged
#[test]
fn a_manifest_round_trips_through_serde() {
    // write a manifest that sets every field as JSON
    let manifest = sample_manifest();
    let text = serde_json::to_string_pretty(&manifest).unwrap();
    // and read the same manifest back
    assert_eq!(serde_json::from_str::<Manifest>(&text).unwrap(), manifest);
}

/// Archives one value the way a backup's archive holds it
///
/// # Arguments
///
/// * `value` - The value to archive
fn archived_bytes<T>(value: &T) -> String
where
    T: rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<256>>,
{
    // rkyv's own encoding, as lowercase hex
    data_encoding::HEXLOWER.encode(&rkyv::to_bytes::<_, 256>(value).unwrap())
}

/// The archived bytes of every variant of each enum stored inside backup archives, in
/// declaration order; a new variant adds a value at the end of its list
const GOLDEN_ENUM_BYTES: &[(&str, &[&str])] = &[
    ("TagType", &["00", "01", "02"]),
    (
        "NodeHealth",
        &[
            "00000000000000000000000000000000",
            "01000000000000000000000000000000",
            "6d61696e74656e616e63650002000000010000000b000000ecffffff",
            "03000000000000000000000000000000",
        ],
    ),
    (
        "EntityKinds",
        &[
            "00", "01", "02", "03", "04", "05", "06", "07", "08", "09", "0a", "0b", "0c", "0d",
            "0e", "0f",
        ],
    ),
    (
        "OutputDisplayType",
        &[
            "00", "01", "02", "03", "04", "05", "06", "07", "08", "09", "0a", "0b",
        ],
    ),
    ("NotificationType", &["00", "01"]),
    ("NotificationLevel", &["00", "01", "02"]),
    ("S3Objects", &["00", "01"]),
    (
        "AssociationKind",
        &[
            "00", "01", "02", "03", "04", "05", "06", "07", "08", "09", "0a", "0b", "0c", "0d",
            "0e", "0f", "10", "11", "12", "13", "14", "15", "16", "17", "18",
        ],
    ),
    ("Directionality", &["00", "01", "02"]),
    ("CommitishKinds", &["00", "01", "02"]),
];

/// Returns the archived bytes of every variant of each enum stored inside backup archives
fn archived_enum_bytes() -> Vec<(&'static str, Vec<String>)> {
    // one value per variant, from the harness's guarded lists
    use common::enums;
    vec![
        (
            "TagType",
            enums::tag_types().iter().map(archived_bytes).collect(),
        ),
        (
            "NodeHealth",
            enums::node_healths().iter().map(archived_bytes).collect(),
        ),
        (
            "EntityKinds",
            enums::entity_kinds().iter().map(archived_bytes).collect(),
        ),
        (
            "OutputDisplayType",
            enums::output_display_types()
                .iter()
                .map(archived_bytes)
                .collect(),
        ),
        (
            "NotificationType",
            enums::notification_types()
                .iter()
                .map(archived_bytes)
                .collect(),
        ),
        (
            "NotificationLevel",
            enums::notification_levels()
                .iter()
                .map(archived_bytes)
                .collect(),
        ),
        (
            "S3Objects",
            enums::s3_objects().iter().map(archived_bytes).collect(),
        ),
        (
            "AssociationKind",
            enums::association_kinds()
                .iter()
                .map(archived_bytes)
                .collect(),
        ),
        (
            "Directionality",
            enums::directionalities()
                .iter()
                .map(archived_bytes)
                .collect(),
        ),
        (
            "CommitishKinds",
            enums::commitish_kinds()
                .iter()
                .map(archived_bytes)
                .collect(),
        ),
    ]
}

/// Every variant of each enum stored inside backup archives archives to its committed bytes, so
/// a reordered or inserted variant, which would make old backups decode as other values, fails
#[test]
fn archived_enums_keep_their_golden_bytes() {
    let found = archived_enum_bytes();
    let golden: Vec<(&str, Vec<String>)> = GOLDEN_ENUM_BYTES
        .iter()
        .map(|(name, bytes)| (*name, bytes.iter().map(|hex| (*hex).to_owned()).collect()))
        .collect();
    // print the table as code, so a new variant's value can be reviewed and committed
    let table = found
        .iter()
        .fold(String::new(), |mut table, (name, bytes)| {
            let values: Vec<String> = bytes.iter().map(|hex| format!("\"{hex}\"")).collect();
            let _ = writeln!(table, "    (\"{name}\", &[{}]),", values.join(", "));
            table
        });
    assert_eq!(
        found, golden,
        "the archived enum bytes changed; found:\n{table}"
    );
}

/// An enum declared with explicit discriminants that disagree with its declaration order
#[derive(rkyv::Archive, rkyv::Serialize)]
#[repr(u8)]
enum PinnedOutOfOrder {
    /// Declared first, with the discriminant 1
    DocumentationFor = 1,
    /// Declared second, with the discriminant 0
    FileFor = 0,
}

/// Archived enums store each variant's position in the declaration, whatever discriminant it is
/// given, so only the order of the variants (which the golden bytes check) keeps old backups
/// readable
#[test]
fn archived_enums_store_positions_not_discriminants() {
    // the second variant archives as 01 and the first as 00, against their discriminants
    assert_eq!(archived_bytes(&PinnedOutOfOrder::FileFor), "01");
    assert_eq!(archived_bytes(&PinnedOutOfOrder::DocumentationFor), "00");
}

/// A copy of `AssociationKind` with its first two variants swapped
#[derive(rkyv::Archive, rkyv::Serialize)]
enum SwappedAssociationKind {
    /// Declared first here, second in the real enum
    DocumentationFor,
    /// Declared second here, first in the real enum
    FileFor,
}

/// An enum whose variants were reordered archives to other bytes than the golden ones, and its
/// archive decodes with the real type as another variant: the change the golden bytes catch
#[test]
fn a_reordered_enum_fails_the_golden_bytes() {
    // the reordered copy's FileFor is the real enum's second variant
    let swapped = archived_bytes(&SwappedAssociationKind::FileFor);
    let golden = GOLDEN_ENUM_BYTES
        .iter()
        .find(|(name, _)| *name == "AssociationKind")
        .map(|(_, bytes)| bytes[0])
        .expect("no golden bytes for AssociationKind");
    assert_ne!(
        swapped, golden,
        "a reordered variant archived to the golden bytes"
    );
    // the copy's first variant takes the golden bytes of the real enum's first variant instead
    assert_eq!(
        archived_bytes(&SwappedAssociationKind::DocumentationFor),
        golden
    );
    // decoded with the real type, the archive reads as the other variant
    let bytes = data_encoding::HEXLOWER.decode(swapped.as_bytes()).unwrap();
    let archived = rkyv::check_archived_root::<thorium::models::AssociationKind>(&bytes).unwrap();
    let decoded: thorium::models::AssociationKind =
        rkyv::Deserialize::deserialize(archived, &mut rkyv::Infallible).unwrap();
    assert_eq!(decoded, thorium::models::AssociationKind::DocumentationFor);
}

/// An error whose transience a test chooses, for driving the retry policy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flaky {
    /// Whether the error is worth retrying
    transient: bool,
}

impl Transient for Flaky {
    /// Returns the chosen transience
    fn is_transient(&self) -> bool {
        // the test decides
        self.transient
    }
}

/// Builds a Scylla execution error that failed on the server with a database error
///
/// # Arguments
///
/// * `error` - The database error the server returned
fn scylla_db_error(error: DbError) -> thoradm::Error {
    // wrap the database error the way the driver reports a failed last attempt
    thoradm::Error::ScyllaQuery(ExecutionError::LastAttemptError(
        RequestAttemptError::DbError(error, "from the test".to_owned()),
    ))
}

/// Builds an S3 error with a status and code
///
/// # Arguments
///
/// * `status` - The HTTP status, when the service answered
/// * `code` - The S3 error code, when the body named one
/// * `no_response` - Whether the request was sent and timed out or couldn't connect
fn s3_error(status: Option<u16>, code: Option<&str>, no_response: bool) -> thoradm::Error {
    // carry the status and code the way the S3 conversions do
    thoradm::Error::S3 {
        status,
        code: code.map(ToOwned::to_owned),
        message: None,
        no_response,
    }
}

/// The retry policy classifies each backend's errors as the retry table says
#[tokio::test]
async fn retry_policy_classifies_errors_by_backend() {
    // Scylla: timeouts, unavailable, overloaded, broken connections, and failed pages retry
    let unavailable = DbError::Unavailable {
        consistency: scylla::statement::Consistency::One,
        required: 1,
        alive: 0,
    };
    let transient_scylla = [
        thoradm::Error::ScyllaQuery(ExecutionError::RequestTimeout(Duration::from_secs(1))),
        thoradm::Error::ScyllaQuery(ExecutionError::ConnectionPoolError(
            ConnectionPoolError::Initializing,
        )),
        scylla_db_error(unavailable),
        scylla_db_error(DbError::Overloaded),
        thoradm::Error::ScyllaNextRow(NextRowError::NextPageError(NextPageError::RequestFailure(
            RequestError::RequestTimeout(Duration::from_secs(1)),
        ))),
        thoradm::Error::ScyllaPagedQuery(PagerExecutionError::MetadataError(
            MetadataError::ConnectionPoolError(ConnectionPoolError::Initializing),
        )),
    ];
    for error in &transient_scylla {
        assert!(error.is_transient(), "{error}");
    }
    // Scylla: syntax, invalid requests, authorization, undecodable rows, and bad cluster
    // metadata don't
    let permanent_scylla = [
        thoradm::Error::ScyllaPagedQuery(PagerExecutionError::MetadataError(MetadataError::Peers(
            PeersMetadataError::EmptyPeers,
        ))),
        scylla_db_error(DbError::SyntaxError),
        scylla_db_error(DbError::Invalid),
        scylla_db_error(DbError::Unauthorized),
        thoradm::Error::ScyllaNextRow(NextRowError::RowDeserializationError(
            DeserializationError::new(std::io::Error::other("bad enum")),
        )),
    ];
    for error in &permanent_scylla {
        assert!(!error.is_transient(), "{error}");
    }
    // S3: 5xx, throttling, timeouts, and failed connections retry; 403 (clock skew too), 404,
    // 400, and requests that couldn't be built don't
    for error in [
        s3_error(Some(500), Some("InternalError"), false),
        s3_error(Some(503), Some("SlowDown"), false),
        s3_error(None, Some("SlowDown"), false),
        s3_error(None, None, true),
    ] {
        assert!(error.is_transient(), "{error}");
    }
    for error in [
        s3_error(Some(403), Some("AccessDenied"), false),
        s3_error(Some(403), Some("RequestTimeTooSkewed"), false),
        s3_error(Some(404), Some("NoSuchKey"), false),
        s3_error(Some(404), Some("NoSuchBucket"), false),
        s3_error(Some(400), Some("InvalidRequest"), false),
        s3_error(None, None, false),
    ] {
        assert!(!error.is_transient(), "{error}");
    }
    // the API: 5xx and connection failures retry; 4xx don't
    let api = |code: u16| {
        thoradm::Error::Thorium(thorium::Error::Thorium {
            code: reqwest::StatusCode::from_u16(code).unwrap(),
            msg: None,
        })
    };
    assert!(api(502).is_transient() && api(503).is_transient());
    assert!(!api(400).is_transient() && !api(401).is_transient() && !api(404).is_transient());
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let refused = direct_client()
        .get(format!("http://{closed}/"))
        .send()
        .await
        .unwrap_err();
    assert!(thoradm::Error::Thorium(thorium::Error::Reqwest(refused)).is_transient());
    // the local disk: every error is recorded at once
    for errno in [28, 24, 13] {
        assert!(!thoradm::Error::IO(std::io::Error::from_raw_os_error(errno)).is_transient());
    }
}

/// A worker that ended early is one record naming the range or map it held, and the step
/// error its controller ends with isn't recorded a second time
#[test]
fn a_dead_worker_is_one_record_naming_what_it_held() {
    use std::path::PathBuf;
    use thoradm::shared::workers::WorkerFailure;
    // a backup worker that died reading a range names the range
    let reading = WorkerFailure {
        error: "worker panicked: bad page".to_owned(),
        order: Some((-10_i64, 10_i64)),
    };
    let record = FailureRecord::worker_ended(
        "samples-list",
        Operation::ReadRange,
        "samples_list",
        &reading,
    );
    assert_eq!(record.token_range, Some((-10, 10)));
    assert_eq!(record.operation, Operation::ReadRange);
    assert_eq!(record.table.as_deref(), Some("samples_list"));
    assert_eq!(record.error, "worker panicked: bad page");
    // a restore worker that died on a map names the map
    let map = PathBuf::from("/backup/tags/maps/0.thoriummap");
    let restoring = WorkerFailure {
        error: "insert failed".to_owned(),
        order: Some(map.clone()),
    };
    let record = FailureRecord::worker_ended("tags", Operation::Insert, "tags", &restoring);
    assert_eq!(
        record.key.as_deref(),
        Some("/backup/tags/maps/0.thoriummap")
    );
    assert_eq!(record.token_range, None);
    // a worker that held nothing names nothing
    let idle: WorkerFailure<PathBuf> = WorkerFailure {
        error: "final flush failed".to_owned(),
        order: None,
    };
    let record = FailureRecord::worker_ended("tags", Operation::Insert, "tags", &idle);
    assert_eq!((record.key, record.token_range), (None, None));
    // the step error its controller returns is not recorded again, while any other component
    // error is
    let ended = thoradm::Error::WorkersEnded("the tags backup stopped".to_owned());
    assert!(FailureRecord::component_failed("tags", Operation::WriteArchive, &ended).is_none());
    let other = thoradm::Error::new("failed to create the tags directory");
    let record = FailureRecord::component_failed("tags", Operation::WriteArchive, &other).unwrap();
    assert_eq!(record.operation, Operation::WriteArchive);
}

/// Transient failures are retried with growing, jittered delays until the attempts run out
#[tokio::test]
async fn retry_policy_retries_transient_errors_with_growing_jittered_delays() {
    // fail every attempt with a transient error and record the delays between attempts
    let policy = RetryPolicy::default();
    assert_eq!(policy.attempts, 5);
    assert_eq!(policy.base_delay, Duration::from_millis(200));
    let delays = Arc::new(Mutex::new(Vec::new()));
    let mut calls = 0;
    let retried = policy
        .run_with(
            || {
                calls += 1;
                async { Err::<(), _>(Flaky { transient: true }) }
            },
            |delay| {
                delays.lock().unwrap().push(delay);
                async {}
            },
        )
        .await;
    // every attempt was made and reported, and the last error is returned
    assert_eq!(retried.attempts, 5);
    assert_eq!(calls, 5);
    assert_eq!(retried.result, Err(Flaky { transient: true }));
    // each delay is jittered within a ceiling that doubles from the base delay
    let delays = delays.lock().unwrap().clone();
    assert_eq!(delays.len(), 4);
    for (retry, delay) in delays.iter().enumerate() {
        let ceiling = policy.delay_ceiling(u32::try_from(retry).unwrap() + 1);
        assert_eq!(
            ceiling,
            Duration::from_millis(200) * 2_u32.pow(u32::try_from(retry).unwrap())
        );
        assert!(*delay <= ceiling, "{delay:?} > {ceiling:?}");
    }
    // the ceiling never passes the longest delay
    assert_eq!(policy.delay_ceiling(30), policy.max_delay);
    // the delays are jittered rather than fixed
    let samples: Vec<Duration> = (0..50).map(|_| policy.delay(3)).collect();
    assert!(
        samples.iter().any(|delay| *delay != samples[0]),
        "delays are not jittered"
    );
}

/// Permanent failures stop at once, and a transient failure that clears succeeds
#[tokio::test]
async fn retry_policy_stops_on_permanent_errors_and_recovers_from_transient_ones() {
    // a permanent error is returned after one attempt with no delay
    let policy = RetryPolicy::default();
    let mut slept = 0;
    let permanent = policy
        .run_with(
            || async { Err::<(), _>(Flaky { transient: false }) },
            |_| {
                slept += 1;
                async {}
            },
        )
        .await;
    assert_eq!((permanent.attempts, slept), (1, 0));
    assert_eq!(permanent.result, Err(Flaky { transient: false }));
    // two transient failures and then a success take three attempts
    let mut calls = 0;
    let recovered = policy
        .run_with(
            || {
                calls += 1;
                let attempt = calls;
                async move {
                    if attempt < 3 {
                        Err(Flaky { transient: true })
                    } else {
                        Ok(attempt)
                    }
                }
            },
            |_| async {},
        )
        .await;
    assert_eq!(recovered.attempts, 3);
    assert_eq!(recovered.result, Ok(3));
    // a policy of one attempt never retries
    let once = RetryPolicy::new(1)
        .run_with(
            || async { Err::<(), _>(Flaky { transient: true }) },
            |_| async {},
        )
        .await;
    assert_eq!(once.attempts, 1);
}

/// Every guarded enum list holds each variant once at its guard's index, every `Backup` enum has a
/// list naming exactly the schema's variants, and a list missing a variant fails
#[test]
fn enum_lists_hold_every_variant_once() {
    // every list passes its checks against the variant count the compiler gives its enum
    let schema = common::schema::BackupSchema::load();
    let lists = common::enums::all();
    let counts = variant_counts();
    let count = |name: &str| {
        counts
            .iter()
            .find(|(listed, _)| *listed == name)
            .map_or_else(
                || panic!("{name} has no variant count"),
                |(_, count)| *count,
            )
    };
    for list in &lists {
        let problems = common::enums::list_problems(list, &schema, count(list.name));
        assert!(problems.is_empty(), "{problems:?}");
    }
    // ten archived enums, and a list for every enum the Backup schema has
    assert_eq!(lists.iter().filter(|list| list.cql.is_some()).count(), 10);
    for name in common::enums::backup_enums(&schema) {
        assert!(
            lists.iter().any(|list| list.in_backup && list.name == name),
            "the Backup enum {name} has no guarded list"
        );
    }
    // a copy missing its second variant fails, naming the enum
    for list in lists.iter().filter(|list| list.names.len() > 1) {
        let mut copy = common::enums::EnumList {
            name: list.name,
            in_backup: list.in_backup,
            names: list.names.clone(),
            indexes: list.indexes.clone(),
            cql: list.cql.clone(),
        };
        copy.names.remove(1);
        copy.indexes.remove(1);
        let problems = common::enums::list_problems(&copy, &schema, count(list.name));
        assert!(
            !problems.is_empty(),
            "{} missing a variant passed",
            list.name
        );
        assert!(
            problems
                .iter()
                .all(|problem| problem.starts_with(list.name)),
            "{problems:?}"
        );
    }
}

/// Checks that every value of an enum stored in Scylla as text parses back from that text into
/// a value stored as the same text
///
/// # Arguments
///
/// * `name` - The enum's name, for the failure message
/// * `values` - One value of every variant
/// * `stored` - The text the value is stored as
fn reads_back<T, F>(name: &str, values: &[T], stored: F) -> Vec<String>
where
    T: std::str::FromStr + std::fmt::Debug,
    F: Fn(&T) -> String,
{
    // a variant whose stored text parses to another variant, or to nothing, can't be read back
    values
        .iter()
        .filter_map(|value| {
            let text = stored(value);
            match T::from_str(&text) {
                Ok(parsed) if stored(&parsed) == text => None,
                Ok(parsed) => Some(format!("{name}: {text:?} reads back as {parsed:?}")),
                Err(_) => Some(format!("{name}: {text:?} doesn't read back as {value:?}")),
            }
        })
        .collect()
}

/// Every enum Scylla stores as text reads back as the variant it was written from, so no row
/// holding one is skipped by the API or by backup
#[test]
fn stored_enum_text_reads_back_as_its_variant() {
    // every enum whose Scylla column holds its as_str text
    use common::enums;
    let mut problems = Vec::new();
    problems.extend(reads_back("TagType", &enums::tag_types(), |v| {
        v.as_str().to_owned()
    }));
    problems.extend(reads_back(
        "OutputDisplayType",
        &enums::output_display_types(),
        |v| v.as_str().to_owned(),
    ));
    problems.extend(reads_back(
        "NotificationType",
        &enums::notification_types(),
        |v| v.as_str().to_owned(),
    ));
    problems.extend(reads_back(
        "NotificationLevel",
        &enums::notification_levels(),
        |v| v.as_str().to_owned(),
    ));
    problems.extend(reads_back("S3Objects", &enums::s3_objects(), |v| {
        v.as_str().to_owned()
    }));
    problems.extend(reads_back(
        "AssociationKind",
        &enums::association_kinds(),
        |v| v.as_str().to_owned(),
    ));
    problems.extend(reads_back(
        "Directionality",
        &enums::directionalities(),
        |v| v.as_str().to_owned(),
    ));
    problems.extend(reads_back(
        "CommitishKinds",
        &enums::commitish_kinds(),
        |v| v.as_str().to_owned(),
    ));
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Returns the variant count the compiler gives each enum with a guarded list
fn variant_counts() -> Vec<(&'static str, usize)> {
    // one entry per guarded list
    use std::mem::variant_count;
    use thorium::models::{
        ArgStrategy, AssociationKind, AutoTagLogic, CommitishKinds, DependencyPassStrategy,
        Directionality, EntityKinds, EventTrigger, FileNamingStrategy, HostPathTypes, ImageBanKind,
        ImageScaler, ImageVersion, KwargDependency, NodeHealth, NotificationLevel,
        NotificationType, OutputDisplayType, OutputHandler, PipelineBanKind, PruneAction,
        PrunedReferent, S3Objects, ScopedTokenRole, SpawnLimits, TagType, Theme, UserRole,
        VolumeTypes,
    };
    vec![
        ("TagType", variant_count::<TagType>()),
        ("NodeHealth", variant_count::<NodeHealth>()),
        ("EntityKinds", variant_count::<EntityKinds>()),
        ("OutputDisplayType", variant_count::<OutputDisplayType>()),
        ("NotificationType", variant_count::<NotificationType>()),
        ("NotificationLevel", variant_count::<NotificationLevel>()),
        ("S3Objects", variant_count::<S3Objects>()),
        ("AssociationKind", variant_count::<AssociationKind>()),
        ("Directionality", variant_count::<Directionality>()),
        ("CommitishKinds", variant_count::<CommitishKinds>()),
        ("UserRole", variant_count::<UserRole>()),
        ("ScopedTokenRole", variant_count::<ScopedTokenRole>()),
        ("Theme", variant_count::<Theme>()),
        ("PruneAction", variant_count::<PruneAction>()),
        ("PrunedReferent", variant_count::<PrunedReferent>()),
        ("ImageScaler", variant_count::<ImageScaler>()),
        (
            "DependencyPassStrategy",
            variant_count::<DependencyPassStrategy>(),
        ),
        ("FileNamingStrategy", variant_count::<FileNamingStrategy>()),
        ("ArgStrategy", variant_count::<ArgStrategy>()),
        ("KwargDependency", variant_count::<KwargDependency>()),
        ("ImageBanKind", variant_count::<ImageBanKind>()),
        ("PipelineBanKind", variant_count::<PipelineBanKind>()),
        ("EventTrigger", variant_count::<EventTrigger>()),
        ("VolumeTypes", variant_count::<VolumeTypes>()),
        ("HostPathTypes", variant_count::<HostPathTypes>()),
        ("OutputHandler", variant_count::<OutputHandler>()),
        ("AutoTagLogic", variant_count::<AutoTagLogic>()),
        ("SpawnLimits", variant_count::<SpawnLimits>()),
        ("ImageVersion", variant_count::<ImageVersion>()),
    ]
}

/// The coverage report marks every cell state, lists every backed-up entry and any known-broken
/// finding, and writes the same cells as Markdown and JSON
#[test]
fn coverage_report_shows_every_cell_state() {
    use common::classes::{CoverageMatrix, KNOWN_BROKEN, Location, MatrixSize, Need, ValueClass};
    // one cell in each state, one of them under a known-broken entry when any kind is broken
    let known = KNOWN_BROKEN.first();
    let broken = known.map_or("comments", |kind| kind.entries[0]);
    let mut matrix = CoverageMatrix::default();
    let covered = Location::column("comments", "text");
    matrix.require(covered.clone(), ValueClass::Unicode, Need::Required);
    matrix.observe(covered.clone(), ValueClass::Unicode);
    matrix.observe(covered.clone(), ValueClass::Unicode);
    matrix.require(covered.clone(), ValueClass::NearCellLimit, Need::FullOnly);
    matrix.require(
        Location::column("comments", "author"),
        ValueClass::Empty,
        Need::Required,
    );
    matrix.require(
        Location::column(broken, "kind"),
        ValueClass::Variant("unrepresentable Mystery".to_owned()),
        Need::Unrepresentable,
    );
    let markdown = common::classes::render_markdown(&matrix, MatrixSize::Ci);
    let json: serde_json::Value =
        serde_json::from_str(&common::classes::render_json(&matrix, MatrixSize::Ci)).unwrap();
    // the Markdown shows each state, and n/a where a class doesn't apply to a location
    assert!(
        markdown.contains("| `comments.text` | n/a | 2 | full only |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| `comments.author` | **MISSING** | n/a | n/a |"),
        "{markdown}"
    );
    assert!(markdown.contains("unrepresentable"), "{markdown}");
    if let Some(kind) = known {
        assert!(
            markdown.contains(&format!("Doesn't restore yet: {}", kind.finding)),
            "{markdown}"
        );
    }
    // every backed-up entry appears in both forms
    for (entry, _) in common::classes::backed_up_entries() {
        assert!(
            markdown.contains(&format!("`{entry}`"))
                || markdown.contains(&format!("<code>{entry}</code>")),
            "{entry} isn't in the Markdown report"
        );
        assert!(
            json["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|listed| listed["entry"] == entry),
            "{entry} isn't in the JSON report"
        );
    }
    // the JSON holds the same cells with the same states
    let cells = json["cells"].as_array().unwrap();
    assert_eq!(cells.len(), matrix.needs.len());
    let state = |location: &str, class: &str| {
        cells
            .iter()
            .find(|cell| cell["location"] == location && cell["class"] == class)
            .map(|cell| cell["state"].clone())
            .unwrap()
    };
    assert_eq!(
        state("comments.text", "Unicode"),
        serde_json::json!({"Covered": 2})
    );
    assert_eq!(state("comments.text", "NearCellLimit"), "FullOnly");
    assert_eq!(state("comments.author", "Empty"), "Missing");
    assert_eq!(
        state(
            &format!("{broken}.kind"),
            "Variant(unrepresentable Mystery)"
        ),
        "Unrepresentable"
    );
    let marked = cells.iter().find(|cell| cell["entry"] == broken).unwrap();
    assert_eq!(
        marked["known_broken"],
        known.map_or(serde_json::Value::Null, |kind| kind.finding.into())
    );
    // a written report holds exactly the rendered forms
    let dir = scratch("report");
    common::classes::write_report(&matrix, MatrixSize::Ci, &dir).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("coverage-matrix.md")).unwrap(),
        markdown
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("coverage-matrix.json")).unwrap())
            .unwrap();
    assert_eq!(written, json);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Builds a snapshot holding one Redis hash
///
/// # Arguments
///
/// * `key` - The hash's key without the namespace prefix
/// * `fields` - The hash's fields and their stored text
fn hash_snapshot(key: &str, fields: &[(&str, &str)]) -> common::oracle::Snapshot {
    // one key with no TTL
    use common::oracle::{RedisEntry, RedisValueRepr, Snapshot};
    let mut snapshot = Snapshot::default();
    let hash = fields
        .iter()
        .map(|(field, value)| ((*field).to_owned(), (*value).to_owned()))
        .collect();
    snapshot.redis.insert(
        key.to_owned(),
        RedisEntry {
            value: RedisValueRepr::Hash(hash),
            expires: false,
        },
    );
    snapshot
}

/// The fields of a user record written before its optional fields existed
const OLD_USER: &[(&str, &str)] = &[
    ("username", "alice"),
    ("email", "alice@example.invalid"),
    ("role", "\"User\""),
    ("token", "tok"),
    ("token_expiration", "\"2030-01-01T00:00:00Z\""),
    ("settings", "{\"theme\":\"Dark\"}"),
];

/// A key the source record lacks may come back only as the default it reads as
#[test]
fn an_absent_optional_key_equals_only_its_default() {
    use common::oracle::Oracle;
    let expected = hash_snapshot("user_data:alice", OLD_USER);
    // values the source never had, written into its optional fields, are each a difference
    let mut wrong = OLD_USER.to_vec();
    wrong.extend([
        ("image", "someone-elses-icon.png"),
        ("verification_token", "not-the-default"),
        ("password", "$argon2id$attacker"),
        ("unix", "{\"user\":0,\"group\":0}"),
    ]);
    let differences =
        Oracle::default().compare(&expected, &hash_snapshot("user_data:alice", &wrong));
    let mut fields: Vec<&str> = differences
        .iter()
        .filter_map(|difference| difference.location.rsplit(" field ").next())
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec!["image", "password", "unix", "verification_token"],
        "{differences:?}"
    );
    // the default an optional field reads as is the same value as no field
    let mut defaulted = OLD_USER.to_vec();
    defaulted.push(("unix", "null"));
    let differences =
        Oracle::default().compare(&expected, &hash_snapshot("user_data:alice", &defaulted));
    assert!(differences.is_empty(), "{differences:?}");
}

/// A nested key the source record lacks may come back only as the default it reads as
#[test]
fn an_absent_nested_optional_key_equals_only_its_default() {
    use common::oracle::Oracle;
    let expected = hash_snapshot(
        "user_data:bob",
        &[("username", "bob"), ("settings", "{\"theme\":\"Dark\"}")],
    );
    // a value the source never had inside an object field is a difference
    let wrong = hash_snapshot(
        "user_data:bob",
        &[
            ("username", "bob"),
            (
                "settings",
                "{\"theme\":\"Dark\",\"ai\":{\"not\":\"a valid AiSettings at all\"}}",
            ),
        ],
    );
    let differences = Oracle::default().compare(&expected, &wrong);
    assert_eq!(differences.len(), 1, "{differences:?}");
    assert!(
        differences[0].location.ends_with("field settings"),
        "{differences:?}"
    );
    // the nested default is the same value as no key
    let defaulted = hash_snapshot(
        "user_data:bob",
        &[
            ("username", "bob"),
            ("settings", "{\"theme\":\"Dark\",\"ai\":null}"),
        ],
    );
    let differences = Oracle::default().compare(&expected, &defaulted);
    assert!(differences.is_empty(), "{differences:?}");
}

/// One absent-key case: the key, its source fields, the field the found side adds, and how many
/// differences that makes
type AbsentCase<'a> = (&'a str, &'a [(&'a str, &'a str)], (&'a str, &'a str), usize);

/// A key the source record lacks reads only as the default the API's types give it, not as
/// the empty value of its type
#[test]
fn an_absent_key_reads_only_as_its_types_default() {
    use common::oracle::Oracle;
    // source hashes and the fields that differ from what they read as
    let group: &[(&str, &str)] = &[("name", "g")];
    let image: &[(&str, &str)] = &[("group", "g"), ("name", "img"), ("creator", "alice")];
    let context: &[(&str, &str)] = &[
        ("group", "g"),
        ("name", "img"),
        ("security_context", "{\"allow_privilege_escalation\":false}"),
    ];
    let locked = "{\"files\":false,\"repos\":false,\"tags\":false,\"images\":false,\
        \"pipelines\":false,\"reactions\":false,\"results\":false,\"comments\":false,\
        \"entities\":false,\"associations\":false}";
    let open = locked.replace("false", "true");
    let cases: &[AbsentCase] = &[
        // a group without `allowed` allows everything, so a locked-down group differs
        ("groups_data:g", group, ("allowed", locked), 1),
        ("groups_data:g", group, ("allowed", &open), 0),
        // an image without a timeout has none, so a zero timeout differs
        ("image_data:g:img", image, ("timeout", "0"), 1),
        // a security context without a user or group runs as neither, not as root
        (
            "image_data:g:img",
            context,
            (
                "security_context",
                "{\"user\":0,\"allow_privilege_escalation\":false}",
            ),
            1,
        ),
        (
            "image_data:g:img",
            context,
            (
                "security_context",
                "{\"group\":0,\"allow_privilege_escalation\":false}",
            ),
            1,
        ),
        // a user without a password has none, so an empty password differs
        ("user_data:alice", OLD_USER, ("password", ""), 1),
        ("user_data:alice", OLD_USER, ("password", "\"\""), 1),
        // a null nullable key is still the default
        ("user_data:alice", OLD_USER, ("unix", "null"), 0),
    ];
    // compare each source against itself with the one extra field
    let mut wrong = Vec::new();
    for (key, source, extra, expected) in cases {
        let mut found = source.to_vec();
        found.push(*extra);
        let differences =
            Oracle::default().compare(&hash_snapshot(key, source), &hash_snapshot(key, &found));
        if differences.len() != *expected {
            wrong.push(format!(
                "{key} {}={}: {} differences, expected {expected}",
                extra.0,
                extra.1,
                differences.len()
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Every key a record may leave out has a default derived from the API's own types
#[test]
fn every_optional_record_key_has_a_derived_default() {
    let missing = common::oracle::underivable_defaults();
    assert!(missing.is_empty(), "{missing:#?}");
}

/// A set of host paths stored in another order is the same settings value
#[test]
fn a_reordered_host_path_whitelist_is_the_same_set() {
    use common::oracle::Oracle;
    // the API stores a set, so two writes of one set can list its paths in any order
    let settings = |list: &str| {
        hash_snapshot(
            "system_settings",
            &[("reserved_cpu", "250"), ("host_path_whitelist", list)],
        )
    };
    let differences =
        Oracle::default().compare(&settings("[\"/a\",\"/b\"]"), &settings("[\"/b\",\"/a\"]"));
    assert!(differences.is_empty(), "{differences:?}");
    // a different set is still a difference
    let differences =
        Oracle::default().compare(&settings("[\"/a\",\"/b\"]"), &settings("[\"/a\",\"/c\"]"));
    assert_eq!(differences.len(), 1, "{differences:?}");
}

/// A near-cell-limit case never targets a column that keys its table or a view, and a near-limit
/// entry naming such a column is a problem
#[test]
fn near_limit_cases_never_target_key_columns() {
    use common::classes::{CoverageMatrix, Location, MatrixSize, NEAR_LIMIT_COLUMNS, ValueClass};
    use common::columns::{Column, CqlType, Kind, Table, key_sized_entries, require};
    // place each listed column in each kind of key, and once outside every key
    let lists = common::enums::all();
    let placements = [
        (Kind::Partition, false, true),
        (Kind::Clustering, false, true),
        (Kind::Regular, true, true),
        (Kind::Regular, false, false),
    ];
    for (table, column) in NEAR_LIMIT_COLUMNS {
        for (kind, view_key, keyed) in placements {
            let live = Table {
                name: (*table).to_owned(),
                columns: vec![Column {
                    name: (*column).to_owned(),
                    kind,
                    cql: CqlType::Text,
                    view_key,
                }],
            };
            let mut matrix = CoverageMatrix::default();
            require(&live, &lists, MatrixSize::Full, &mut matrix);
            // a key-sized column gets no near-limit case; any other listed column does
            let targeted = matrix.needs.keys().any(|(location, class)| {
                *location == Location::column(table, column) && *class == ValueClass::NearCellLimit
            });
            assert_eq!(
                targeted, !keyed,
                "{table}.{column} as {kind:?} (view key: {view_key})"
            );
            // the list entry is a problem exactly when the column is key-sized
            let problems = key_sized_entries(&live, NEAR_LIMIT_COLUMNS);
            assert_eq!(
                problems.len(),
                usize::from(keyed),
                "{table}.{column} as {kind:?} (view key: {view_key}): {problems:?}"
            );
        }
    }
}
