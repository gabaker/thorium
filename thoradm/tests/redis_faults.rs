//! The API's Redis restore when Redis resets the connection partway: the error names the step
//! that failed and what the steps before it wrote, and the same restore succeeds once the fault
//! clears; after a failure that leaves no users, it says how an admin gets back in to run it again
//!
//! The binary's API and thoradm reach Redis through a fault proxy, so this binary holds one test.

mod common;

use common::harness::{ADMIN, Harness, namespace_conf};
use common::proxy::FaultProxy;
use std::net::SocketAddr;
use std::sync::OnceLock;
use thoradm::backup::failures::{FAILURES_FILE, read_records};
use thorium::client::ClientSettings;
use thorium::models::{AuthResponse, Backup};
use thorium::test_utilities::generators;
use thorium::{Conf, Thorium};

/// The port of the proxy in front of Redis, set before the harness starts
static PROXY_PORT: OnceLock<u16> = OnceLock::new();

/// Points a config's Redis at the fault proxy
///
/// # Arguments
///
/// * `conf` - The config to change
fn through_proxy(conf: &mut Conf) {
    // the proxy listens on loopback
    "127.0.0.1".clone_into(&mut conf.redis.host);
    conf.redis.port = *PROXY_PORT.get().expect("the proxy isn't started");
}

/// Returns the bytes of the command that writes the first field of an image or a user in a restore
///
/// # Arguments
///
/// * `key` - The image's or user's data key
fn field_write(key: &str) -> Vec<u8> {
    // the command and its key as Redis's protocol frames them
    format!("hsetnx\r\n${}\r\n{key}", key.len()).into_bytes()
}

/// A reset during the images step fails the restore with an error naming the step and the counts
/// of the steps before it, thoradm records it, and once the fault clears the same restore
/// succeeds and the cluster matches the backup; a reset during the users step, which leaves no
/// user to log in as, says how to get back in, and an admin bootstrapped that way runs the
/// restore again
///
/// The proxy runs on this test's runtime, which needs worker threads of its own to keep
/// forwarding while the test waits on thoradm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn a_reset_during_the_images_step_names_the_step_and_its_counts() {
    // a proxy in front of the lane's Redis, which the API and thoradm reach it through
    let direct = namespace_conf("proxy_probe", 0);
    let upstream: SocketAddr = format!("{}:{}", direct.redis.host, direct.redis.port)
        .parse()
        .expect("the lane's Redis host must be an address");
    let proxy = FaultProxy::start(upstream).await.unwrap();
    PROXY_PORT.set(proxy.listen.port()).unwrap();
    let harness = Harness::get_with(through_proxy).await;
    let ns = &harness.conf.thorium.namespace;
    // a group with images and pipelines, and a backup of the Redis state
    let client = harness.client().await;
    let (groups, images, _) = generators::gen_all(2, &client).await.unwrap();
    let out = harness.scratch("reset");
    let dir = out.join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", "redis", "-o"])
        .arg(&dir)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        taken.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&taken.stderr)
    );
    let backup: Backup =
        serde_json::from_slice(&std::fs::read(dir.join("redis.json")).unwrap()).unwrap();
    // Redis resets the connection that writes the first image
    let key = format!("{ns}:image_data:{}:{}", groups[0].name, images[0].name);
    proxy.reset_on(Some(field_write(&key)));
    // the API's restore fails, naming the images step and what the steps before it wrote
    let error = client
        .system
        .restore(&backup)
        .await
        .expect_err("the restore survived a reset");
    assert!(!proxy.waiting(), "the reset never fired");
    assert_eq!(error.status().map(|status| status.as_u16()), Some(500));
    let msg = error.msg().unwrap_or_default();
    assert!(msg.contains("the images step"), "{msg}");
    for written in [
        "settings 1".to_owned(),
        format!("users {}", backup.users.len()),
        format!("scoped tokens {}", backup.scoped_tokens.len()),
        format!("groups {}", backup.groups.len()),
    ] {
        assert!(msg.contains(&written), "{written}: {msg}");
    }
    assert!(!msg.contains("pipelines "), "{msg}");
    // thoradm records the same failure and exits 2
    proxy.reset_on(Some(field_write(&key)));
    let report = out.join("report-reset");
    let failed = harness
        .thoradm()
        .args(["backup", "restore", "--yes", "-b"])
        .arg(&dir)
        .arg("--report-dir")
        .arg(&report)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert_eq!(failed.status.code(), Some(2), "{stderr}");
    let records = read_records(&report.join(FAILURES_FILE)).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.component == "redis" && record.error.contains("the images step")),
        "{records:?}"
    );
    // once the fault clears, the same restore succeeds
    proxy.reset_on(None);
    let report = out.join("report-cleared");
    let restored = harness
        .thoradm()
        .args(["backup", "restore", "--yes", "-b"])
        .arg(&dir)
        .arg("--report-dir")
        .arg(&report)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert_eq!(restored.status.code(), Some(0), "{stderr}");
    // and the cluster matches the backup
    let verified = harness
        .thoradm()
        .args(["backup", "verify", "--cluster", "-b"])
        .arg(&dir)
        .arg("--report-dir")
        .arg(out.join("report-verify"))
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        verified.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    // Redis resets the connection that writes the admin in the users step
    let admin_key = format!("{ns}:user_data:{ADMIN}");
    proxy.reset_on(Some(field_write(&admin_key)));
    let error = client
        .system
        .restore(&backup)
        .await
        .expect_err("the restore survived a reset");
    assert!(!proxy.waiting(), "the reset never fired");
    proxy.reset_on(None);
    // the error names the users step and says the rerun needs an admin bootstrapped with the key
    assert_eq!(error.status().map(|status| status.as_u16()), Some(500));
    let msg = error.msg().unwrap_or_default();
    assert!(msg.contains("the users step"), "{msg}");
    assert!(msg.contains("may hold no users"), "{msg}");
    assert!(
        msg.contains("bootstrap an admin with the API's secret key"),
        "{msg}"
    );
    assert!(!msg.contains("can be run again once"), "{msg}");
    // no user is left, so thoradm's rerun fails, and its summary says how to get back in
    let report = out.join("report-locked");
    let locked = harness
        .thoradm()
        .args(["backup", "restore", "--yes", "-b"])
        .arg(&dir)
        .arg("--report-dir")
        .arg(&report)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&locked.stderr);
    assert_eq!(locked.status.code(), Some(2), "{stderr}");
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    let warnings = summary["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("bootstrapped with the API's secret key"))),
        "{summary:#}"
    );
    // an admin bootstrapped with the secret key runs the same restore again
    let bootstrapped = Thorium::bootstrap(
        &harness.api_url,
        "rerunadmin",
        "rerun-password",
        "rerun@example.invalid",
        &harness.conf.thorium.secret_key,
        &ClientSettings::default(),
    )
    .await
    .expect("failed to bootstrap an admin");
    let AuthResponse::Authed { token, .. } = bootstrapped else {
        panic!("the bootstrapped admin needs email verification");
    };
    let rerun = Thorium::build(harness.api_url.clone())
        .token(&token)
        .build()
        .await
        .expect("failed to build the bootstrapped admin's client");
    rerun.system.restore(&backup).await.unwrap();
    // the backup's admin logs in again
    assert_eq!(client.users.info().await.unwrap().username, ADMIN);
    harness.cleanup().await.unwrap();
}
