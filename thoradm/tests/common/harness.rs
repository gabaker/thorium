//! Run isolation, readiness, guards, cleanup, and the in-process API for thoradm's tests
//!
//! Every test binary gets its own namespace: a Scylla keyspace `testing_<id>`, buckets
//! `testing-<id>-<role>` for every bucket role, and the Redis keys under `<keyspace>:`. The id is
//! a run id (`THORADM_TEST_RUN`, or a random one) joined with the binary's name, so binaries and
//! concurrent runs never share data. The harness starts one Thorium API in-process for the
//! binary, bootstrapped with an admin, and writes the cluster and Thorctl configs thoradm reads.

use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule,
};
use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use rand::Rng;
use scylla::client::session::Session;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thorium::client::ClientSettings;
use thorium::models::AuthResponse;
use thorium::{Conf, Thorium};
use tokio::sync::{OnceCell, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The template config every test namespace starts from, relative to the thoradm crate
const CONF_TEMPLATE: &str = "../api/tests/thorium-testing.yml";

/// How long the harness waits for each backing service before naming it as down
const READY_DEADLINE: Duration = Duration::from_secs(120);

/// The name of the admin the harness bootstraps in every namespace
pub const ADMIN: &str = "thorium";

/// The harness for this test binary, started by the first test that asks for it
static HARNESS: OnceCell<Harness> = OnceCell::const_new();

/// Shared by every test of a binary that restores, and held alone by each restoring test
///
/// A restore changes more than its own data: the Redis restore replaces the namespace's whole
/// Redis state, the users and groups other tests use included, and the Scylla restore drops the
/// materialized views the API reads until it recreates them, so no other test of the binary may
/// run while a restore does. Other binaries, each in its own namespace, may share the Redis.
static NAMESPACE_LOCK: RwLock<()> = RwLock::const_new(());

/// Every bucket role in a Thorium config, in the order the config lists them
pub const BUCKET_ROLES: &[&str] = &[
    "files",
    "repos",
    "attachments",
    "results",
    "graphics",
    "ephemeral",
    "reaction_cache",
];

/// Returns every bucket role a Thorium config has: each section of `thorium` with a `bucket`
///
/// # Arguments
///
/// * `conf` - The config to read
///
/// # Panics
///
/// Panics if the config doesn't serialize to JSON.
#[must_use]
pub fn config_bucket_roles(conf: &Conf) -> Vec<String> {
    // walk the serialized config rather than a list, so a new bucket can't be missed
    let thorium = serde_json::to_value(&conf.thorium).expect("configs serialize");
    let mut roles: Vec<String> = thorium.as_object().map_or_else(Vec::new, |sections| {
        sections
            .iter()
            .filter(|(_, section)| {
                section
                    .get("bucket")
                    .is_some_and(serde_json::Value::is_string)
            })
            .map(|(name, _)| name.clone())
            .collect()
    });
    roles.sort();
    roles
}

/// The isolated namespace and running API of one test binary
#[derive(Debug)]
pub struct Harness {
    /// The id that names this binary's keyspace, buckets, and Redis keys
    pub id: String,
    /// The Thorium config of this binary's namespace
    pub conf: Conf,
    /// The URL of the in-process API
    pub api_url: String,
    /// The admin token for the in-process API
    pub token: String,
    /// The directory holding this binary's configs and scratch files
    pub dir: PathBuf,
    /// The cluster config file thoradm reads (`--cluster-conf`)
    pub cluster_conf: PathBuf,
    /// The Thorctl config file thoradm reads (`--ctl-conf`)
    pub ctl_conf: PathBuf,
}

/// Returns the bucket name a config uses for a role
///
/// # Arguments
///
/// * `conf` - The config to read
/// * `role` - The bucket role (`files`, `results`, …)
#[must_use]
pub fn bucket_for(conf: &Conf, role: &str) -> String {
    // map each role to the config field that names its bucket
    match role {
        "files" => conf.thorium.files.bucket.clone(),
        "repos" => conf.thorium.repos.bucket.clone(),
        "attachments" => conf.thorium.attachments.bucket.clone(),
        "results" => conf.thorium.results.bucket.clone(),
        "graphics" => conf.thorium.graphics.bucket.clone(),
        "ephemeral" => conf.thorium.ephemeral.bucket.clone(),
        "reaction_cache" => conf.thorium.reaction_cache.bucket.clone(),
        other => panic!("unknown bucket role {other}"),
    }
}

/// Sets the bucket name a config uses for a role
///
/// # Arguments
///
/// * `conf` - The config to change
/// * `role` - The bucket role (`files`, `results`, …)
/// * `bucket` - The bucket name to use
fn set_bucket(conf: &mut Conf, role: &str, bucket: String) {
    // map each role to the config field that names its bucket
    match role {
        "files" => conf.thorium.files.bucket = bucket,
        "repos" => conf.thorium.repos.bucket = bucket,
        "attachments" => conf.thorium.attachments.bucket = bucket,
        "results" => conf.thorium.results.bucket = bucket,
        "graphics" => conf.thorium.graphics.bucket = bucket,
        "ephemeral" => conf.thorium.ephemeral.bucket = bucket,
        "reaction_cache" => conf.thorium.reaction_cache.bucket = bucket,
        other => panic!("unknown bucket role {other}"),
    }
}

/// Refuses to touch any keyspace or bucket that isn't a test one
///
/// # Arguments
///
/// * `conf` - The config whose keyspace and buckets the harness will write or wipe
pub fn guard(conf: &Conf) -> Result<(), String> {
    // the keyspace must be a test keyspace
    if !conf.thorium.namespace.starts_with("testing_") {
        return Err(format!(
            "refusing to use keyspace {:?}: test keyspaces must start with \"testing_\"",
            conf.thorium.namespace
        ));
    }
    // the keyspace must pass the namespace rule a loaded config enforces, since the harness
    // names it after loading
    thorium::conf::validate_namespace(&conf.thorium.namespace)?;
    // every bucket must be a test bucket
    for role in BUCKET_ROLES {
        // get the bucket for this role
        let bucket = bucket_for(conf, role);
        // refuse anything that isn't clearly a test bucket
        if !bucket.starts_with("testing-") {
            return Err(format!(
                "refusing to use bucket {bucket:?} for role {role}: test buckets must start with \"testing-\""
            ));
        }
    }
    Ok(())
}

/// The environment variable that names the run
pub const RUN_VAR: &str = "THORADM_TEST_RUN";

/// Decide this run's id
///
/// A run id names the keyspace, buckets, and Redis keys of every binary, so an empty or shared
/// id would make two runs wipe each other's data at start. An empty id is always refused, and in
/// CI the id must be the pipeline's `<stamp>-<5 hex digits>`.
///
/// # Arguments
///
/// * `configured` - The value of `THORADM_TEST_RUN`, if it is set
/// * `ci` - Whether the `CI` environment variable is `true`
/// * `random` - Makes a random id for a local run that names none
pub fn choose_run_id(
    configured: Option<&str>,
    ci: bool,
    random: impl FnOnce() -> String,
) -> Result<String, String> {
    // an id that is set must name the run
    match configured {
        Some("") => Err(format!(
            "{RUN_VAR} is set but empty: two runs would share their names"
        )),
        Some(run) if ci && !is_stamped(run) => Err(format!(
            "{RUN_VAR}={run:?} isn't this pipeline's run id (<stamp>-<5 hex digits>), so runs \
             could share names and wipe each other"
        )),
        Some(run) => Ok(run.to_lowercase().replace('-', "_")),
        // CI must name its run, and a local run gets a random id
        None if ci => Err(format!(
            "set {RUN_VAR} to this job's run id (<stamp>-<5 hex digits>): CI runs never share a \
             default id"
        )),
        None => Ok(random()),
    }
}

/// Returns whether an id has the pipeline's form: seven hex digits, a dash, five hex digits
///
/// # Arguments
///
/// * `run` - The id
fn is_stamped(run: &str) -> bool {
    // the two parts, each of lowercase hex digits
    run.split_once('-').is_some_and(|(stamp, random)| {
        let hex = |part: &str, len: usize| {
            part.len() == len && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        hex(stamp, 7) && hex(random, 5)
    })
}

/// Returns this run's id: `THORADM_TEST_RUN` when set, else a random six-character id
///
/// # Panics
///
/// Panics when the environment gives no usable run id.
#[must_use]
pub fn run_id() -> String {
    // read the run id and whether this is a CI job
    let configured = std::env::var(RUN_VAR).ok();
    let ci = std::env::var("CI").is_ok_and(|value| value == "true");
    // choose it, making a short random id for a local run that names none
    choose_run_id(configured.as_deref(), ci, || {
        let mut rng = rand::rng();
        (0..6)
            .map(|_| char::from(b"abcdefghijklmnopqrstuvwxyz0123456789"[rng.random_range(0..36)]))
            .collect()
    })
    .unwrap_or_else(|err| panic!("{err}"))
}

/// Lists every key matching a pattern with `SCAN`, so a large database is never blocked
///
/// # Arguments
///
/// * `connection` - The Redis connection to scan on
/// * `pattern` - The glob pattern the keys must match
pub async fn scan_keys<C: redis::aio::ConnectionLike>(
    connection: &mut C,
    pattern: &str,
) -> Result<Vec<String>, redis::RedisError> {
    // walk the keys page by page until the scan wraps around
    let mut found = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        // get the next page of matching keys
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(connection)
            .await?;
        found.extend(keys);
        // stop once the scan has covered every key; a key may appear on two pages
        if next == 0 {
            found.sort();
            found.dedup();
            return Ok(found);
        }
        cursor = next;
    }
}

/// Returns the name of the running test binary without cargo's hash suffix
#[must_use]
pub fn binary_name() -> String {
    // read the executable's file name
    let exe = std::env::current_exe().expect("failed to read the test binary's path");
    let stem = exe
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    // cargo names test binaries `<name>-<hash>`
    match stem.rsplit_once('-') {
        Some((name, _)) => name.to_lowercase(),
        None => stem.to_lowercase(),
    }
}

/// Builds the config of an isolated namespace from the template and the environment
///
/// # Arguments
///
/// * `id` - The id naming the keyspace and buckets
/// * `port` - The port the in-process API binds
pub fn namespace_conf(id: &str, port: u16) -> Conf {
    // load the template with the lane's THORIUM__ overrides on top
    let mut conf = Conf::new(CONF_TEMPLATE)
        .unwrap_or_else(|err| panic!("failed to load the test config {CONF_TEMPLATE}: {err}"));
    // name the keyspace and Redis prefix for this id
    conf.thorium.namespace = format!("testing_{id}");
    // name every bucket for this id; bucket names can't hold underscores
    for role in BUCKET_ROLES {
        set_bucket(
            &mut conf,
            role,
            format!(
                "testing-{}-{}",
                id.replace('_', "-"),
                role.replace('_', "-")
            ),
        );
    }
    // bind the API to loopback on this binary's port
    "127.0.0.1".clone_into(&mut conf.thorium.interface);
    conf.thorium.port = port;
    conf
}

/// Picks a free local port for the in-process API
#[must_use]
pub fn free_port() -> u16 {
    // bind an ephemeral port and release it for the API to take
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind a free port");
    listener
        .local_addr()
        .expect("failed to read the free port")
        .port()
}

/// Builds an S3 client for a config
///
/// # Arguments
///
/// * `conf` - The config naming the S3 endpoint and credentials
#[must_use]
pub fn s3_client(conf: &Conf) -> S3Client {
    // build credentials from the config
    let s3_conf = &conf.thorium.s3;
    let creds = Credentials::new(
        &s3_conf.access_key,
        &s3_conf.secret_token,
        None,
        None,
        "Thorium",
    );
    // build a client for the configured endpoint and addressing style
    let mut builder = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&s3_conf.endpoint)
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .force_path_style(s3_conf.use_path_style)
        .behavior_version_latest();
    // use the configured region, or a placeholder the SDK accepts
    let region = s3_conf
        .region
        .clone()
        .unwrap_or_else(|| "us-east-1".to_owned());
    builder = builder.region(aws_types::region::Region::new(region));
    S3Client::from_conf(builder.build())
}

/// Builds a Redis pool for a config
///
/// # Arguments
///
/// * `conf` - The config naming the Redis server
pub async fn redis_pool(conf: &Conf) -> Result<Pool<RedisConnectionManager>, String> {
    // reuse thoradm's own Redis client builder
    thoradm::shared::redis::get_client(conf)
        .await
        .map_err(|err| format!("failed to connect to redis: {err}"))
}

/// Waits for one backing service, retrying until a deadline
///
/// # Arguments
///
/// * `name` - The service's name, used in the error
/// * `deadline` - How long to keep trying
/// * `probe` - Checks the service once
async fn wait_for<F, Fut>(name: &str, deadline: Duration, mut probe: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    // keep probing until the service answers or the deadline passes
    let start = Instant::now();
    loop {
        // try the service once
        let error = match probe().await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        // give up and name the service once the deadline has passed
        if start.elapsed() >= deadline {
            return Err(format!(
                "{name} never became ready within {}s: {error}",
                deadline.as_secs()
            ));
        }
        // wait a moment before the next try
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Waits for Scylla, Redis, and S3, naming the first one that never comes up
///
/// # Arguments
///
/// * `conf` - The config naming the services
/// * `deadline` - How long to wait for each service
pub async fn wait_ready(conf: &Conf, deadline: Duration) -> Result<(), String> {
    // Scylla is ready once a session can run a query
    wait_for("scylla", deadline, || async {
        let session = tokio::time::timeout(
            Duration::from_secs(10),
            thoradm::shared::scylla::get_client(conf),
        )
        .await
        .map_err(|_| "connection timed out".to_owned())?
        .map_err(|err| err.to_string())?;
        session
            .query_unpaged("SELECT release_version FROM system.local", &[])
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    })
    .await?;
    // Redis is ready once it answers a PING
    wait_for("redis", deadline, || async {
        let connections = tokio::time::timeout(Duration::from_secs(10), redis_pool(conf))
            .await
            .map_err(|_| "connection timed out".to_owned())??;
        let mut connection = connections.get().await.map_err(|err| err.to_string())?;
        redis::cmd("PING")
            .query_async::<String>(&mut *connection)
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    })
    .await?;
    // S3 is ready once it lists buckets
    wait_for("s3", deadline, || async {
        tokio::time::timeout(
            Duration::from_secs(10),
            s3_client(conf).list_buckets().send(),
        )
        .await
        .map_err(|_| "request timed out".to_owned())?
        .map(|_| ())
        .map_err(|err| aws_sdk_s3::error::DisplayErrorContext(err).to_string())
    })
    .await
}

/// Deletes every Redis key under a namespace by prefix, never with `FLUSHDB`
///
/// # Arguments
///
/// * `conf` - The config naming the namespace and Redis server
pub async fn delete_redis_namespace(conf: &Conf) -> Result<u64, String> {
    // connect to Redis
    let connections = redis_pool(conf).await?;
    let mut connection = connections.get().await.map_err(|err| err.to_string())?;
    // walk the namespace's keys with SCAN so a large namespace never blocks Redis
    let pattern = format!("{}:*", conf.thorium.namespace);
    let mut cursor: u64 = 0;
    let mut deleted = 0;
    loop {
        // get the next page of keys under the namespace
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut *connection)
            .await
            .map_err(|err| err.to_string())?;
        // delete this page's keys
        if !keys.is_empty() {
            deleted += redis::cmd("DEL")
                .arg(&keys)
                .query_async::<u64>(&mut *connection)
                .await
                .map_err(|err| err.to_string())?;
        }
        // stop once the scan wraps around
        if next == 0 {
            return Ok(deleted);
        }
        cursor = next;
    }
}

/// Drops a namespace's keyspace
///
/// # Arguments
///
/// * `conf` - The config naming the keyspace
pub async fn drop_keyspace(conf: &Conf) -> Result<(), String> {
    // refuse anything but a test keyspace
    guard(conf)?;
    // drop the keyspace; Scylla's schema agreement makes this safe to repeat
    let session = thoradm::shared::scylla::get_client(conf)
        .await
        .map_err(|err| err.to_string())?;
    session
        .query_unpaged(
            format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
            &[],
        )
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Lists every key in a bucket
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `bucket` - The bucket to list
pub async fn list_keys(s3: &S3Client, bucket: &str) -> Result<Vec<String>, String> {
    // page through the bucket's listing
    let mut keys = Vec::new();
    let mut pages = s3.list_objects_v2().bucket(bucket).into_paginator().send();
    while let Some(page) = pages.next().await {
        // a missing bucket holds nothing
        let page = match page {
            Ok(page) => page,
            Err(err) => {
                let text = aws_sdk_s3::error::DisplayErrorContext(&err).to_string();
                if text.contains("NoSuchBucket") {
                    return Ok(keys);
                }
                return Err(text);
            }
        };
        // collect this page's keys
        keys.extend(
            page.contents()
                .iter()
                .filter_map(|object| object.key().map(ToOwned::to_owned)),
        );
    }
    Ok(keys)
}

/// Deletes every object in a bucket, leaving the bucket itself
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `bucket` - The bucket to empty
pub async fn empty_bucket(s3: &S3Client, bucket: &str) -> Result<u64, String> {
    // refuse anything but a test bucket
    if !bucket.starts_with("testing-") {
        return Err(format!("refusing to empty non-test bucket {bucket}"));
    }
    // delete each listed object
    let keys = list_keys(s3, bucket).await?;
    for key in &keys {
        s3.delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|err| aws_sdk_s3::error::DisplayErrorContext(err).to_string())?;
    }
    Ok(keys.len() as u64)
}

/// Creates every bucket of a namespace that doesn't exist yet
///
/// # Arguments
///
/// * `conf` - The config naming the buckets
pub async fn create_buckets(conf: &Conf) -> Result<(), String> {
    // refuse anything but test buckets
    guard(conf)?;
    // create each role's bucket, accepting one that already exists
    let s3 = s3_client(conf);
    for role in BUCKET_ROLES {
        let bucket = bucket_for(conf, role);
        if let Err(err) = s3.create_bucket().bucket(&bucket).send().await {
            let text = aws_sdk_s3::error::DisplayErrorContext(&err).to_string();
            if !text.contains("BucketAlreadyOwnedByYou") && !text.contains("BucketAlreadyExists") {
                return Err(format!("failed to create bucket {bucket}: {text}"));
            }
        }
        // expire the bucket's objects after a day, so a run that is killed leaves nothing long
        s3.put_bucket_lifecycle_configuration()
            .bucket(&bucket)
            .lifecycle_configuration(one_day_lifecycle()?)
            .send()
            .await
            .map_err(|err| {
                format!(
                    "failed to set the lifecycle of bucket {bucket}: {}",
                    aws_sdk_s3::error::DisplayErrorContext(&err)
                )
            })?;
    }
    Ok(())
}

/// Builds the lifecycle that expires every object of a bucket after one day
#[allow(deprecated)]
fn one_day_lifecycle() -> Result<BucketLifecycleConfiguration, String> {
    // one enabled rule over the whole bucket; the empty prefix is deprecated, but Ceph's gateway
    // expects it
    let rule = LifecycleRule::builder()
        .expiration(LifecycleExpiration::builder().days(1).build())
        .status(ExpirationStatus::Enabled)
        .prefix("")
        .build()
        .map_err(|err| err.to_string())?;
    BucketLifecycleConfiguration::builder()
        .rules(rule)
        .build()
        .map_err(|err| err.to_string())
}

/// Starts the API in-process on its own thread and runtime
///
/// # Arguments
///
/// * `conf` - The config the API serves
fn spawn_api(conf: Conf) {
    // the API runs for the rest of the process on a runtime of its own
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 << 20)
            .build()
            .expect("failed to build the test API's runtime");
        let opts = thorium::utils::StartupOpts { apply_schema: true };
        runtime.block_on(async move {
            thorium::axum_with(conf, opts)
                .await
                .expect("the API starts")
        });
    });
}

/// Bootstraps the admin user against a freshly started API, retrying while it starts
///
/// # Arguments
///
/// * `api_url` - The API's URL
/// * `secret_key` - The API's secret key
async fn bootstrap_admin(api_url: &str, secret_key: &str) -> Result<String, String> {
    // keep trying while the API sets up its schema and binds its port
    let start = Instant::now();
    loop {
        // try to create the admin
        let attempt = Thorium::bootstrap(
            api_url,
            ADMIN,
            "bootstrap-password",
            "thorium@example.invalid",
            secret_key,
            &ClientSettings::default(),
        )
        .await;
        match attempt {
            Ok(AuthResponse::Authed { token, .. }) => return Ok(token),
            Ok(AuthResponse::VerifyEmail(_)) => {
                return Err("the bootstrapped admin needs email verification".to_owned());
            }
            Err(err) if start.elapsed() > Duration::from_secs(300) => {
                return Err(format!("failed to bootstrap the API at {api_url}: {err}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

impl Harness {
    /// Returns this binary's harness, starting it on first use
    pub async fn get() -> &'static Harness {
        // the namespace's config as the template and environment give it
        Harness::get_with(|_| ()).await
    }

    /// Returns this binary's harness, starting it on first use with an adjusted config; every
    /// test of a binary must pass the same adjustment
    ///
    /// # Arguments
    ///
    /// * `adjust` - Changes the namespace's config before anything starts
    pub async fn get_with(adjust: fn(&mut Conf)) -> &'static Harness {
        // start the harness once per binary; every later test reuses it
        HARNESS
            .get_or_init(|| async {
                Harness::start(adjust)
                    .await
                    .unwrap_or_else(|err| panic!("failed to start the test harness: {err}"))
            })
            .await
    }

    /// Starts an isolated namespace and its API
    ///
    /// # Arguments
    ///
    /// * `adjust` - Changes the namespace's config before anything starts
    async fn start(adjust: fn(&mut Conf)) -> Result<Harness, String> {
        // name everything for this run and binary
        let id = format!("{}_{}", run_id(), binary_name());
        let mut conf = namespace_conf(&id, free_port());
        adjust(&mut conf);
        // refuse anything that isn't a test keyspace or bucket
        guard(&conf)?;
        // wait for the backing services
        wait_ready(&conf, READY_DEADLINE).await?;
        // start from an empty namespace
        drop_keyspace(&conf).await?;
        delete_redis_namespace(&conf).await?;
        create_buckets(&conf).await?;
        let s3 = s3_client(&conf);
        for role in BUCKET_ROLES {
            empty_bucket(&s3, &bucket_for(&conf, role)).await?;
        }
        // start the API and bootstrap its admin
        let api_url = format!("http://127.0.0.1:{}", conf.thorium.port);
        spawn_api(conf.clone());
        let token = bootstrap_admin(&api_url, &conf.thorium.secret_key).await?;
        // initialize the system the way a fresh deployment does
        let client = Thorium::build(api_url.clone())
            .token(&token)
            .build()
            .await
            .map_err(|err| err.to_string())?;
        client.system.init().await.map_err(|err| err.to_string())?;
        // write the configs thoradm reads
        let dir = std::env::temp_dir().join(format!("thoradm-{id}"));
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let cluster_conf = dir.join("thorium.yml");
        let ctl_conf = dir.join("thorctl.yml");
        write_cluster_conf(&conf, &cluster_conf)?;
        write_ctl_conf(&api_url, &token, &ctl_conf)?;
        Ok(Harness {
            id,
            conf,
            api_url,
            token,
            dir,
            cluster_conf,
            ctl_conf,
        })
    }

    /// Builds an admin client for the in-process API in the calling test's runtime
    pub async fn client(&self) -> Thorium {
        // clients hold connections tied to the runtime that made them, so build one per call
        Thorium::build(self.api_url.clone())
            .token(&self.token)
            .build()
            .await
            .expect("failed to build an admin client")
    }

    /// Opens a Scylla session in the calling test's runtime
    pub async fn scylla(&self) -> Arc<Session> {
        // sessions die with the runtime that made them, so open one per call
        Arc::new(
            thoradm::shared::scylla::get_client(&self.conf)
                .await
                .expect("failed to open a scylla session"),
        )
    }

    /// Builds an S3 client for this namespace
    #[must_use]
    pub fn s3(&self) -> S3Client {
        // reuse the shared builder
        s3_client(&self.conf)
    }

    /// Builds a command that runs the thoradm binary against this namespace
    ///
    /// The command carries no `THORIUM__` variables from the test environment, so the config
    /// file alone decides which cluster thoradm talks to.
    #[must_use]
    pub fn thoradm(&self) -> Command {
        // this namespace's own cluster config
        self.thoradm_with(&self.cluster_conf)
    }

    /// Builds a command that runs the thoradm binary with another cluster config and this
    /// namespace's Thorctl config, such as one pointing S3 at a fault-injecting proxy
    ///
    /// # Arguments
    ///
    /// * `cluster_conf` - The cluster config thoradm reads
    #[must_use]
    pub fn thoradm_with(&self, cluster_conf: &Path) -> Command {
        // this namespace's own Thorctl config
        thoradm_command(cluster_conf, &self.ctl_conf)
    }

    /// Makes a fresh scratch directory for one test
    ///
    /// # Arguments
    ///
    /// * `name` - The test's name
    #[must_use]
    pub fn scratch(&self, name: &str) -> PathBuf {
        // put each test's files under the binary's directory
        let dir = self.dir.join(name);
        // start from an empty directory
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("failed to create a scratch directory");
        dir
    }

    /// Takes the guard every test of a binary that restores holds, so no restore runs beside it
    pub async fn shared_guard(&self) -> RwLockReadGuard<'static, ()> {
        // share the lock with every other test that doesn't restore
        NAMESPACE_LOCK.read().await
    }

    /// Takes the guard a restoring test holds alone while it restores
    pub async fn restore_guard(&self) -> RwLockWriteGuard<'static, ()> {
        // wait for every other test of the binary
        NAMESPACE_LOCK.write().await
    }

    /// Removes this namespace's data: called by a test once it has passed
    ///
    /// A failing test never reaches this, so its data stays for inspection.
    pub async fn cleanup(&self) -> Result<(), String> {
        // drop the keyspace and delete the namespace's Redis keys
        drop_keyspace(&self.conf).await?;
        delete_redis_namespace(&self.conf).await?;
        // empty every bucket of the namespace
        let s3 = self.s3();
        for role in BUCKET_ROLES {
            empty_bucket(&s3, &bucket_for(&self.conf, role)).await?;
        }
        Ok(())
    }
}

/// Builds a command that runs the thoradm binary with another cluster config and another
/// Thorctl config, such as one reaching the API through a proxy
///
/// # Arguments
///
/// * `cluster_conf` - The cluster config thoradm reads
/// * `ctl_conf` - The Thorctl config thoradm reads
#[must_use]
pub fn thoradm_command(cluster_conf: &Path, ctl_conf: &Path) -> Command {
    // start from the built binary
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_thoradm"));
    // drop every config override the test process inherited
    for (key, _) in std::env::vars() {
        if key.starts_with("THORIUM__") || key.starts_with("THORCTL__") {
            cmd.env_remove(key);
        }
    }
    // point thoradm at the configs
    cmd.arg("--cluster-conf")
        .arg(cluster_conf)
        .arg("--ctl-conf")
        .arg(ctl_conf);
    cmd
}

/// Writes a cluster config thoradm can load with `--cluster-conf`
///
/// # Arguments
///
/// * `conf` - The config to write
/// * `path` - Where to write it
pub fn write_cluster_conf(conf: &Conf, path: &Path) -> Result<(), String> {
    // serialize the config as YAML; the scylla override is already folded into the node list
    let yaml = serde_norway::to_string(conf).map_err(|err| err.to_string())?;
    std::fs::write(path, yaml).map_err(|err| err.to_string())
}

/// Writes a Thorctl config thoradm can load with `--ctl-conf`
///
/// # Arguments
///
/// * `api_url` - The API's URL
/// * `token` - The token to authenticate with
/// * `path` - Where to write it
pub fn write_ctl_conf(api_url: &str, token: &str, path: &Path) -> Result<(), String> {
    // a Thorctl config needs only the API and a token
    let yaml = format!("keys:\n  api: \"{api_url}\"\n  token: \"{token}\"\n");
    std::fs::write(path, yaml).map_err(|err| err.to_string())
}
