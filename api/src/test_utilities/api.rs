//! The utlities for integration tests involving the API
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::{
    error::DisplayErrorContext,
    types::{BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule},
};
use aws_types::region::Region;
use bb8_redis::{RedisConnectionManager, bb8::Pool};
use scylla::client::session::Session;
use std::{sync::LazyLock, time::Duration};
use tokio::sync::OnceCell;
use tokio_util::time::FutureExt;

use crate::models::schema::{ApplyOptions, ApplyTarget, apply, connect};
use crate::utils::StartupOpts;
use crate::{Conf, Error, Thorium, client::ClientSettings, conf::S3, models::AuthResponse};

/// The path to a valid Thorium config in the testing directory
///
/// This config is a placeholder, but must successfully pass validation.
/// Its values can be overridden using environment variables.
static CONF_PATH: &str = "../api/tests/thorium-testing.yml";

/// The environment variable that names the namespace the harness runs in
const NAMESPACE_VAR: &str = "THORIUM__THORIUM__NAMESPACE";

/// The environment variable that names a config file to use instead of [`CONF_PATH`]
///
/// With it set the harness is read-only: it never wipes or creates anything, so a config that
/// points at a real deployment can't damage it.
pub const TEST_CONFIG_VAR: &str = "THORIUM_TEST_CONFIG";

/// The config to use for integration tests
///
/// The config to read is the file `THORIUM_TEST_CONFIG` names when it's set, and otherwise the
/// template in the tests directory. Either way, environment variables (e.g.
/// `THORIUM__THORIUM__S3__ENDPOINT`) override the file's values.
pub static CONF: LazyLock<Conf> = LazyLock::new(|| {
    // load the config the harness reads
    let path = test_config_path(std::env::var(TEST_CONFIG_VAR).ok().as_deref());
    let mut conf = Conf::new(&path).unwrap_or_else(|_| panic!("Failed to load config at '{path}'"));
    // a run that names no namespace gets one of its own, or fails in CI
    let configured = std::env::var(NAMESPACE_VAR).is_ok_and(|namespace| !namespace.is_empty());
    let ci = std::env::var("CI").is_ok_and(|value| value == "true");
    match choose_namespace(configured, ci, read_only()) {
        Ok(NamespaceChoice::Keep) => {}
        Ok(NamespaceChoice::Generate) => {
            // the stamp is the current epoch minute and the random part tells processes apart
            let minutes = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs() / 60);
            conf.thorium.namespace = generated_namespace(minutes, rand::random());
        }
        Err(error) => panic!("{error}"),
    }
    conf
});

/// How the harness picks the namespace it runs in
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceChoice {
    /// Use the namespace the config and its environment overrides name
    Keep,
    /// Run in a namespace made up for this process
    Generate,
}

/// Decide where the harness's namespace comes from
///
/// A namespace named in the environment is always kept, and so is any namespace of a read-only
/// harness, which only reads what its config points at. Without one, a local run makes up a
/// random namespace so developers and runs never share a keyspace, and a CI run fails because
/// the job's namespace must have been chosen by the pipeline.
///
/// # Arguments
///
/// * `configured` - Whether `THORIUM__THORIUM__NAMESPACE` is set to a non-empty value
/// * `ci` - Whether the `CI` environment variable is `true`
/// * `read_only` - Whether `THORIUM_TEST_CONFIG` is set
pub fn choose_namespace(
    configured: bool,
    ci: bool,
    read_only: bool,
) -> Result<NamespaceChoice, String> {
    // a namespace the environment names, or a read-only harness's own, is the one to use
    if configured || read_only {
        return Ok(NamespaceChoice::Keep);
    }
    // otherwise CI must have named one, and a local run makes one up
    if ci {
        return Err(
            "set THORIUM__THORIUM__NAMESPACE to this job's namespace (testing_ci_<stamp>_<random>): \
             CI runs never share a default namespace"
                .to_owned(),
        );
    }
    Ok(NamespaceChoice::Generate)
}

/// Build a namespace unique to one process: `testing_ci_<stamp>_<random>`
///
/// The stamp is the epoch minute in seven hex digits and the random part is eight hex digits, so
/// the name is 27 characters (the longest a keyspace suffix budget allows) and shows its age.
///
/// # Arguments
///
/// * `epoch_minutes` - The current time in minutes since the Unix epoch
/// * `random` - Random bits that tell concurrent processes apart
#[must_use]
pub fn generated_namespace(epoch_minutes: u64, random: u32) -> String {
    // the stamp in seven hex digits, then the random part in eight
    format!("{TEST_NAMESPACE_PREFIX}ci_{epoch_minutes:07x}_{random:08x}")
}

/// Choose the config file the harness loads
///
/// # Arguments
///
/// * `override_path` - The value of `THORIUM_TEST_CONFIG`, if it is set
#[must_use]
pub fn test_config_path(override_path: Option<&str>) -> String {
    // an unset or empty variable leaves the template in place
    match override_path {
        Some(path) if !path.is_empty() => path.to_owned(),
        _ => CONF_PATH.to_owned(),
    }
}

/// Whether the harness is read-only because `THORIUM_TEST_CONFIG` is set
#[must_use]
pub fn read_only() -> bool {
    // a set, non-empty variable is the override
    std::env::var(TEST_CONFIG_VAR).is_ok_and(|path| !path.is_empty())
}

/// One thing the harness does to the backends before it starts the API
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapAction {
    /// Drop the test namespace's keyspace and delete its Redis keys
    WipeDatabases,
    /// Create the test buckets, with a lifecycle on each one it creates
    InitBuckets,
    /// Apply the declared schema to the empty keyspace
    ApplySchema,
}

/// The actions the harness takes before it starts the API, in order
///
/// A read-only harness takes none: it only reads what the config points at.
///
/// # Arguments
///
/// * `read_only` - Whether `THORIUM_TEST_CONFIG` is set
#[must_use]
pub fn bootstrap_plan(read_only: bool) -> Vec<BootstrapAction> {
    // wipe first so the schema lands on an empty keyspace
    if read_only {
        Vec::new()
    } else {
        vec![
            BootstrapAction::WipeDatabases,
            BootstrapAction::InitBuckets,
            BootstrapAction::ApplySchema,
        ]
    }
}

/// Apply the declared schema to the namespace's keyspace, as `thoradm schema apply` would
///
/// # Arguments
///
/// * `config` - The Thorium config we are using for tests
async fn apply_schema(config: &Conf) -> Result<(), Error> {
    // connect to scylla and converge it to the declared schema
    let scylla = get_scylla_client(config).await?;
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    };
    apply(&scylla, config, &options, "test harness", &|_| ())
        .await
        .map_err(|err| Error::new(format!("Failed to apply the schema: {err}")))?;
    Ok(())
}

/// Prepare the backends for a test API by running the bootstrap plan
///
/// # Arguments
///
/// * `conf` - The Thorium config the tests use
/// * `read_only` - Whether `THORIUM_TEST_CONFIG` is set
pub async fn prepare_backends(conf: &Conf, read_only: bool) -> Result<(), Error> {
    // run each planned action in order
    for action in bootstrap_plan(read_only) {
        match action {
            BootstrapAction::WipeDatabases => wipe_dbs(conf).await?,
            BootstrapAction::InitBuckets => init_s3_buckets(&conf.thorium.s3, conf).await?,
            BootstrapAction::ApplySchema => apply_schema(conf).await?,
        }
    }
    Ok(())
}

/// The addr to talk to the api at
///
/// The config's interface is the address the API *binds* to, which is usually a wildcard so it
/// answers on every interface. A wildcard is not a destination though: dialing it either fails
/// outright or, when an http proxy is configured in the environment, gets handed to a proxy that
/// cannot route it. The tests only ever talk to the API running in their own process, so rewrite
/// a wildcard bind to loopback rather than dialing it.
static ADDR: LazyLock<String> = LazyLock::new(|| addr_of(&CONF));

/// Build a scylla client for a specific cluster
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
async fn get_scylla_client(config: &Conf) -> Result<Session, Error> {
    // connect with the shared session settings
    let scylla = connect(config).await.expect("Failed to connect to scylla!");
    Ok(scylla)
}

/// Open a scylla session to the test cluster
///
/// Each call opens its own session, since every test runs in its own runtime.
pub async fn scylla_session() -> Result<Session, Error> {
    // connect to the scylla cluster in our test config
    get_scylla_client(&CONF).await
}

/// Setup a connection pool to the redis backend
///
/// The pool connects to the Redis database selected by the config's `redis.db`.
///
/// # Arguments
///
/// * `config` - The config for the Thorium API
pub async fn get_redis_client(config: &Conf) -> Pool<RedisConnectionManager> {
    // get redis config
    let redis = &config.redis;
    // build url to the configured database using authentication if its configured
    let url = match (&redis.username, &redis.password) {
        // redis with username/password auth setup
        (Some(user), Some(password)) => format!(
            "redis://{}:{}@{}:{}/{}",
            user, password, redis.host, redis.port, redis.db
        ),
        (None, Some(password)) => format!(
            "redis://default:{}@{}:{}/{}",
            password, redis.host, redis.port, redis.db
        ),
        (None, None) => format!("redis://{}:{}/{}", redis.host, redis.port, redis.db),
        _ => panic!("Redis Setup Error - Password must be set if username is set"),
    };
    // build manager
    let manager = match RedisConnectionManager::new(url) {
        Ok(manager) => manager,
        Err(e) => panic!("{}", e),
    };
    // build redis connection pool
    Pool::builder()
        .max_size(redis.pool_size.unwrap_or(50))
        .build(manager)
        .await
        .expect("Failed to build redis connection pool")
}

/// Open a redis connection pool to the test redis
pub async fn redis_pool() -> Pool<RedisConnectionManager> {
    // connect to the redis in our test config
    get_redis_client(&CONF).await
}

/// Drop the keyspace in Scylla
async fn wipe_scylla(config: &Conf) -> Result<(), Error> {
    // connect to scylla
    let scylla = get_scylla_client(config)
        .await
        .map_err(|err| Error::new(format!("Failed to get scylla client: {err}")))?;
    // drop our current keyspace; retry once on failure
    let drop_keyspace = || async {
        scylla
            .query_unpaged(
                format!("DROP KEYSPACE IF EXISTS {}", config.thorium.namespace),
                &(),
            )
            .timeout(tokio::time::Duration::from_secs(120))
            .await
            .map_err(|err| {
                Error::new(format!(
                    "Error connecting to scylla while dropping keyspace: {err}"
                ))
            })?
            .map_err(|err| Error::new(format!("Error dropping scylla keyspace: {err}")))
    };
    // first attempt
    if let Err(e) = drop_keyspace().await {
        // log the error and retry
        eprintln!("Failed to drop keyspace on first attempt: {e}");
        // sleep to let scylla get settled
        tokio::time::sleep(Duration::from_secs(2)).await;
        // second attempt
        drop_keyspace().await?;
    }
    Ok(())
}

/// The prefix every namespace the harness may wipe starts with
pub const TEST_NAMESPACE_PREFIX: &str = "testing_";

/// The prefix every bucket the harness may create or change starts with
pub const TEST_BUCKET_PREFIX: &str = "testing-";

/// Delete this test namespace's keys from Redis before running tests
///
/// Only keys under the namespace's prefix are deleted, so other namespaces sharing the Redis
/// database are left alone.
///
/// # Arguments
///
/// * `config` - The Thorium config we are using for tests
async fn wipe_redis(config: &Conf) -> Result<(), Error> {
    // connect to redis
    let redis = get_redis_client(config).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Error getting Redis connection from pool: {err}")))?;
    // match every key under our namespace, escaping any glob characters in its name
    let escaped = config
        .thorium
        .namespace
        .chars()
        .fold(String::new(), |mut escaped, letter| {
            if matches!(letter, '*' | '?' | '[' | ']' | '\\') {
                escaped.push('\\');
            }
            escaped.push(letter);
            escaped
        });
    let pattern = format!("{escaped}:*");
    // scan for our keys and delete each batch we find until the scan wraps around
    let mut cursor: u64 = 0;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("Error scanning keys in Redis: {err}")))?;
        // delete this batch of keys if we found any
        if !keys.is_empty() {
            let (): () = redis::cmd("DEL")
                .arg(&keys)
                .query_async(&mut *conn)
                .await
                .map_err(|err| Error::new(format!("Error deleting keys from Redis: {err}")))?;
        }
        // stop once the scan has covered every key
        if next == 0 {
            break;
        }
        cursor = next;
    }
    Ok(())
}

/// Wipe all of the databases under test (Scylla + Redis)
///
/// We don't currently test against ES. Only namespaces starting with [`TEST_NAMESPACE_PREFIX`]
/// are wiped, and any other namespace is refused before either database is contacted, so a test
/// config pointed at a real deployment can't drop its data.
///
/// # Arguments
///
/// * `config` - The Thorium config we are using for tests
pub async fn wipe_dbs(config: &Conf) -> Result<(), Error> {
    // refuse any namespace that isn't marked as a test namespace
    if !config.thorium.namespace.starts_with(TEST_NAMESPACE_PREFIX) {
        return Err(Error::new(format!(
            "Refusing to wipe namespace '{}': test namespaces must start with \
            '{TEST_NAMESPACE_PREFIX}', such as testing_thorium",
            config.thorium.namespace
        )));
    }
    tokio::try_join!(wipe_scylla(config), wipe_redis(config))?;
    Ok(())
}

/// Bind the address the test API serves on, failing if another process already holds it
///
/// Holding the port before anything is wiped means a second run on the same port stops here,
/// instead of wiping the first run's data and then bootstrapping against the first run's API.
///
/// # Arguments
///
/// * `config` - The Thorium config we are using for tests
fn bind_api_port(config: &Conf) -> Result<std::net::TcpListener, Error> {
    // bind the same interface and port the API would
    let interface = config.thorium.interface.as_str();
    let port = config.thorium.port;
    std::net::TcpListener::bind((interface, port)).map_err(|err| {
        Error::new(format!(
            "The test API port {interface}:{port} is in use by another process ({err}), so \
            nothing was wiped! Stop that process or pick another THORIUM__THORIUM__PORT."
        ))
    })
}

/// Create a single bucket for integration tests and set its lifecycle
///
/// Only buckets starting with [`TEST_BUCKET_PREFIX`] are touched, and the lifecycle is set only
/// when this call created the bucket, so an existing bucket keeps its own rules.
///
/// # Arguments
///
/// * `s3_client` - The S3 client to create the bucket
/// * `lifecycle` - The lifecycle to set for this bucket
/// * `bucket` - The name of the bucket to create
pub async fn init_bucket(
    s3_client: &aws_sdk_s3::Client,
    lifecycle: BucketLifecycleConfiguration,
    bucket: &str,
) -> Result<(), Error> {
    // refuse any bucket that isn't marked as a test bucket
    if !bucket.starts_with(TEST_BUCKET_PREFIX) {
        return Err(Error::new(format!(
            "Refusing to set up bucket '{bucket}': test buckets must start with \
            '{TEST_BUCKET_PREFIX}'"
        )));
    }
    // attempt to create the bucket
    if let Err(err) = s3_client.create_bucket().bucket(bucket).send().await {
        match err.as_service_error() {
            // a bucket we already own existed before this run, so leave its lifecycle alone
            Some(service_err) if service_err.is_bucket_already_owned_by_you() => return Ok(()),
            _ => {
                return Err(Error::new(format!(
                    "Error creating S3 bucket '{}': {}",
                    bucket,
                    DisplayErrorContext(&err)
                )));
            }
        }
    }
    // set the lifecycle on the bucket we just created
    s3_client
        .put_bucket_lifecycle_configuration()
        .bucket(bucket)
        .lifecycle_configuration(lifecycle)
        .send()
        .await
        .map_err(|err| {
            Error::new(format!(
                "Error setting lifecycle for bucket '{}': {}",
                bucket,
                DisplayErrorContext(&err)
            ))
        })?;
    Ok(())
}

/// Deletes the test buckets it names, with their objects, when it is dropped
///
/// A test that creates a bucket holds one of these so the bucket goes away when the test ends
/// however it ends, a panic included. Only buckets starting with [`TEST_BUCKET_PREFIX`] are
/// deleted.
pub struct TempBuckets {
    /// The S3 settings of the store that holds the buckets
    s3: S3,
    /// The buckets to delete
    buckets: Vec<String>,
}

impl TempBuckets {
    /// Names the buckets to delete when the guard is dropped
    ///
    /// # Arguments
    ///
    /// * `s3` - The S3 settings of the store that holds the buckets
    /// * `buckets` - The buckets to delete
    #[must_use]
    pub fn new(s3: &S3, buckets: Vec<String>) -> Self {
        // keep the settings rather than a client, since the guard deletes from a thread with a
        // runtime of its own, and a client's pooled connections belong to the runtime that made it
        TempBuckets {
            s3: s3.clone(),
            buckets,
        }
    }
}

impl Drop for TempBuckets {
    /// Deletes the buckets
    fn drop(&mut self) {
        // the test's own runtime may be shutting down, so delete from a thread with a runtime of
        // its own and wait for it
        let s3 = self.s3.clone();
        let buckets = std::mem::take(&mut self.buckets);
        let _ = std::thread::spawn(move || {
            // a runtime that can't be built leaves the buckets to their lifecycle
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                // a client built here uses this runtime's connections
                let s3 = s3_client(&s3);
                for bucket in &buckets {
                    // a bucket that can't be deleted is left to its lifecycle
                    if let Err(err) = delete_bucket_with_objects(&s3, bucket).await {
                        eprintln!("Couldn't delete temporary bucket {bucket}: {err}");
                    }
                }
            });
        })
        .join();
    }
}

/// Delete a test bucket after deleting every object in it
///
/// Only buckets starting with [`TEST_BUCKET_PREFIX`] are touched, and a bucket that doesn't exist
/// is a success.
///
/// # Arguments
///
/// * `s3_client` - The S3 client to delete with
/// * `bucket` - The name of the bucket to delete
async fn delete_bucket_with_objects(
    s3_client: &aws_sdk_s3::Client,
    bucket: &str,
) -> Result<(), Error> {
    // refuse any bucket that isn't marked as a test bucket
    if !bucket.starts_with(TEST_BUCKET_PREFIX) {
        return Err(Error::new(format!(
            "Refusing to delete bucket '{bucket}': test buckets must start with \
            '{TEST_BUCKET_PREFIX}'"
        )));
    }
    // delete every object a page at a time
    let mut token = None;
    loop {
        // a missing bucket has nothing to delete
        let page = match s3_client
            .list_objects_v2()
            .bucket(bucket)
            .set_continuation_token(token.take())
            .send()
            .await
        {
            Ok(page) => page,
            Err(err)
                if err.as_service_error().is_some_and(
                    aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error::is_no_such_bucket,
                ) =>
            {
                return Ok(());
            }
            Err(err) => {
                return Err(Error::new(format!(
                    "Error listing S3 bucket '{}': {}",
                    bucket,
                    DisplayErrorContext(&err)
                )));
            }
        };
        for object in page.contents() {
            if let Some(key) = object.key() {
                s3_client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|err| {
                        Error::new(format!(
                            "Error deleting object '{key}' in '{bucket}': {}",
                            DisplayErrorContext(&err)
                        ))
                    })?;
            }
        }
        // stop after the last page
        match page.next_continuation_token() {
            Some(next) => token = Some(next.to_owned()),
            None => break,
        }
    }
    // then the bucket itself
    s3_client
        .delete_bucket()
        .bucket(bucket)
        .send()
        .await
        .map_err(|err| {
            Error::new(format!(
                "Error deleting S3 bucket '{}': {}",
                bucket,
                DisplayErrorContext(&err)
            ))
        })?;
    Ok(())
}

/// Build an S3 client for the test object store
///
/// # Arguments
///
/// * `s3_conf` - The S3 configuration from the main Thorium configuration to use
#[must_use]
pub fn s3_client(s3_conf: &S3) -> aws_sdk_s3::Client {
    // get our s3 credentials
    let creds = aws_credential_types::Credentials::new(
        &s3_conf.access_key,
        &s3_conf.secret_token,
        None,
        None,
        "Thorium",
    );
    // build an s3 client
    let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&s3_conf.endpoint)
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .force_path_style(s3_conf.use_path_style);
    // if we have a region set then add that to our config
    if let Some(region) = &s3_conf.region {
        // set our region
        s3_config_builder = s3_config_builder.region(Region::new(region.clone()));
    }
    // build our s3 config
    let s3_config = s3_config_builder.build();
    // build our s3 client
    aws_sdk_s3::Client::from_conf(s3_config)
}

/// Create required S3 buckets for testing with a strict lifecycle policy that
/// removes objects automatically
///
/// # Arguments
///
/// * `s3_conf` - The S3 configuration from the main Thorium configuration to use
/// * `conf` - The Thorium config we are using for tests
async fn init_s3_buckets(s3_conf: &S3, conf: &Conf) -> Result<(), Error> {
    // build an s3 client
    let s3_client = s3_client(s3_conf);
    // create a 1 day lifecycle to set for each bucket
    let lifecycle = BucketLifecycleConfiguration::builder()
        .rules(
            LifecycleRule::builder()
                .expiration(LifecycleExpiration::builder().days(1).build())
                .status(ExpirationStatus::Enabled)
                // <Prefix/> is deprecated, but Ceph object gateway expects it so we include it here
                .prefix("")
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    // create all required buckets
    init_bucket(&s3_client, lifecycle.clone(), &conf.thorium.files.bucket).await?;
    init_bucket(&s3_client, lifecycle.clone(), &conf.thorium.repos.bucket).await?;
    init_bucket(&s3_client, lifecycle.clone(), &conf.thorium.results.bucket).await?;
    init_bucket(
        &s3_client,
        lifecycle.clone(),
        &conf.thorium.ephemeral.bucket,
    )
    .await?;
    init_bucket(
        &s3_client,
        lifecycle.clone(),
        &conf.thorium.attachments.bucket,
    )
    .await?;
    Ok(())
}

/// Get every stored row of an entity as its group and its serialized kind data
///
/// Tests use this to inspect what an entity write stored, including groups and fields the API
/// never returns.
///
/// # Arguments
///
/// * `id` - The id of the entity to read
pub async fn entity_rows(id: uuid::Uuid) -> Result<Vec<(String, String)>, Error> {
    // connect to Scylla on this test's runtime, since a session dies with the runtime it was
    // built on and every test has its own
    let session = get_scylla_client(&CONF).await?;
    // read every row of this entity from its id partition
    let query = format!(
        "SELECT group, kind_data FROM {}.entities_by_id WHERE id = ?",
        CONF.thorium.namespace
    );
    let result = session
        .query_unpaged(query, (id,))
        .await
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?;
    // collect each row's group and kind data
    let rows = result
        .into_rows_result()
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?;
    let mut entity_rows = Vec::new();
    for row in rows
        .rows::<(String, String)>()
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?
    {
        // add this row's group and kind data
        entity_rows
            .push(row.map_err(|err| Error::new(format!("Failed to read entity row: {err}")))?);
    }
    Ok(entity_rows)
}

/// Delete every stored row of an entity straight from Scylla, bypassing the API's checks
///
/// Tests use this to leave a dangling reference behind, as a partial write would.
///
/// # Arguments
///
/// * `id` - The id of the entity to delete
pub async fn delete_entity_rows(id: uuid::Uuid) -> Result<(), Error> {
    // connect to Scylla on this test's runtime
    let session = get_scylla_client(&CONF).await?;
    // find the partition of each of the entity's rows
    let query = format!(
        "SELECT kind, group, year, bucket, created FROM {}.entities_by_id WHERE id = ?",
        CONF.thorium.namespace
    );
    let result = session
        .query_unpaged(query, (id,))
        .await
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?;
    let rows = result
        .into_rows_result()
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?;
    let mut keys = Vec::new();
    for row in rows
        .rows::<(String, String, i32, i32, chrono::DateTime<chrono::Utc>)>()
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?
    {
        // keep this row's primary key
        keys.push(row.map_err(|err| Error::new(format!("Failed to read entity row: {err}")))?);
    }
    // delete each row from the base table, which also drops it from the views
    let delete = format!(
        "DELETE FROM {}.entities WHERE kind = ? AND group = ? AND year = ? AND bucket = ? AND created = ? AND id = ?",
        CONF.thorium.namespace
    );
    for (kind, group, year, bucket, created) in keys {
        session
            .query_unpaged(delete.clone(), (kind, group, year, bucket, created, id))
            .await
            .map_err(|err| Error::new(format!("Failed to delete entity row: {err}")))?;
    }
    Ok(())
}

/// Overwrite one text column of every stored row of an entity straight in Scylla
///
/// # Arguments
///
/// * `id` - The id of the entity to change
/// * `column` - The column to overwrite
/// * `value` - The value to store, or `None` to clear the column
async fn set_entity_text(id: uuid::Uuid, column: &str, value: Option<&str>) -> Result<(), Error> {
    // connect to Scylla on this test's runtime
    let session = get_scylla_client(&CONF).await?;
    // find the partition of each of the entity's rows
    let query = format!(
        "SELECT kind, group, year, bucket, created FROM {}.entities_by_id WHERE id = ?",
        CONF.thorium.namespace
    );
    // read the entity's rows from its id partition
    let result = session
        .query_unpaged(query, (id,))
        .await
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?;
    // enable rows on the response
    let rows = result
        .into_rows_result()
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?;
    // collect the primary key of each row
    let mut keys = Vec::new();
    for row in rows
        .rows::<(String, String, i32, i32, chrono::DateTime<chrono::Utc>)>()
        .map_err(|err| Error::new(format!("Failed to read entity rows: {err}")))?
    {
        // keep this row's primary key
        keys.push(row.map_err(|err| Error::new(format!("Failed to read entity row: {err}")))?);
    }
    // write the value to each row of the base table, which also updates the views
    let update = format!(
        "UPDATE {}.entities SET {column} = ? WHERE kind = ? AND group = ? AND year = ? AND bucket = ? AND created = ? AND id = ?",
        CONF.thorium.namespace
    );
    for (kind, group, year, bucket, created) in keys {
        session
            .query_unpaged(
                update.clone(),
                (value, kind, group, year, bucket, created, id),
            )
            .await
            .map_err(|err| Error::new(format!("Failed to write entity row: {err}")))?;
    }
    Ok(())
}

/// Overwrite the serialized kind data of every stored row of an entity straight in Scylla
///
/// Tests use this to change an entity behind the API's back, as another request's write would.
///
/// # Arguments
///
/// * `id` - The id of the entity to change
/// * `kind_data` - The serialized kind data to store
pub async fn set_entity_kind_data(id: uuid::Uuid, kind_data: &str) -> Result<(), Error> {
    // overwrite the kind data column
    set_entity_text(id, "kind_data", Some(kind_data)).await
}

/// Overwrite the image key of every stored row of an entity straight in Scylla
///
/// Tests use this to give an entity an image behind the API's back, as another request's write
/// would.
///
/// # Arguments
///
/// * `id` - The id of the entity to change
/// * `image` - The image key to store, or `None` to clear it
pub async fn set_entity_image(id: uuid::Uuid, image: Option<&str>) -> Result<(), Error> {
    // overwrite the image column
    set_entity_text(id, "image", image).await
}

/// Check whether a graphic is stored in S3, bypassing the API
///
/// Tests use this to make sure a refused write cleaned up the graphics it uploaded.
///
/// # Arguments
///
/// * `key` - The graphic's key in the graphics bucket
pub async fn graphic_exists(key: &str) -> Result<bool, Error> {
    // build an S3 client on this test's runtime
    let client = s3_client(&CONF.thorium.s3);
    // look the graphic up without downloading it
    let result = client
        .head_object()
        .bucket(&CONF.thorium.graphics.bucket)
        .key(key)
        .send()
        .await;
    // a missing graphic is a not found error
    match result {
        Ok(_) => Ok(true),
        Err(err) => match err.as_service_error() {
            Some(service) if service.is_not_found() => Ok(false),
            _ => Err(Error::new(format!(
                "Failed to check graphic {key}: {}",
                DisplayErrorContext(&err)
            ))),
        },
    }
}

/// Count the stored notifications of one type for one key straight from Scylla
///
/// # Arguments
///
/// * `kind` - The notification type
/// * `key` - The key the notifications are stored under
pub async fn notification_rows(
    kind: crate::models::NotificationType,
    key: &str,
) -> Result<usize, Error> {
    // connect to Scylla on this test's runtime
    let session = get_scylla_client(&CONF).await?;
    // count the rows in the notification's partition
    let query = format!(
        "SELECT id FROM {}.notifications WHERE kind = ? AND key = ?",
        CONF.thorium.namespace
    );
    let result = session
        .query_unpaged(query, (kind.as_str(), key))
        .await
        .map_err(|err| Error::new(format!("Failed to read notifications: {err}")))?;
    let rows = result
        .into_rows_result()
        .map_err(|err| Error::new(format!("Failed to read notifications: {err}")))?;
    Ok(rows.rows_num())
}

/// Build the key of the lock a root entity's writes hold
///
/// # Arguments
///
/// * `root` - The id of the root entity
fn instance_lock_key(root: uuid::Uuid) -> String {
    // the lock is one key per root in the test namespace
    format!("{}:entities:instance_lock:{root}", CONF.thorium.namespace)
}

/// Check whether any entity census entry exists for a group in the current year
///
/// An entity write adds its group's census entry, so a group that no entity was written to has
/// none.
///
/// # Arguments
///
/// * `group` - The group to check
pub async fn entity_census_exists(group: &str) -> Result<bool, Error> {
    // connect to Redis on this test's runtime
    let redis = get_redis_client(&CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
    // build the group's census key for this year
    let key = format!(
        "{}:census:entities:stream:{group}:{}",
        CONF.thorium.namespace,
        chrono::Datelike::year(&chrono::Utc::now())
    );
    // the key exists once an entity was written to the group
    let exists: i64 = redis::cmd("exists")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to read the census: {err}")))?;
    Ok(exists > 0)
}

/// Take the lock a root entity's writes hold, as another request holding it would
///
/// The lock expires after a minute if the test never releases it.
///
/// # Arguments
///
/// * `root` - The id of the root entity to lock
pub async fn hold_instance_lock(root: uuid::Uuid) -> Result<(), Error> {
    // connect to Redis on this test's runtime
    let redis = get_redis_client(&CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
    // take the lock only if nobody holds it
    let taken: Option<String> = redis::cmd("set")
        .arg(instance_lock_key(root))
        .arg("held-by-test")
        .arg("NX")
        .arg("PX")
        .arg(60_000)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to take the lock: {err}")))?;
    match taken {
        Some(_) => Ok(()),
        None => Err(Error::new(format!("the lock of {root} is already held"))),
    }
}

/// Take the lock a root entity's writes hold with another value and a short expiry, as a
/// request that crashed while holding it would leave it
///
/// # Arguments
///
/// * `root` - The id of the root entity to lock
/// * `value` - The value the crashed holder left in the lock
/// * `expire_ms` - How long until the lock expires, in milliseconds
pub async fn hold_instance_lock_for(
    root: uuid::Uuid,
    value: &str,
    expire_ms: u64,
) -> Result<(), Error> {
    // connect to Redis on this test's runtime
    let redis = get_redis_client(&CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
    // set the lock with its value and expiry
    let _: Option<String> = redis::cmd("set")
        .arg(instance_lock_key(root))
        .arg(value)
        .arg("PX")
        .arg(expire_ms)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to set the lock: {err}")))?;
    Ok(())
}

/// Check whether the lock a root entity's writes hold exists
///
/// # Arguments
///
/// * `root` - The id of the root entity
pub async fn instance_lock_exists(root: uuid::Uuid) -> Result<bool, Error> {
    // connect to Redis on this test's runtime
    let redis = get_redis_client(&CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
    // the lock exists while its key does
    let exists: i64 = redis::cmd("exists")
        .arg(instance_lock_key(root))
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to read the lock: {err}")))?;
    Ok(exists > 0)
}

/// Delete every Redis key under the entities prefix that names an entity, as restoring a backup
/// without Redis's state would leave it
///
/// Census keys live under their own prefix and name groups rather than entities, so none are
/// deleted. Returns how many keys were deleted.
///
/// # Arguments
///
/// * `id` - The id of the entity
pub async fn drop_entity_redis_keys(id: uuid::Uuid) -> Result<usize, Error> {
    // connect to Redis on this test's runtime
    let redis = get_redis_client(&CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
    // find every key under the entities prefix that names the entity
    let keys: Vec<String> = redis::cmd("keys")
        .arg(format!("{}:entities:*{id}*", CONF.thorium.namespace))
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to list keys: {err}")))?;
    // delete them all at once
    if !keys.is_empty() {
        let _: i64 = redis::cmd("del")
            .arg(&keys)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("Failed to delete keys: {err}")))?;
    }
    Ok(keys.len())
}

/// Release a lock taken with [`hold_instance_lock`]
///
/// # Arguments
///
/// * `root` - The id of the root entity to unlock
pub async fn release_instance_lock(root: uuid::Uuid) -> Result<(), Error> {
    // connect to Redis on this test's runtime
    let redis = get_redis_client(&CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
    // delete the lock
    let _: i64 = redis::cmd("del")
        .arg(instance_lock_key(root))
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to release the lock: {err}")))?;
    Ok(())
}

/// A write-once cell for the admin token, written to when the API is stood up for the first time
static ADMIN_TOKEN: OnceCell<String> = OnceCell::const_new();

/// The stack size of the test API's worker threads, fixed when the API is stood up
static API_STACK_SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Run this test binary's API with a chosen worker thread stack size
///
/// The test API's workers get 8 MiB stacks unless a test asks for another size before the API is
/// first stood up; a binary that wants the API's real default of 2 MiB asks for it here. Returns
/// the stack size the API's workers run with, which is another size when the API was already
/// started or another size was asked for first.
///
/// # Arguments
///
/// * `bytes` - The stack size of each worker thread, in bytes
pub fn use_api_stack_size(bytes: usize) -> usize {
    // only the first size set before the API starts is used
    *API_STACK_SIZE.get_or_init(|| bytes)
}

/// The address a config's API listens on, as a client dials it
///
/// # Arguments
///
/// * `conf` - The Thorium config the API runs with
fn addr_of(conf: &Conf) -> String {
    // a wildcard bind is not a destination, so dial loopback instead
    let host = match conf.thorium.interface.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        interface => interface,
    };
    format!("http://{}:{}", host, conf.thorium.port)
}

/// Run the API on its own thread and bootstrap an admin on it
///
/// Gives up as soon as the API's thread has exited, such as when startup refuses the schema,
/// instead of polling an address nothing will ever listen on.
///
/// # Arguments
///
/// * `conf` - The Thorium config to run the API with
/// * `attempts` - How many one-second bootstrap attempts to make while the API is running
///
/// # Panics
///
/// Panics if the runtime for the API's thread can't be built.
pub async fn start_api(conf: &Conf, attempts: u32) -> Result<AuthResponse, Error> {
    // bind the configured address when the API starts
    start_api_on(conf, attempts, None).await
}

/// Run the API on its own thread, serving on an already bound listener if one is given
///
/// # Arguments
///
/// * `conf` - The Thorium config to run the API with
/// * `attempts` - How many one-second bootstrap attempts to make while the API is running
/// * `listener` - A listener already bound to the config's interface and port, if any
///
/// # Panics
///
/// Panics if the runtime for the API's thread can't be built.
async fn start_api_on(
    conf: &Conf,
    attempts: u32,
    listener: Option<std::net::TcpListener>,
) -> Result<AuthResponse, Error> {
    // spawn the api
    let api_conf = conf.clone();
    let api = std::thread::spawn(move || {
        // create a tokio runtime
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(*API_STACK_SIZE.get_or_init(|| 8 << 20))
            .build()
            .expect("Failed to spawn tokio runtime for the test API");
        // run our api, returning the startup refusal if there is one
        rt.block_on(async move {
            crate::axum_with_listener(api_conf, StartupOpts::default(), listener).await
        })
    });
    let addr = addr_of(conf);
    // try to bootstrap until it works
    let mut tried = 0;
    // start trying to bootstrap
    let resp = loop {
        // stop at once when the API has exited
        if api.is_finished() {
            return Err(match api.join() {
                Ok(Ok(())) => Error::new("The test API exited before it was bootstrapped"),
                Ok(Err(err)) => Error::new(format!("The test API refused to start: {err}")),
                Err(_) => Error::new("The test API panicked while starting"),
            });
        }
        // start with default client settings
        let settings = ClientSettings::default();
        // boot strap an admin Thorium client with the password "password"
        let attempt = Thorium::bootstrap(
            &addr,
            "thorium",
            "password",
            "fake@fake.gov",
            &conf.thorium.secret_key,
            &settings,
        )
        .await;
        // check if our bootstrap attempt worked
        match attempt {
            Ok(resp) => break resp,
            // this attempt failed so sleep for 1 second
            Err(error) => {
                // increment our attempts by 1
                tried += 1;
                // check if we have used up all of our attempts yet
                if tried == attempts {
                    // report the address we dialed and why the last try failed, since without
                    // them every cause looks identical from five minutes away
                    return Err(Error::new(format!(
                        "Failed to bootstrap Thorium at {addr} after {tried} attempts: {error:#?}"
                    )));
                }
                // sleep for 1 second
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    };
    Ok(resp)
}

/// Bootstrap a test API, running the API on another thread and returning
/// a token for authenticating with the running API
///
/// The API's port is taken before anything is wiped, so a port that is already in use fails
/// the bootstrap at once.
async fn bootstrap_test_api() -> Result<String, Error> {
    // take the API port first so a port in use stops us before anything is wiped
    let listener = bind_api_port(&CONF)?;
    // wipe the databases, create the buckets, and apply the schema, unless the config came from
    // an override and the harness is read-only
    prepare_backends(&CONF, read_only()).await?;
    // run the api on the port we hold and bootstrap an admin on it for up to five minutes
    let resp = start_api_on(&CONF, 300, Some(listener)).await?;
    // get our auth token if we got one
    let auth_token = match resp {
        AuthResponse::Authed { token, .. } => token,
        AuthResponse::VerifyEmail(_) => {
            return Err(Error::new("Admin client needs to verify email?".to_owned()));
        }
    };
    // build our admin client
    let client = Thorium::build(ADDR.clone())
        .token(&auth_token)
        .build()
        .await
        .map_err(|err| Error::new(format!("Failed to build admin client: {err}")))?;
    // make sure Thorium is initialized
    client
        .system
        .init()
        .await
        .map_err(|err| Error::new(format!("Failed to init system: {err}")))?;
    Ok(auth_token)
}

/// Get an admin client, bootstrapping the API if needed
pub async fn admin_client() -> Result<Thorium, Error> {
    // start the API if it hasn't been started already and get a token
    let token = ADMIN_TOKEN.get_or_try_init(bootstrap_test_api).await?;
    // build our admin client
    Thorium::build(ADDR.clone())
        .token(token.clone())
        .build()
        .await
}

cfg_if::cfg_if! {
    if #[cfg(all(feature = "sync"), not(feature = "python"))] {
        use crate::ThoriumBlocking;
        use crate::RUNTIME;

        /// Get a blocking admin client, bootstrapping the API if needed
        pub fn admin_client_blocking() -> Result<ThoriumBlocking, Error> {
            // start the API if it hasn't been started already and get a token
            let token =
                RUNTIME.block_on(async { ADMIN_TOKEN.get_or_try_init(bootstrap_test_api).await })?;
            // build our admin client
            ThoriumBlocking::build(ADDR.clone())
                .token(token.clone())
                .build_blocking()
        }
    }
}
