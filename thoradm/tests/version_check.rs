//! thoradm and the API at one version: a backup of the Redis state and a restore that loads it or
//! wipes are refused against an API reporting another version, or none, before anything is
//! written, unless `--allow-version-mismatch` lets them go ahead; `verify --cluster` only warns,
//! and the offline checks never ask
//!
//! thoradm reaches the API through a proxy that rewrites or removes the `thorium-version` header
//! of every answer. Every test restores or compares the cluster, so each holds the binary's
//! restore guard.

mod common;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use common::cycle::{describe, wipe};
use common::harness::{Harness, thoradm_command, write_ctl_conf};
use common::names::Names;
use common::seed;
use common::state::target_state;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, Operation, read_records};

/// The header the API names its version in
const VERSION_HEADER: &str = "thorium-version";

/// The version the rewriting proxy reports, which no thoradm has
const OTHER_VERSION: &str = "0.0.1-other";

/// The flag that lets a run go ahead against an API of another version
const ALLOW: &str = "--allow-version-mismatch";

/// thoradm's own version, which the API in this binary also runs
const OWN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The components of the backups that hold the Redis state
const WITH_REDIS: &str = "samples-list,s3-ids,s3-ids-objects,tags,redis";

/// The components of the backups that don't
const WITHOUT_REDIS: &str = "samples-list,s3-ids,s3-ids-objects,tags";

/// What the proxy does to the version header of every answer
#[derive(Debug, Clone)]
enum HeaderChange {
    /// Report this version instead of the API's
    Rewrite(&'static str),
    /// Send no version at all
    Remove,
}

/// What the proxy's handler shares
struct ProxyState {
    /// The API's URL
    upstream: String,
    /// The client forwarding to the API
    client: reqwest::Client,
    /// What happens to the version header
    change: HeaderChange,
}

/// A proxy in front of the in-process API with a Thorctl config that reaches the API through it
struct VersionProxy {
    /// The Thorctl config naming the proxy as the API
    ctl_conf: PathBuf,
}

impl VersionProxy {
    /// Starts a proxy on an ephemeral port on the calling test's runtime and writes its Thorctl
    /// config
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the API
    /// * `change` - What the proxy does to the version header
    /// * `dir` - The directory the Thorctl config is written to
    async fn start(harness: &Harness, change: HeaderChange, dir: &Path) -> Self {
        // bind an ephemeral port and serve every request through one handler
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(ProxyState {
            upstream: harness.api_url.clone(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            change,
        });
        let app = Router::new().fallback(forward).with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // a Thorctl config naming the proxy as the API
        let ctl_conf = dir.join(format!("thorctl-{port}.yml"));
        write_ctl_conf(
            &format!("http://127.0.0.1:{port}"),
            &harness.token,
            &ctl_conf,
        )
        .unwrap();
        VersionProxy { ctl_conf }
    }

    /// Builds a thoradm command against the harness's cluster that reaches the API through this
    /// proxy
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the cluster
    fn thoradm(&self, harness: &Harness) -> Command {
        // the namespace's cluster config, with this proxy's Thorctl config
        thoradm_command(&harness.cluster_conf, &self.ctl_conf)
    }
}

/// Forwards one request to the API and relays its answer with the version header changed
///
/// # Arguments
///
/// * `state` - The proxy's state
/// * `request` - The request to forward
async fn forward(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    // copy the request, leaving out the headers tied to this hop
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let mut upstream = state
        .client
        .request(method, format!("{}{path}", state.upstream))
        .body(body);
    for (name, value) in &headers {
        if !matches!(
            name.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection"
        ) {
            upstream = upstream.header(name, value);
        }
    }
    let answer = match upstream.send().await {
        Ok(answer) => answer,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(error.to_string()))
                .unwrap();
        }
    };
    // relay the status and headers, with the version header changed
    let mut response = Response::builder().status(answer.status());
    for (name, value) in answer.headers() {
        if !matches!(
            name.as_str(),
            VERSION_HEADER | "content-length" | "transfer-encoding" | "connection"
        ) {
            response = response.header(name, value);
        }
    }
    if let HeaderChange::Rewrite(version) = &state.change {
        response = response.header(VERSION_HEADER, HeaderValue::from_static(version));
    }
    let bytes = answer.bytes().await.unwrap_or_default();
    response.body(Body::from(bytes)).unwrap()
}

/// Runs a thoradm command without a terminal
///
/// # Arguments
///
/// * `cmd` - The command, with its arguments
fn run(mut cmd: Command) -> Output {
    // no terminal, so a restore needs --confirm
    cmd.stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm")
}

/// Empties the target's backed-up data and initializes the system again, as a fresh install has
/// it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn empty_target(harness: &Harness) {
    // wipe everything but the admin's login, then put back what a fresh install has
    wipe(harness).await;
    harness
        .client()
        .await
        .system
        .init()
        .await
        .expect("failed to initialize the system");
}

/// Seeds a group with a sample into an empty target
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn seed_sample(harness: &Harness) {
    // one sample in a group of its own
    empty_target(harness).await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("vesta");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "vesta.bin", names.suffix.as_bytes()).await;
}

/// Backs up the namespace straight from the API, with no proxy in between
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `components` - The components to back up
/// * `out` - The backup directory
fn direct_backup(harness: &Harness, components: &str, out: &Path) {
    // the harness's own configs
    let mut cmd = harness.thoradm();
    cmd.args(["backup", "new", components, "-o"]).arg(out);
    let taken = run(cmd);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
}

/// Returns the WARN lines of a run's stderr that name the version mismatch
///
/// # Arguments
///
/// * `output` - The run
fn mismatch_warnings(output: &Output) -> Vec<String> {
    // a WARN line naming thoradm's version and the flag or the API's version
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.contains("WARN") && line.contains(OWN_VERSION))
        .map(ToOwned::to_owned)
        .collect()
}

/// Returns the version the API reports through a proxy, as a refusal names it
///
/// # Arguments
///
/// * `change` - What the proxy does to the version header
fn reported(change: &HeaderChange) -> &'static str {
    // a rewritten version is named; a missing one is said to be missing
    match change {
        HeaderChange::Rewrite(version) => version,
        HeaderChange::Remove => "no version",
    }
}

/// Restores a backup through a proxy, its report in a fresh directory, and returns the run with
/// its records and report directory
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `proxy` - The proxy thoradm reaches the API through
/// * `backup` - The backup to restore
/// * `extra` - Extra `backup restore` arguments
fn restore(
    harness: &Harness,
    proxy: &VersionProxy,
    backup: &Path,
    extra: &[&str],
) -> (Output, Vec<FailureRecord>, PathBuf) {
    // a fresh report directory, so this run's records are the only ones read
    let report_dir = backup
        .parent()
        .unwrap()
        .join(format!("report-{}", uuid::Uuid::new_v4()));
    let mut cmd = proxy.thoradm(harness);
    cmd.args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(&report_dir)
        .args(extra);
    let output = run(cmd);
    let records = read_records(&report_dir.join(FAILURES_FILE)).unwrap_or_default();
    (output, records, report_dir)
}

/// Checks a restore was refused by the version check: exit 1 and a preflight record naming both
/// versions and the flag
///
/// # Arguments
///
/// * `case` - The case, for the message
/// * `output` - The run
/// * `records` - Its records
/// * `api` - How the refusal names the API's version
fn assert_version_refusal(case: &str, output: &Output, records: &[FailureRecord], api: &str) {
    // exit 1 and the restore's own preflight record for the version check
    assert_eq!(
        output.status.code(),
        Some(1),
        "{case}: {}",
        describe(output)
    );
    let refusal = records
        .iter()
        .find(|record| record.component == "restore" && record.operation == Operation::Preflight)
        .unwrap_or_else(|| panic!("{case}: no preflight record: {}", describe(output)));
    assert_eq!(
        refusal.object.as_deref(),
        Some("version"),
        "{case}: {refusal:?}"
    );
    for text in [OWN_VERSION, api, ALLOW] {
        assert!(refusal.error.contains(text), "{case}: {text}: {refusal:?}");
    }
}

/// A backup with the `redis` component through an API reporting another version, or none, exits
/// 1 naming both versions and the flag and writes nothing; with `--allow-version-mismatch` it
/// runs to completion, warns, and records the mismatch in its manifest; a backup without the
/// `redis` component isn't refused
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redis_backup_through_another_version_is_refused_before_writing() {
    // a sample to back up, and the two proxies
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    seed_sample(harness).await;
    let dir = harness.scratch("backup-version");
    for change in [HeaderChange::Rewrite(OTHER_VERSION), HeaderChange::Remove] {
        let proxy = VersionProxy::start(harness, change.clone(), &dir).await;
        let case = format!("{change:?}");
        // refused: exit 1, both versions and the flag named, and nothing written
        let out = dir.join(format!("refused-{}", uuid::Uuid::new_v4()));
        let mut cmd = proxy.thoradm(harness);
        cmd.args(["backup", "new", WITH_REDIS, "-o"]).arg(&out);
        let refused = run(cmd);
        assert_eq!(
            refused.status.code(),
            Some(1),
            "{case}: {}",
            describe(&refused)
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        for text in [OWN_VERSION, reported(&change), ALLOW] {
            assert!(
                stderr.contains(text),
                "{case}: {text}: {}",
                describe(&refused)
            );
        }
        assert!(
            !out.join("redis.json").exists(),
            "{case}: redis.json was written"
        );
        let written: Vec<_> = std::fs::read_dir(&out).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .collect()
            },
        );
        assert!(
            written.is_empty(),
            "{case}: the refused backup wrote {written:?}"
        );
        // allowed: the backup finishes, warns, and records the mismatch in its manifest
        let out = dir.join(format!("allowed-{}", uuid::Uuid::new_v4()));
        let mut cmd = proxy.thoradm(harness);
        cmd.args(["backup", "new", WITH_REDIS, ALLOW, "-o"])
            .arg(&out);
        let allowed = run(cmd);
        assert_eq!(
            allowed.status.code(),
            Some(0),
            "{case}: {}",
            describe(&allowed)
        );
        assert!(out.join("redis.json").exists(), "{case}: no redis.json");
        assert!(
            !mismatch_warnings(&allowed).is_empty(),
            "{case}: no WARN naming the mismatch: {}",
            describe(&allowed)
        );
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        let recorded = &manifest["api_version_mismatch"];
        assert_eq!(recorded["thoradm"], OWN_VERSION, "{case}: {manifest}");
        match &change {
            HeaderChange::Rewrite(version) => assert_eq!(recorded["api"], *version, "{case}"),
            HeaderChange::Remove => assert!(recorded["api"].is_null(), "{case}: {recorded}"),
        }
        // a backup without the Redis state doesn't ask the API's version
        let out = dir.join(format!("no-redis-{}", uuid::Uuid::new_v4()));
        let mut cmd = proxy.thoradm(harness);
        cmd.args(["backup", "new", WITHOUT_REDIS, "-o"]).arg(&out);
        let plain = run(cmd);
        assert_eq!(plain.status.code(), Some(0), "{case}: {}", describe(&plain));
        assert!(
            mismatch_warnings(&plain).is_empty(),
            "{case}: {}",
            describe(&plain)
        );
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        assert!(
            manifest.get("api_version_mismatch").is_none(),
            "{case}: {manifest}"
        );
    }
    // a backup straight from the API, of the same version, records no mismatch
    let out = dir.join("matching");
    direct_backup(harness, WITH_REDIS, &out);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest.get("api_version_mismatch").is_none(), "{manifest}");
}

/// A restore of a backup holding `redis.json`, and a `--wipe` restore of one without it, through
/// an API reporting another version, or none, exit 1 with a preflight record naming both versions
/// and the flag, and leave the target's tables, buckets, and Redis keys unchanged; with
/// `--allow-version-mismatch` the restore runs to completion, warns, and records the mismatch in
/// its summary; a restore of a backup without `redis.json` and without `--wipe` isn't refused
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn a_restore_through_another_version_is_refused_before_writing() {
    // a backup with the Redis state and one without, taken straight from the API
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    seed_sample(harness).await;
    let dir = harness.scratch("restore-version");
    let with_redis = dir.join("with-redis");
    direct_backup(harness, WITH_REDIS, &with_redis);
    let without_redis = dir.join("without-redis");
    direct_backup(harness, WITHOUT_REDIS, &without_redis);
    let ns = harness.conf.thorium.namespace.clone();
    for change in [HeaderChange::Rewrite(OTHER_VERSION), HeaderChange::Remove] {
        let proxy = VersionProxy::start(harness, change.clone(), &dir).await;
        let case = format!("{change:?}");
        // a restore loading redis.json is refused and changes nothing
        empty_target(harness).await;
        let before = target_state(harness).await;
        let (output, records, _) = restore(harness, &proxy, &with_redis, &[]);
        assert_version_refusal(&case, &output, &records, reported(&change));
        assert_eq!(
            target_state(harness).await,
            before,
            "{case}: the target changed"
        );
        // a restore without redis.json and without --wipe doesn't ask the API's version
        let (output, records, _) = restore(harness, &proxy, &without_redis, &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{case}: {}\n{records:?}",
            describe(&output)
        );
        assert!(
            mismatch_warnings(&output).is_empty(),
            "{case}: {}",
            describe(&output)
        );
        // a --wipe restore is refused even without redis.json, and changes nothing
        let before = target_state(harness).await;
        let (output, records, _) =
            restore(harness, &proxy, &without_redis, &["--wipe", ns.as_str()]);
        assert_version_refusal(&case, &output, &records, reported(&change));
        assert_eq!(
            target_state(harness).await,
            before,
            "{case}: the target changed"
        );
        // allowed: the restore finishes, warns, and records the mismatch in its summary
        empty_target(harness).await;
        let (output, records, report_dir) = restore(harness, &proxy, &with_redis, &[ALLOW]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{case}: {}\n{records:?}",
            describe(&output)
        );
        assert!(
            !mismatch_warnings(&output).is_empty(),
            "{case}: no WARN naming the mismatch: {}",
            describe(&output)
        );
        let summary: Value =
            serde_json::from_slice(&std::fs::read(report_dir.join("summary.json")).unwrap())
                .unwrap();
        let warnings = summary["warnings"].as_array().cloned().unwrap_or_default();
        assert!(
            warnings.iter().any(|warning| {
                let warning = warning.as_str().unwrap_or_default();
                warning.contains(OWN_VERSION)
                    && warning.contains(reported(&change))
                    && warning.contains(ALLOW)
            }),
            "{case}: the summary doesn't record the mismatch: {summary}"
        );
    }
}

/// `verify --cluster` through an API reporting another version logs the mismatch at WARN and still
/// checks the cluster, and the offline `backup verify` and `backup scrub` never ask the API's
/// version
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_cluster_only_warns_and_offline_checks_never_ask() {
    // a backup of the cluster as it is, taken straight from the API
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    seed_sample(harness).await;
    let dir = harness.scratch("verify-version");
    let backup = dir.join("backup");
    direct_backup(harness, WITH_REDIS, &backup);
    let proxy = VersionProxy::start(harness, HeaderChange::Rewrite(OTHER_VERSION), &dir).await;
    // verify --cluster warns, naming both versions, and finds the cluster matching
    let mut cmd = proxy.thoradm(harness);
    cmd.args(["backup", "verify", "--cluster", "-b"])
        .arg(&backup);
    let verified = run(cmd);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
    let warnings = mismatch_warnings(&verified);
    assert!(
        warnings.iter().any(|line| line.contains(OTHER_VERSION)),
        "no WARN naming both versions: {}",
        describe(&verified)
    );
    // the offline checks finish without a word about versions
    for args in [
        vec!["backup", "verify", "-b"],
        vec!["backup", "scrub", "-b"],
    ] {
        let mut cmd = proxy.thoradm(harness);
        cmd.args(&args).arg(&backup);
        let checked = run(cmd);
        assert_eq!(
            checked.status.code(),
            Some(0),
            "{args:?}: {}",
            describe(&checked)
        );
        assert!(
            !String::from_utf8_lossy(&checked.stderr).contains(OTHER_VERSION),
            "{args:?}: {}",
            describe(&checked)
        );
    }
}
