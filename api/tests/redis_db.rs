//! Tests that a test run stays inside the Redis database it selects
//!
//! This is its own test binary so nothing else starts the test API before the database is seeded.
//!
//! The test also writes to a neighboring database, which must not be in use by anything else
//! while it runs. It picks an empty database other than its own, or the one set with
//! `THORIUM_TEST_NEIGHBOR_DB`.

use futures::FutureExt;
use redis::AsyncCommands;
use std::panic::AssertUnwindSafe;
use thorium::Error;
use thorium::conf::Conf;
use thorium::test_utilities::{self, CONF};

/// The number of databases a Redis server has unless it's configured otherwise
const DEFAULT_DATABASES: u32 = 16;

/// Get a connection to one Redis database
///
/// # Arguments
///
/// * `conf` - The config whose Redis database to connect to
async fn connect(conf: &Conf) -> redis::aio::MultiplexedConnection {
    // reuse the harness's pool so the connection selects the configured database
    let pool = test_utilities::get_redis_client(conf).await;
    // clone the multiplexed connection out of the pool so it outlives the pool
    let pooled = pool.get().await.expect("a Redis connection");
    pooled.clone()
}

/// Pick the neighboring database this test writes to, which must be empty
async fn neighbor_conf() -> Conf {
    // use the database the environment names if it names one
    let named = std::env::var("THORIUM_TEST_NEIGHBOR_DB").ok().map(|db| {
        db.parse::<u32>()
            .expect("THORIUM_TEST_NEIGHBOR_DB is a number")
    });
    // otherwise try every database a default Redis has, highest first
    let candidates = match named {
        Some(db) => vec![db],
        None => (0..DEFAULT_DATABASES).rev().collect(),
    };
    // take the first empty database that isn't ours
    for db in candidates.into_iter().filter(|db| *db != CONF.redis.db) {
        // point a copy of our config at this database
        let mut conf = CONF.clone();
        conf.redis.db = db;
        // count the keys this database holds
        let size: u64 = redis::cmd("DBSIZE")
            .query_async(&mut connect(&conf).await)
            .await
            .expect("the size of a neighboring database");
        // an empty database is one no other run is using
        if size == 0 {
            return conf;
        }
    }
    // every candidate holds data, so there is no safe neighbor
    panic!("no empty Redis database is free to check against; set THORIUM_TEST_NEIGHBOR_DB");
}

/// List the keys in a database under this run's namespace
///
/// # Arguments
///
/// * `conn` - A connection to the database to list
async fn namespace_keys(conn: &mut redis::aio::MultiplexedConnection) -> Vec<String> {
    // match only keys under our namespace, since another namespace may share the database
    let pattern = format!("{}:*", CONF.thorium.namespace);
    let mut keys = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        // get the next batch of matching keys
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(conn)
            .await
            .expect("scanning a database");
        keys.extend(batch);
        // stop once the scan wraps around
        if next == 0 {
            break;
        }
        cursor = next;
    }
    // sort the keys so they compare in a fixed order
    keys.sort();
    keys
}

/// Seed both databases, start the test API, and check only this run's database changed
///
/// # Arguments
///
/// * `run` - A connection to this run's database
/// * `neighbor` - A connection to the neighboring database
/// * `key` - The key to seed into both databases
async fn check_isolation(
    run: &mut redis::aio::MultiplexedConnection,
    neighbor: &mut redis::aio::MultiplexedConnection,
    key: &str,
) -> Result<(), Error> {
    // seed the same key into both databases
    let () = run
        .set(key, "run")
        .await
        .expect("seeding this run's database");
    let () = neighbor
        .set(key, "other")
        .await
        .expect("seeding the other database");
    // start the test API, which empties this run's database and creates the admin
    test_utilities::admin_client().await?;
    // the key in this run's database was flushed
    let flushed: Option<String> = run.get(key).await.expect("reading this run's database");
    assert_eq!(flushed, None, "this run's database wasn't flushed");
    // the key in the other database is untouched
    let kept: Option<String> = neighbor.get(key).await.expect("reading the other database");
    assert_eq!(
        kept.as_deref(),
        Some("other"),
        "the other database was flushed"
    );
    // the admin landed in this run's database
    let users_key = format!("{}:users", CONF.thorium.namespace);
    let run_users: Vec<String> = run.smembers(&users_key).await.expect("users");
    assert!(
        run_users.contains(&"thorium".to_owned()),
        "the admin isn't in this run's database"
    );
    // the API wrote nothing under our namespace to the other database
    assert_eq!(
        namespace_keys(neighbor).await,
        vec![key.to_owned()],
        "the API wrote to the other database"
    );
    Ok(())
}

/// Starting a test run empties only its own Redis database, and the API writes only to it
#[tokio::test]
async fn run_stays_in_its_own_database() -> Result<(), Error> {
    // connect to our database and an empty neighboring one
    let other_conf = neighbor_conf().await;
    let mut run = connect(&CONF).await;
    let mut neighbor = connect(&other_conf).await;
    // seed a key unique to this run
    let key = format!(
        "{}:redis_db_test:{}",
        CONF.thorium.namespace,
        uuid::Uuid::new_v4()
    );
    // run the checks, catching a failure so the seeded key is always removed
    let outcome = AssertUnwindSafe(check_isolation(&mut run, &mut neighbor, &key))
        .catch_unwind()
        .await;
    let () = neighbor.del(&key).await.expect("removing the seeded key");
    // report the checks' outcome now that the neighbor is clean
    match outcome {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
