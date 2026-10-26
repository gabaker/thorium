//! Tests of the `drain-reactions` check driven in-process against Redis: it waits while a reaction
//! is in flight or a stage-path worker is registered, skips when nothing is, and never writes

mod common;

use common::migrate::World;
use thoradm::migrate::registry::MIGRATIONS;
use thorium::models::schema::{RunMode, RunOptions, check_pass};

/// The migration's id
const ID: &str = "drain-reactions";

/// Remove every key a test namespace holds, so a rerun of a test starts empty
///
/// # Arguments
///
/// * `world` - The test's world
async fn clear_redis(world: &World) {
    // the namespace is unique to the test, so everything under it is the test's
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    for key in keys(world).await {
        let _: () = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .expect("a key deletes");
    }
}

/// List every key of the test's namespace, sorted
///
/// # Arguments
///
/// * `world` - The test's world
async fn keys(world: &World) -> Vec<String> {
    // the namespace is unique to the test
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let mut keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{}:*", world.ns))
        .query_async(&mut *conn)
        .await
        .expect("keys list");
    keys.sort();
    keys
}

/// Store a reaction's data hash with a status, the way the API stores it
///
/// # Arguments
///
/// * `world` - The test's world
/// * `id` - The reaction's id
/// * `status` - The reaction's status
async fn seed_reaction(world: &World, id: &str, status: &str) {
    // the status is a JSON string in the hash
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = redis::cmd("HSET")
        .arg(format!("{}:reaction_data:corn:{id}", world.ns))
        .arg("status")
        .arg(format!("\"{status}\""))
        .query_async(&mut *conn)
        .await
        .expect("a reaction is stored");
}

/// Store a worker's data hash, with a tool instance when it was registered by the tool path
///
/// # Arguments
///
/// * `world` - The test's world
/// * `name` - The worker's name
/// * `tool_instance` - The worker's tool instance, when it has one
async fn seed_worker(world: &World, name: &str, tool_instance: Option<&str>) {
    // a worker of the previous release has no tool instance field
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let key = format!("{}:worker_data:{name}", world.ns);
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg("name")
        .arg(name)
        .query_async(&mut *conn)
        .await
        .expect("a worker is stored");
    if let Some(instance) = tool_instance {
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("tool_instance")
            .arg(instance)
            .query_async(&mut *conn)
            .await
            .expect("a tool instance is stored");
    }
}

/// Run the drain's check
///
/// # Arguments
///
/// * `world` - The test's world
async fn check(world: &World) -> thorium::models::schema::CheckRun {
    // a check with one slice
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.id == ID)
        .expect("the drain is registered");
    let ctx = world.ctx(RunMode::Check, 10);
    check_pass(
        migration.runner,
        &World::spec(migration),
        &RunOptions::default(),
        &ctx,
    )
    .await
    .expect("the check runs")
}

/// Reactions that are `Created` or `Started`, and a worker without a tool instance, make the check
/// wait with their counts and nothing else, and the check writes nothing
#[tokio::test]
async fn in_flight_work_makes_the_check_wait() {
    // two unfinished reactions, one finished, and a stage-path worker
    let world = World::new("dr01").await;
    clear_redis(&world).await;
    seed_reaction(&world, "r1", "Created").await;
    seed_reaction(&world, "r2", "Started").await;
    seed_reaction(&world, "r3", "Completed").await;
    seed_worker(&world, "w1", None).await;
    let before = keys(&world).await;
    let run = check(&world).await;
    // the wait names the counts and the delete command, and no id or group
    let wait = run.report.wait.expect("the check waits");
    assert!(wait.reason.contains("2 reaction(s)"), "{}", wait.reason);
    assert!(
        wait.reason.contains("1 stage-path worker(s)"),
        "{}",
        wait.reason
    );
    assert!(
        wait.reason.contains("thorctl reactions delete"),
        "{}",
        wait.reason
    );
    assert!(!wait.reason.contains("r1") && !wait.reason.contains("corn"));
    assert_eq!(wait.retry_after_secs, 15);
    // nothing was classified or written
    assert_eq!(run.report.counts.migratable, 0);
    assert_eq!(keys(&world).await, before);
}

/// Finished reactions and workers of the tool path don't make the check wait
#[tokio::test]
async fn finished_work_lets_the_step_skip() {
    // finished reactions of every kind of ending, and a worker of the tool path
    let world = World::new("dr02").await;
    clear_redis(&world).await;
    seed_reaction(&world, "r1", "Completed").await;
    seed_reaction(&world, "r2", "Failed").await;
    seed_worker(&world, "w1", Some("3b8e7d14-52a6-4c0f-9e73-a1c2b4d5e608")).await;
    let run = check(&world).await;
    // there is nothing to wait for and nothing to migrate
    assert!(run.report.wait.is_none());
    assert_eq!(run.report.counts.migratable, 0);
    // a namespace with no reactions at all is the same
    clear_redis(&world).await;
    assert!(check(&world).await.report.wait.is_none());
}
