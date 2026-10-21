//! Tests that starting the test API refuses to disturb another run or deployment
//!
//! This is its own test binary because the test here expects starting the test API to fail, so
//! nothing else may start it first.

use redis::AsyncCommands;
use std::time::{Duration, Instant};
use thorium::test_utilities::{self, CONF};

/// Seed a key into this run's Redis database
///
/// # Arguments
///
/// * `conn` - A connection to this run's Redis database
/// * `key` - The key to seed
async fn seed(conn: &mut redis::aio::MultiplexedConnection, key: &str) {
    // write a marker value the tests check survives
    let () = conn
        .set(key, "kept")
        .await
        .expect("seeding this run's database");
}

/// Open a connection to this run's Redis database
async fn connect() -> redis::aio::MultiplexedConnection {
    // reuse the harness's pool so the connection selects the configured database
    let pool = test_utilities::get_redis_client(&CONF).await;
    // clone the multiplexed connection out of the pool so it outlives the pool
    let pooled = pool
        .get()
        .await
        .expect("a connection to this run's database");
    pooled.clone()
}

/// Starting the test API on a port another process holds fails fast and leaves the databases alone
#[tokio::test]
async fn bootstrap_refuses_a_port_in_use() {
    // hold the API port like another run's API would
    let held = std::net::TcpListener::bind((CONF.thorium.interface.as_str(), CONF.thorium.port))
        .expect("binding the test API port");
    // seed a key under our namespace that a flush would delete
    let mut conn = connect().await;
    let key = format!(
        "{}:bootstrap_guard:{}",
        CONF.thorium.namespace,
        uuid::Uuid::new_v4()
    );
    seed(&mut conn, &key).await;
    // starting the test API fails without waiting on the other process
    let start = Instant::now();
    let err = test_utilities::admin_client()
        .await
        .err()
        .expect("the test API started on a port another process holds");
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "refusing a port in use took {:?}",
        start.elapsed()
    );
    assert!(format!("{err:?}").contains("in use"), "{err:?}");
    // our database wasn't flushed
    let kept: Option<String> = conn.get(&key).await.expect("reading the seeded key");
    let () = conn.del(&key).await.expect("removing the seeded key");
    assert_eq!(kept.as_deref(), Some("kept"), "the databases were wiped");
    drop(held);
}
