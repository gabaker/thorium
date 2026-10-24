//! Tests that the integration test harness only wipes data that belongs to a test run

use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule,
    LifecycleRuleFilter,
};
use thorium::Error;
use thorium::test_utilities::{self, CONF};

/// Build a lifecycle with one rule expiring objects under a prefix after some days
///
/// # Arguments
///
/// * `id` - The id of the rule
/// * `prefix` - The prefix of the objects the rule expires
/// * `days` - The number of days after which objects expire
fn lifecycle(id: &str, prefix: &str, days: i32) -> BucketLifecycleConfiguration {
    // build a single expiration rule
    let rule = LifecycleRule::builder()
        .id(id)
        .filter(LifecycleRuleFilter::builder().prefix(prefix).build())
        .expiration(LifecycleExpiration::builder().days(days).build())
        .status(ExpirationStatus::Enabled)
        .build()
        .unwrap();
    BucketLifecycleConfiguration::builder()
        .rules(rule)
        .build()
        .unwrap()
}

/// Get the expiration days of every lifecycle rule on a bucket
///
/// # Arguments
///
/// * `s3` - The S3 client to use
/// * `bucket` - The bucket to read the lifecycle of
async fn expirations(s3: &aws_sdk_s3::Client, bucket: &str) -> Vec<Option<i32>> {
    // a bucket without a lifecycle has no rules
    match s3
        .get_bucket_lifecycle_configuration()
        .bucket(bucket)
        .send()
        .await
    {
        Ok(resp) => resp
            .rules()
            .iter()
            .map(|rule| rule.expiration().and_then(LifecycleExpiration::days))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Delete a bucket and any objects in it, ignoring errors
///
/// # Arguments
///
/// * `s3` - The S3 client to use
/// * `bucket` - The bucket to delete
async fn delete_bucket(s3: &aws_sdk_s3::Client, bucket: &str) {
    // the buckets these tests create hold no objects
    let _ = s3.delete_bucket().bucket(bucket).send().await;
}

/// Run a redis command against the test redis and return its result
///
/// # Arguments
///
/// * `cmd` - The command to run
async fn redis_query<T: bb8_redis::redis::FromRedisValue>(
    cmd: &bb8_redis::redis::Cmd,
) -> Result<T, Error> {
    // get a connection to the test redis
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // run our command
    cmd.query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))
}

/// Check if a keyspace exists in the test Scylla
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to look for
async fn keyspace_exists(session: &scylla::client::session::Session, keyspace: &str) -> bool {
    // ask the schema tables for this keyspace
    let rows = session
        .query_unpaged(
            "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
            (keyspace,),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    rows.rows_num() > 0
}

/// The harness refuses to wipe a namespace without the test marker before contacting any
/// database
#[tokio::test]
async fn wipe_refuses_unmarked_namespaces() {
    // point the harness at a namespace without the marker and at backends nothing listens on, so
    // any attempt to reach them fails differently
    let mut conf = CONF.clone();
    conf.thorium.namespace = "secverify_prod".to_owned();
    conf.scylla.nodes = vec!["127.0.0.1:1".to_owned()];
    conf.redis.port = 1;
    // the wipe must be refused for the namespace itself
    let refused = test_utilities::wipe_dbs(&conf).await;
    let message = refused
        .err()
        .map_or_else(String::new, |err| err.to_string());
    assert!(
        message.contains("secverify_prod") && message.contains("testing_"),
        "wiping secverify_prod wasn't refused for its name: {message:?}"
    );
}

/// Wiping a test namespace drops only its keyspace and deletes only its own Redis keys
#[tokio::test]
async fn wipe_deletes_only_its_namespace() -> Result<(), Error> {
    // use two namespaces of our own next to the run's namespace
    let doomed = format!("{}_harness", CONF.thorium.namespace);
    let neighbor = format!("{}_neighbor", CONF.thorium.namespace);
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&doomed);
    // give each namespace a keyspace and a Redis key in the same database
    let session = test_utilities::scylla_session().await?;
    for keyspace in [&doomed, &neighbor] {
        session
            .query_unpaged(
                format!(
                    "CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH replication = \
                    {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
                ),
                &(),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        let (): () = redis_query(
            bb8_redis::redis::cmd("SET")
                .arg(format!("{keyspace}:harness:key"))
                .arg("kept"),
        )
        .await?;
    }
    // wipe the doomed namespace
    test_utilities::wipe_dbs(&conf).await?;
    // only the doomed namespace's keyspace and key are gone
    let doomed_key: Option<String> =
        redis_query(bb8_redis::redis::cmd("GET").arg(format!("{doomed}:harness:key"))).await?;
    let neighbor_key: Option<String> =
        redis_query(bb8_redis::redis::cmd("GET").arg(format!("{neighbor}:harness:key"))).await?;
    let doomed_keyspace = keyspace_exists(&session, &doomed).await;
    let neighbor_keyspace = keyspace_exists(&session, &neighbor).await;
    // clean up the neighbor before checking so a failure leaves nothing behind
    let (): () =
        redis_query(bb8_redis::redis::cmd("DEL").arg(format!("{neighbor}:harness:key"))).await?;
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {neighbor}"), &())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(doomed_key, None, "the wiped namespace's key survived");
    assert!(!doomed_keyspace, "the wiped namespace's keyspace survived");
    assert_eq!(
        neighbor_key.as_deref(),
        Some("kept"),
        "another namespace's key in the same database was deleted"
    );
    assert!(
        neighbor_keyspace,
        "another namespace's keyspace was dropped"
    );
    Ok(())
}

/// The harness refuses to create or change a bucket without the test prefix
#[tokio::test]
async fn init_bucket_refuses_unprefixed_buckets() {
    // try to set up a bucket named like a real deployment's
    let s3 = test_utilities::s3_client(&CONF.thorium.s3);
    let bucket = format!("secverify-prod-{}", uuid::Uuid::new_v4().simple());
    let result = test_utilities::init_bucket(&s3, lifecycle("test", "", 1), &bucket).await;
    // check whether the bucket was created, then remove it either way
    let created = s3.head_bucket().bucket(&bucket).send().await.is_ok();
    delete_bucket(&s3, &bucket).await;
    let message = result.err().map_or_else(String::new, |err| err.to_string());
    assert!(
        message.contains(&bucket) && message.contains("testing-"),
        "setting up {bucket} wasn't refused for its name: {message:?}"
    );
    assert!(!created, "the refused bucket was created anyway");
}

/// A guard of temporary buckets deletes each bucket and the objects in it when it is dropped, and
/// refuses buckets that aren't test buckets
#[tokio::test]
async fn temporary_buckets_are_deleted_when_their_guard_drops() -> Result<(), Error> {
    // a test bucket holding an object, and a bucket outside the test prefix
    let s3 = test_utilities::s3_client(&CONF.thorium.s3);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let temporary = format!(
        "{}-guard-{}",
        CONF.thorium.namespace.replace('_', "-"),
        &suffix[..12]
    );
    let foreign = format!("secverify-guard-{}", &suffix[..12]);
    test_utilities::init_bucket(&s3, lifecycle("test", "", 1), &temporary).await?;
    s3.put_object()
        .bucket(&temporary)
        .key("held/object")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    s3.create_bucket()
        .bucket(&foreign)
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    // dropping the guard deletes the test bucket with its object, and leaves the other alone
    drop(test_utilities::TempBuckets::new(
        &CONF.thorium.s3,
        vec![temporary.clone(), foreign.clone()],
    ));
    let temporary_exists = s3.head_bucket().bucket(&temporary).send().await.is_ok();
    let foreign_exists = s3.head_bucket().bucket(&foreign).send().await.is_ok();
    // clean up what the guard should have left, so a failure leaves nothing behind
    let _ = s3.delete_bucket().bucket(&foreign).send().await;
    assert!(!temporary_exists, "the temporary bucket was kept");
    assert!(
        foreign_exists,
        "a bucket outside the test prefix was deleted"
    );
    Ok(())
}

/// The harness leaves an existing bucket's lifecycle alone and sets one only on buckets it
/// creates
#[tokio::test]
async fn init_bucket_keeps_existing_lifecycles() -> Result<(), Error> {
    // create a test bucket that already has its own 30 day rule
    let s3 = test_utilities::s3_client(&CONF.thorium.s3);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let prefix = CONF.thorium.namespace.replace('_', "-");
    let existing = format!("{prefix}-keep-{}", &suffix[..12]);
    let created = format!("{prefix}-new-{}", &suffix[..12]);
    // both buckets go away when the test ends, however it ends
    let _buckets =
        test_utilities::TempBuckets::new(&CONF.thorium.s3, vec![existing.clone(), created.clone()]);
    s3.create_bucket()
        .bucket(&existing)
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    s3.put_bucket_lifecycle_configuration()
        .bucket(&existing)
        .lifecycle_configuration(lifecycle("keep30", "keep/", 30))
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    // set up both buckets the way the harness does
    let one_day = lifecycle("test", "", 1);
    let existing_result = test_utilities::init_bucket(&s3, one_day.clone(), &existing).await;
    let created_result = test_utilities::init_bucket(&s3, one_day, &created).await;
    // read back both lifecycles
    let existing_rules = expirations(&s3, &existing).await;
    let created_rules = expirations(&s3, &created).await;
    existing_result?;
    created_result?;
    assert_eq!(
        existing_rules,
        vec![Some(30)],
        "the existing bucket's lifecycle was replaced"
    );
    assert_eq!(
        created_rules,
        vec![Some(1)],
        "the new bucket didn't get the test lifecycle"
    );
    Ok(())
}

/// A config for a namespace and five bucket names that belong to one test
///
/// # Arguments
///
/// * `suffix` - What makes the namespace and buckets unique to the test
fn isolated(suffix: &str) -> thorium::Conf {
    // copy the run's config and give it names of its own
    let mut conf = CONF.clone();
    conf.thorium.namespace = format!("{}_{suffix}", CONF.thorium.namespace);
    let unique = uuid::Uuid::new_v4().simple().to_string();
    // the buckets carry the run's namespace, so its stamp tells how old a leftover one is
    let prefix = CONF.thorium.namespace.replace('_', "-");
    let bucket =
        |role: &str| format!("{prefix}-{suffix}-{role}-{}", &unique[..8]).replace('_', "-");
    conf.thorium.files.bucket = bucket("files");
    conf.thorium.repos.bucket = bucket("repos");
    conf.thorium.results.bucket = bucket("results");
    conf.thorium.ephemeral.bucket = bucket("ephemeral");
    conf.thorium.attachments.bucket = bucket("attachments");
    conf
}

/// The five buckets a config names
///
/// # Arguments
///
/// * `conf` - The config to read the buckets of
fn buckets(conf: &thorium::Conf) -> [String; 5] {
    // the buckets the harness creates
    [
        conf.thorium.files.bucket.clone(),
        conf.thorium.repos.bucket.clone(),
        conf.thorium.results.bucket.clone(),
        conf.thorium.ephemeral.bucket.clone(),
        conf.thorium.attachments.bucket.clone(),
    ]
}

/// With `THORIUM_TEST_CONFIG` set the bootstrap drops no keyspace, deletes no Redis key, creates no
/// bucket and sets no lifecycle, even for a `testing_` namespace
#[tokio::test]
async fn a_read_only_bootstrap_changes_nothing() -> Result<(), Error> {
    // a namespace with a keyspace and a Redis key, and an existing bucket with a rule of its own
    let conf = isolated("ro");
    let keyspace = conf.thorium.namespace.clone();
    let session = test_utilities::scylla_session().await?;
    session
        .query_unpaged(
            format!(
                "CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH replication = \
                {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
            ),
            &(),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let key = format!("{keyspace}:harness:key");
    let (): () = redis_query(bb8_redis::redis::cmd("SET").arg(&key).arg("kept")).await?;
    let s3 = test_utilities::s3_client(&conf.thorium.s3);
    let existing = conf.thorium.files.bucket.clone();
    // every bucket of the config goes away when the test ends, however it ends
    let _buckets = test_utilities::TempBuckets::new(&conf.thorium.s3, buckets(&conf).to_vec());
    s3.create_bucket()
        .bucket(&existing)
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    s3.put_bucket_lifecycle_configuration()
        .bucket(&existing)
        .lifecycle_configuration(lifecycle("keep30", "keep/", 30))
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    // run the bootstrap read-only
    let prepared = test_utilities::prepare_backends(&conf, true).await;
    // read everything back, then clean up before checking
    let kept_key: Option<String> = redis_query(bb8_redis::redis::cmd("GET").arg(&key)).await?;
    let kept_keyspace = keyspace_exists(&session, &keyspace).await;
    let rules = expirations(&s3, &existing).await;
    let mut created = Vec::new();
    for bucket in buckets(&conf)
        .into_iter()
        .filter(|bucket| *bucket != existing)
    {
        created.push(s3.head_bucket().bucket(&bucket).send().await.is_ok());
    }
    let (): () = redis_query(bb8_redis::redis::cmd("DEL").arg(&key)).await?;
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), &())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    prepared?;
    assert!(
        kept_keyspace,
        "the read-only bootstrap dropped the keyspace"
    );
    assert_eq!(kept_key.as_deref(), Some("kept"), "it deleted a Redis key");
    assert_eq!(
        rules,
        vec![Some(30)],
        "it changed an existing bucket's lifecycle"
    );
    assert!(
        created.iter().all(|made| !made),
        "it created a bucket: {created:?}"
    );
    Ok(())
}

/// Without the override the bootstrap wipes the namespace, creates the buckets with the test
/// lifecycle, and applies the declared schema to the emptied keyspace
#[tokio::test]
async fn a_bootstrap_wipes_creates_and_applies_the_schema() -> Result<(), Error> {
    // a namespace that holds a stale keyspace and key
    let conf = isolated("rw");
    let keyspace = conf.thorium.namespace.clone();
    // the buckets the bootstrap creates go away when the test ends, however it ends
    let s3 = test_utilities::s3_client(&conf.thorium.s3);
    let _buckets = test_utilities::TempBuckets::new(&conf.thorium.s3, buckets(&conf).to_vec());
    let session = test_utilities::scylla_session().await?;
    session
        .query_unpaged(
            format!(
                "CREATE KEYSPACE IF NOT EXISTS {keyspace} WITH replication = \
                {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
            ),
            &(),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    session
        .query_unpaged(
            format!("CREATE TABLE {keyspace}.stale (id int PRIMARY KEY)"),
            &(),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let key = format!("{keyspace}:harness:key");
    let (): () = redis_query(bb8_redis::redis::cmd("SET").arg(&key).arg("stale")).await?;
    // run the bootstrap
    let prepared = test_utilities::prepare_backends(&conf, false).await;
    // read back what it left, then clean up before checking
    let stale_key: Option<String> = redis_query(bb8_redis::redis::cmd("GET").arg(&key)).await?;
    let tables = session
        .query_unpaged(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
            (&keyspace,),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows::<(String,)>()
        .map_err(|err| Error::new(err.to_string()))?
        .map(|row| row.map(|row| row.0))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    let mut rules = Vec::new();
    for bucket in buckets(&conf) {
        rules.push(expirations(&s3, &bucket).await);
    }
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), &())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    prepared?;
    assert_eq!(stale_key, None, "the stale key survived");
    assert!(
        !tables.contains(&"stale".to_owned()),
        "the stale table survived: {tables:?}"
    );
    assert!(
        tables.contains(&"schema_migrations".to_owned()),
        "the schema wasn't applied: {tables:?}"
    );
    assert!(rules.iter().all(|rule| *rule == vec![Some(1)]), "{rules:?}");
    Ok(())
}

/// Bootstrapping a test API on a keyspace without the schema returns the startup refusal within
/// seconds, not after the five minutes the bootstrap would otherwise poll
#[tokio::test]
async fn a_refused_schema_is_reported_at_once() {
    // a namespace nobody applied the schema to, on a port of its own
    let mut conf = isolated("refused");
    conf.scylla.setup_time = 20;
    conf.thorium.port = CONF.thorium.port + 40;
    let session = test_utilities::scylla_session().await.expect("scylla");
    session
        .query_unpaged(
            format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
            &(),
        )
        .await
        .expect("the keyspace is dropped");
    // ask for 300 attempts and expect to hear back long before they pass
    let started = std::time::Instant::now();
    let err = test_utilities::start_api(&conf, 300)
        .await
        .expect_err("the API started without a schema");
    let message = err.to_string();
    assert!(message.contains("thoradm schema apply"), "{message}");
    assert!(
        started.elapsed().as_secs() < 60,
        "the refusal took {:?}",
        started.elapsed()
    );
}
