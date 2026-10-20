//! The utlities for integration tests involving the API
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::{
    error::DisplayErrorContext,
    types::{BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule},
};
use aws_types::region::Region;
use bb8_redis::{RedisConnectionManager, bb8::Pool};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::{sync::LazyLock, time::Duration};
use tokio::sync::OnceCell;
use tokio_util::time::FutureExt;

use crate::{Conf, Error, Thorium, client::ClientSettings, conf::S3, models::AuthResponse};

/// The path to a valid Thorium config in the testing directory
///
/// This config is a placeholder, but must successfully pass validation.
/// Its values can be overridden using environment variables.
static CONF_PATH: &str = "../api/tests/thorium-testing.yml";

/// The config to use for integration tests
///
/// The config to read is hard-coded to the config in the tests directory, but that is just
/// a template. Use environment variables (e.g. `THORIUM__THORIUM__S3__ENDPOINT`) or
/// update the template manually to configure tests.
pub static CONF: LazyLock<Conf> = LazyLock::new(|| {
    Conf::new(CONF_PATH).unwrap_or_else(|_| panic!("Failed to load config at '{CONF_PATH}'"))
});

/// The addr to talk to the api at
///
/// The config's interface is the address the API *binds* to, which is usually a wildcard so it
/// answers on every interface. A wildcard is not a destination though: dialing it either fails
/// outright or, when an http proxy is configured in the environment, gets handed to a proxy that
/// cannot route it. The tests only ever talk to the API running in their own process, so rewrite
/// a wildcard bind to loopback rather than dialing it.
static ADDR: LazyLock<String> = LazyLock::new(|| {
    // pick a host we can actually connect to, since a wildcard bind is not a destination
    let host = match CONF.thorium.interface.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        interface => interface,
    };
    format!("http://{}:{}", host, CONF.thorium.port)
});

/// Build a scylla client for a specific cluster
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
#[allow(clippy::redundant_closure_for_method_calls)]
async fn get_scylla_client(config: &Conf) -> Result<Session, Error> {
    // start building our scylla client
    let mut session = SessionBuilder::new();
    // if we have auth info for scylla then add that
    if let Some(creds) = &config.scylla.auth {
        // inject our creds
        session = session.user(&creds.username, &creds.password);
    }
    // set our request timeout
    let session = session.connection_timeout(Duration::from_secs(config.scylla.setup_time.into()));
    // build a scylla session
    let scylla = Box::pin(
        config
            .scylla
            .nodes
            .iter()
            .fold(session, |builder, node| builder.known_node(node))
            .build(),
    )
    .await
    .expect("Failed to connect to scylla!");
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
/// # Arguments
///
/// * `config` - The config for the Thorium API
pub async fn get_redis_client(config: &Conf) -> Pool<RedisConnectionManager> {
    // get redis config
    let redis = &config.redis;
    // build url to server using authentication if its configured
    let url = match (&redis.username, &redis.password) {
        // redis with username/password auth setup
        (Some(user), Some(password)) => format!(
            "redis://{}:{}@{}:{}/",
            user, password, redis.host, redis.port
        ),
        (None, Some(password)) => format!(
            "redis://default:{}@{}:{}/",
            password, redis.host, redis.port
        ),
        (None, None) => format!("redis://{}:{}/", redis.host, redis.port),
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

/// A write-once cell for the admin token, written to when the API is stood up for the first time
static ADMIN_TOKEN: OnceCell<String> = OnceCell::const_new();

/// Bootstrap a test API, running the API on another thread and returning
/// a token for authenticating with the running API
async fn bootstrap_test_api() -> Result<String, Error> {
    // wipe our databases
    wipe_dbs(&CONF).await?;
    // initiate s3 buckets
    init_s3_buckets(&CONF.thorium.s3, &CONF).await?;
    // spawn the api
    std::thread::spawn(move || {
        // create a tokio runtime
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 << 20)
            .build()
            .expect("Failed to spawn tokio runtime for the test API");
        // spawn our api
        rt.block_on(async move { crate::axum(CONF.clone()).await });
    });
    // try to bootstrap for 60 seconds until it works
    let mut attempts = 0;
    // start trying to bootstrap
    let resp = loop {
        // start with default client settings
        let settings = ClientSettings::default();
        // boot strap an admin Thorium client with the password "password"
        let attempt = Thorium::bootstrap(
            &ADDR,
            "thorium",
            "password",
            "fake@fake.gov",
            &CONF.thorium.secret_key,
            &settings,
        )
        .await;
        // check if our bootstrap attempt worked
        match attempt {
            Ok(resp) => break resp,
            // this attempt failed so sleep for 1 second
            Err(error) => {
                // increment our attempts by 1
                attempts += 1;
                // check if we have used up all of our attempts yet
                if attempts == 300 {
                    // report the address we dialed and why the last try failed, since without
                    // them every cause looks identical from five minutes away
                    return Err(Error::new(format!(
                        "Failed to bootstrap Thorium at {} after {attempts} attempts: {error:#?}",
                        *ADDR
                    )));
                }
                // sleep for 1 second
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    };
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
