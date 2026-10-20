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

/// The harness leaves an existing bucket's lifecycle alone and sets one only on buckets it
/// creates
#[tokio::test]
async fn init_bucket_keeps_existing_lifecycles() -> Result<(), Error> {
    // create a test bucket that already has its own 30 day rule
    let s3 = test_utilities::s3_client(&CONF.thorium.s3);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let existing = format!("testing-harness-keep-{}", &suffix[..12]);
    let created = format!("testing-harness-new-{}", &suffix[..12]);
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
    // read back both lifecycles, then remove the buckets
    let existing_rules = expirations(&s3, &existing).await;
    let created_rules = expirations(&s3, &created).await;
    delete_bucket(&s3, &existing).await;
    delete_bucket(&s3, &created).await;
    // report a failed setup only now, so the buckets are removed either way
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
