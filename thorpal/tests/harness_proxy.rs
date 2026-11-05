//! Runs the thorpal-harness binary with proxy env vars and checks that its model calls go through
//! the proxy, that `NO_PROXY` bypasses it, that the proxy settings it logs hide user info, and
//! that CA settings which load no certificates refuse the run

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[allow(dead_code)]
mod common {
    pub mod fixtures;
}

use common::fixtures::RunFixture;

/// The model endpoint of the fixture run, a host that never resolves
const MODEL_URL: &str = "https://model.invalid";

/// The user info in the proxy url, which must never be logged
const PROXY_PASSWORD: &str = "callisto-proxy-pass";

/// A local proxy that records the request line of every request and refuses each one
struct RecordingProxy {
    /// The proxy's url without user info
    address: String,
    /// The request lines received, in order
    lines: Arc<Mutex<Vec<String>>>,
}

impl RecordingProxy {
    /// Start a proxy on a free local port
    async fn start() -> Self {
        // listen on any free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&lines);
        // record each request's first line, then refuse the tunnel
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    // read until the end of the request head
                    let mut head = Vec::new();
                    let mut buf = [0_u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => head.extend_from_slice(&buf[..read]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head);
                    if let Some(line) = text.lines().next() {
                        recorded.lock().unwrap().push(line.to_owned());
                    }
                    // refuse the request so the harness fails fast
                    let _ = stream
                        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        RecordingProxy { address, lines }
    }

    /// Get the request lines received so far
    fn lines(&self) -> Vec<String> {
        // copy them out of the lock
        self.lines.lock().unwrap().clone()
    }
}

/// Run the harness on a fixture run with some env vars, in a clean environment
///
/// # Arguments
///
/// * `env` - The env vars to set on the harness only
async fn run_harness(env: Vec<(&'static str, String)>) -> (i32, String) {
    // drop the status file, which only the refusal tests read
    let (code, logs, _) = run_harness_with_status(env).await;
    (code, logs)
}

/// Run the harness on a fixture run with some env vars, in a clean environment, and read the
/// status file it wrote
///
/// # Arguments
///
/// * `env` - The env vars to set on the harness only
async fn run_harness_with_status(
    env: Vec<(&'static str, String)>,
) -> (i32, String, Option<serde_json::Value>) {
    // a fixture run whose model endpoint is the unresolvable host
    let fixture = RunFixture::new(MODEL_URL, "http://127.0.0.1:9");
    let dir = tempfile::tempdir().unwrap();
    let args = fixture.write(dir.path());
    // run the harness off the test's runtime with a short run timeout
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thorpal-harness"));
        command.env_clear().args(&args);
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        command.env("THORPAL_HARNESS_TIMEOUT_SECS", "3");
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{logs}");
        // read the status file before the run's directory is removed
        let status = std::fs::read(dir.path().join("result-files/status.json"))
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok());
        (output.status.code().unwrap_or(-1), logs, status)
    })
    .await
    .unwrap()
}

/// Model calls to an https endpoint tunnel through `HTTPS_PROXY`, and the harness logs the proxy
/// without its user info
#[tokio::test(flavor = "multi_thread")]
async fn model_calls_go_through_the_proxy() {
    // a proxy url carrying a user and password
    let proxy = RecordingProxy::start().await;
    let url = format!("http://europa:{PROXY_PASSWORD}@{}", proxy.address);
    let (code, logs) = run_harness(vec![
        ("HTTPS_PROXY", url),
        ("NO_PROXY", "127.0.0.1".to_owned()),
    ])
    .await;
    // the model call asked the proxy for a tunnel to the model's host, then failed there
    let lines = proxy.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("CONNECT model.invalid:443 ")),
        "{lines:?}"
    );
    assert_ne!(code, 0, "{logs}");
    // the proxy settings are logged without the user info
    let expected = format!("HTTPS_PROXY=http://{}", proxy.address);
    assert!(logs.contains(&expected), "{logs}");
    assert!(logs.contains("NO_PROXY=127.0.0.1"), "{logs}");
    assert!(
        logs.contains("CA certificates from the system store"),
        "{logs}"
    );
    assert!(!logs.contains(PROXY_PASSWORD), "{logs}");
    assert!(!logs.contains("europa"), "{logs}");
}

/// A model host listed in `NO_PROXY` is reached without the proxy
#[tokio::test(flavor = "multi_thread")]
async fn no_proxy_bypasses_the_proxy() {
    // the proxy is set, but the model host skips it
    let proxy = RecordingProxy::start().await;
    let (code, logs) = run_harness(vec![
        ("HTTPS_PROXY", format!("http://{}", proxy.address)),
        ("NO_PROXY", "model.invalid".to_owned()),
    ])
    .await;
    // the run failed to resolve the model host and never asked the proxy
    assert_ne!(code, 0, "{logs}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(proxy.lines().is_empty(), "{:?}", proxy.lines());
    assert!(logs.contains("NO_PROXY=model.invalid"), "{logs}");
}

/// A custom CA bundle that can't be read refuses the run at start, naming the bundle
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_ca_bundle_refuses_the_run() {
    // point the CA bundle at a file that doesn't exist
    let (code, logs) = run_harness(vec![(
        "SSL_CERT_FILE",
        "/nonexistent/thorpal-ca/ca.pem".to_owned(),
    )])
    .await;
    // the run is refused, and the bundle is named in the settings and the reason
    assert_eq!(code, 2, "{logs}");
    assert!(logs.contains("refused the run"), "{logs}");
    assert!(
        logs.contains("CA certificates from SSL_CERT_FILE=/nonexistent/thorpal-ca/ca.pem"),
        "{logs}"
    );
    assert!(logs.contains("can't be read"), "{logs}");
}

/// Run the harness with some CA env vars and check that it refuses the run with exit code 2, a
/// clear reason in the logs, and that reason in the status file
///
/// # Arguments
///
/// * `env` - The CA env vars to set on the harness
/// * `logged` - The CA settings the harness must log
async fn assert_ca_refusal(env: Vec<(&'static str, String)>, logged: &str) {
    // run the harness with only these CA settings
    let (code, logs, status) = run_harness_with_status(env).await;
    // the run is refused rather than panicking in the model client
    assert_eq!(code, 2, "{logs}");
    assert!(!logs.contains("panicked"), "{logs}");
    assert!(logs.contains("refused the run"), "{logs}");
    assert!(logs.contains("model client can't be built"), "{logs}");
    assert!(
        logs.contains(&format!("CA certificates from {logged}")),
        "{logs}"
    );
    // the status file records the refusal and its reason
    let status = status.unwrap_or_else(|| panic!("no status.json: {logs}"));
    assert_eq!(status["success"], false, "{status}");
    let reason = status["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("model client can't be built"), "{status}");
}

/// A CA bundle that reads but holds no certificates refuses the run
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_ca_bundle_refuses_the_run() {
    // /dev/null can be opened but loads no certificates
    assert_ca_refusal(
        vec![("SSL_CERT_FILE", "/dev/null".to_owned())],
        "SSL_CERT_FILE=/dev/null",
    )
    .await;
}

/// A CA bundle path that names a directory refuses the run
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_as_ca_bundle_refuses_the_run() {
    // a directory can be opened as a file but holds no bundle
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_owned();
    assert_ca_refusal(
        vec![("SSL_CERT_FILE", path.clone())],
        &format!("SSL_CERT_FILE={path}"),
    )
    .await;
}

/// An empty `SSL_CERT_FILE` still replaces the system store, so it is logged as set and refuses
/// the run
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_ca_bundle_path_refuses_the_run() {
    // the HTTP clients treat a present but empty SSL_CERT_FILE as set
    assert_ca_refusal(vec![("SSL_CERT_FILE", String::new())], "SSL_CERT_FILE=\"\"").await;
}

/// CA directories that don't exist, with no CA bundle, refuse the run
#[tokio::test(flavor = "multi_thread")]
async fn missing_ca_directories_refuse_the_run() {
    // every listed directory is missing, so no certificates load
    assert_ca_refusal(
        vec![("SSL_CERT_DIR", "/nonexistent/thorpal-ca-dir".to_owned())],
        "SSL_CERT_DIR=/nonexistent/thorpal-ca-dir",
    )
    .await;
}

/// An unreadable CA bundle doesn't refuse a run whose CA directory loads certificates, since the
/// model client is built from the directory
#[tokio::test(flavor = "multi_thread")]
async fn a_readable_ca_directory_covers_an_unreadable_bundle() {
    // the system's certificate directory, next to a bundle that doesn't exist
    let Some(dir) = ["/etc/ssl/certs", "/etc/pki/tls/certs"]
        .into_iter()
        .find(|dir| std::path::Path::new(dir).is_dir())
    else {
        return;
    };
    let (code, logs) = run_harness(vec![
        ("SSL_CERT_FILE", "/nonexistent/thorpal-ca/ca.pem".to_owned()),
        ("SSL_CERT_DIR", dir.to_owned()),
    ])
    .await;
    // the run gets past the start and fails at the unresolvable model host instead
    assert!(!logs.contains("refused the run"), "{logs}");
    assert_ne!(code, 2, "{logs}");
    assert_ne!(code, 101, "{logs}");
}
