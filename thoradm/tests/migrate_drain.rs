//! Tests of the `drain-reactions` check driven in-process and through the CLI against Redis and
//! Scylla: it waits while a reaction is in flight or a stage-path worker is registered, skips when
//! nothing is, names what holds it only on stderr, and never writes

mod common;

use common::migrate::World;
use serde_json::Value;
use thoradm::migrate::registry::MIGRATIONS;
use thorium::models::schema::{RunMode, RunOptions, SchemaObject, apply_objects, check_pass};

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

/// Create the declared `nodes` table in the test's keyspace, which says what nodes are registered
///
/// # Arguments
///
/// * `world` - The test's world
async fn create_nodes_table(world: &World) {
    // only the nodes table, as the schema declares it
    let objects = SchemaObject::declared()
        .into_iter()
        .filter(|object| object.name == "nodes")
        .collect::<Vec<_>>();
    assert_eq!(objects.len(), 1, "the nodes table is declared");
    apply_objects(world.session(), &world.conf, &objects, false, "", &|_| ())
        .await
        .expect("the nodes table applies");
}

/// Register a node the way the API does, as a row of the `nodes` table
///
/// # Arguments
///
/// * `world` - The test's world
/// * `cluster` - The node's cluster
/// * `node` - The node's name
async fn register_node(world: &World, cluster: &str, node: &str) {
    // the health and resources don't matter to the drain
    world
        .session()
        .query_unpaged(
            format!(
                "INSERT INTO {}.nodes (cluster, node, health, resources) VALUES (?, ?, ?, ?)",
                world.ns
            ),
            (cluster, node, "\"Healthy\"", "{}"),
        )
        .await
        .expect("a node registers");
}

/// A reaction as the previous release stores it, for the named report
struct SeededReaction<'a> {
    /// The reaction's id
    id: &'a str,
    /// The reaction's group
    group: &'a str,
    /// The reaction's pipeline
    pipeline: &'a str,
    /// The reaction's status
    status: &'a str,
    /// The reaction's SLA deadline
    sla: &'a str,
    /// Whether the reaction's id is in the status set for its status
    listed: bool,
    /// The timestamps of the reaction's status log entries, oldest first
    logs: &'a [&'a str],
}

/// Store a reaction's data hash, status set entry, and status logs the way the API stores them
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction to store
async fn seed_named_reaction(world: &World, reaction: &SeededReaction<'_>) {
    // the data hash holds the id, group, and pipeline as is and the status and SLA as JSON
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let ns = &world.ns;
    let SeededReaction {
        id,
        group,
        pipeline,
        status,
        ..
    } = reaction;
    let _: () = redis::cmd("HSET")
        .arg(format!("{ns}:reaction_data:{group}:{id}"))
        .arg("id")
        .arg(id)
        .arg("group")
        .arg(group)
        .arg("pipeline")
        .arg(pipeline)
        .arg("creator")
        .arg("mcarson")
        .arg("status")
        .arg(format!("\"{status}\""))
        .arg("sla")
        .arg(format!("\"{}\"", reaction.sla))
        .query_async(&mut *conn)
        .await
        .expect("a reaction is stored");
    // a listed reaction is in the pipeline's status set for its status
    if reaction.listed {
        let _: () = redis::cmd("SADD")
            .arg(format!("{ns}:reactions:{group}:{pipeline}:{status}"))
            .arg(id)
            .query_async(&mut *conn)
            .await
            .expect("the status set is written");
    }
    // each status log entry is a JSON status update pushed onto the reaction's log list
    for timestamp in reaction.logs {
        let update = serde_json::json!({
            "group": group,
            "pipeline": pipeline,
            "reaction": id,
            "action": "ReactionCreated",
            "timestamp": timestamp,
            "msg": null,
            "update": {},
        });
        let _: () = redis::cmd("RPUSH")
            .arg(format!("{ns}:logs:{group}:{id}"))
            .arg(update.to_string())
            .query_async(&mut *conn)
            .await
            .expect("a status log is pushed");
    }
}

/// Store a worker registration of the previous release, which has no tool instance
///
/// # Arguments
///
/// * `world` - The test's world
/// * `name` - The worker's name
/// * `cluster` - The worker's cluster
/// * `node` - The worker's node
/// * `heart_beat` - The worker's last heartbeat, if it sent one
async fn seed_stage_worker(
    world: &World,
    name: &str,
    cluster: &str,
    node: &str,
    heart_beat: Option<&str>,
) {
    // the scaler, status, and heartbeat are JSON in the hash
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let heart_beat = heart_beat.map_or_else(|| "null".to_owned(), |beat| format!("\"{beat}\""));
    let _: () = redis::cmd("HSET")
        .arg(format!("{}:worker_data:{name}", world.ns))
        .arg("name")
        .arg(name)
        .arg("cluster")
        .arg(cluster)
        .arg("node")
        .arg(node)
        .arg("scaler")
        .arg("\"K8s\"")
        .arg("status")
        .arg("\"Running\"")
        .arg("heart_beat")
        .arg(heart_beat)
        .arg("group")
        .arg("corn")
        .query_async(&mut *conn)
        .await
        .expect("a worker is stored");
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
    create_nodes_table(&world).await;
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
    world.drop_keyspace().await;
}

/// Finished reactions and workers of the tool path don't make the check wait
#[tokio::test]
async fn finished_work_lets_the_step_skip() {
    // finished reactions of every kind of ending, and a worker of the tool path
    let world = World::new("dr02").await;
    create_nodes_table(&world).await;
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
    world.drop_keyspace().await;
}

/// The rows of a stderr table whose first column is `first`
///
/// # Arguments
///
/// * `stderr` - The check's stderr
/// * `first` - The value of the row's first column
fn row<'a>(stderr: &'a str, first: &str) -> Vec<&'a str> {
    // a row starts with its first column and splits on runs of spaces
    let line = stderr
        .lines()
        .find(|line| line.split_whitespace().next() == Some(first))
        .unwrap_or_else(|| panic!("no row for {first} in:\n{stderr}"));
    line.split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect()
}

/// The id of the listed in-flight reaction the named report test seeds
const LISTED: &str = "0b6d1c3e-6f2a-4c55-9a51-2f4c3e8d1a77";

/// The id of the orphan reaction the named report test seeds
const ORPHAN: &str = "5e0f8a90-1d3b-4f0e-8c6a-7b2d9e4f3c10";

/// The id of the finished reaction the named report test seeds
const FINISHED: &str = "9a1f2b3c-4d5e-4f60-8a7b-9c0d1e2f3a4b";

/// Seed what the named report test reads: a listed started reaction with status logs, an orphan
/// created one without, a finished one, a stage-path worker on a registered node, one on a node
/// that isn't, and a tool-path worker
///
/// # Arguments
///
/// * `world` - The test's world
async fn seed_holders(world: &World) {
    // the reactions, in flight or finished
    seed_named_reaction(
        world,
        &SeededReaction {
            id: LISTED,
            group: "corn",
            pipeline: "unpack",
            status: "Started",
            sla: "2026-11-11T07:00:00Z",
            listed: true,
            logs: &["2026-11-04T07:00:00Z", "2026-11-04T10:02:11Z"],
        },
    )
    .await;
    seed_named_reaction(
        world,
        &SeededReaction {
            id: ORPHAN,
            group: "wheat",
            pipeline: "strings",
            status: "Created",
            sla: "2026-10-29T08:00:03Z",
            listed: false,
            logs: &[],
        },
    )
    .await;
    seed_named_reaction(
        world,
        &SeededReaction {
            id: FINISHED,
            group: "corn",
            pipeline: "unpack",
            status: "Completed",
            sla: "2026-11-11T07:00:00Z",
            listed: true,
            logs: &["2026-11-04T07:00:00Z"],
        },
    )
    .await;
    // a stage-path worker on a registered node, one on a node that isn't, and a tool-path worker
    register_node(world, "prod", "node-3").await;
    seed_stage_worker(
        world,
        "corn-unpack-7f9c2",
        "prod",
        "node-3",
        Some("2026-11-04T10:02:40Z"),
    )
    .await;
    seed_stage_worker(world, "corn-unpack-a41d0", "prod", "node-9", None).await;
    seed_worker(
        world,
        "corn-tool-b2c3d",
        Some("3b8e7d14-52a6-4c0f-9e73-a1c2b4d5e608"),
    )
    .await;
}

/// The check names each blocking reaction and stage-path worker on stderr, marking the orphan
/// and the worker on an unregistered node, while its stdout report and wait name counts only
#[tokio::test]
async fn the_check_names_orphans_and_unregistered_nodes() {
    // reactions and workers that hold the drain, and ones that don't
    let world = World::new("dr03").await;
    create_nodes_table(&world).await;
    clear_redis(&world).await;
    seed_holders(&world).await;
    let before = keys(&world).await;
    // the check exits 0, the same as any check that asks to wait
    let output = World::run(world.thoradm().args(["migrate", "check", "--id", ID]));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // show the output in a run with --nocapture
    print!("{stderr}{stdout}");
    // the summary and both tables are on stderr
    assert!(
        stderr.contains(
            "drain-reactions waits: 2 reaction(s) Created or Started, 2 stage-path worker(s) \
             registered"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("Reactions holding the drain"), "{stderr}");
    assert!(stderr.contains("Stage-path workers"), "{stderr}");
    for header in [
        "ID",
        "GROUP",
        "PIPELINE",
        "STATUS",
        "AGE",
        "LAST ACTIVITY",
        "LISTED",
    ] {
        assert!(row(&stderr, "ID").contains(&header), "{header}: {stderr}");
    }
    for header in [
        "NAME",
        "SCALER",
        "CLUSTER",
        "NODE",
        "STATUS",
        "HEARTBEAT",
        "NODE REGISTERED",
    ] {
        assert!(row(&stderr, "NAME").contains(&header), "{header}: {stderr}");
    }
    // the listed reaction's age comes from its first log and its activity from its newest
    let cells = row(&stderr, LISTED);
    assert_eq!(cells[1..4], ["corn", "unpack", "Started"], "{cells:?}");
    assert!(
        cells[4].ends_with('m') || cells[4].ends_with('h'),
        "{cells:?}"
    );
    assert_eq!(cells[5], "2026-11-04T10:02:11Z", "{cells:?}");
    assert_eq!(cells[6], "yes", "{cells:?}");
    // the orphan has no log, so its SLA stands in and it is marked unlisted
    let cells = row(&stderr, ORPHAN);
    assert_eq!(cells[1..4], ["wheat", "strings", "Created"], "{cells:?}");
    assert_eq!(cells[4], "-", "{cells:?}");
    assert_eq!(cells[5], "2026-10-29T08:00:03Z (sla)", "{cells:?}");
    assert_eq!(cells[6], "no (orphan)", "{cells:?}");
    // the finished reaction and the tool-path worker hold nothing
    assert!(!stderr.contains(FINISHED), "{stderr}");
    assert!(!stderr.contains("corn-tool-b2c3d"), "{stderr}");
    // each stage-path worker is named with whether its node is registered
    let cells = row(&stderr, "corn-unpack-7f9c2");
    assert_eq!(
        cells[1..],
        [
            "K8s",
            "prod",
            "node-3",
            "Running",
            "2026-11-04T10:02:40Z",
            "yes"
        ],
        "{cells:?}"
    );
    let cells = row(&stderr, "corn-unpack-a41d0");
    assert_eq!(
        cells[1..],
        ["K8s", "prod", "node-9", "Running", "-", "no"],
        "{cells:?}"
    );
    // the stdout report, which is also the stored result, names counts and the command only
    let report: Value =
        serde_json::from_str(stdout.lines().last().unwrap_or("null")).expect("stdout ends in JSON");
    let reason = report["wait"]["reason"].as_str().expect("a wait reason");
    assert!(reason.starts_with("2 reaction(s) are still Created or Started and 2 stage-path"));
    assert!(
        reason.contains("thoradm migrate check --id drain-reactions"),
        "{reason}"
    );
    for named in [
        LISTED,
        ORPHAN,
        "corn",
        "wheat",
        "unpack",
        "strings",
        "corn-unpack-7f9c2",
        "node-9",
    ] {
        assert!(!stdout.contains(named), "{named} in {stdout}");
    }
    // the check wrote nothing to Redis
    assert_eq!(keys(&world).await, before);
    world.drop_keyspace().await;
}

/// Bytes that aren't UTF-8, which a read of the field as text refuses
const NOT_TEXT: &[u8] = b"\xff\xfe-not-text";

/// The check reads only the status of a finished reaction, and of an in-flight one only the fields
/// its row shows, never its arguments, samples, or other large fields
#[tokio::test]
async fn the_check_reads_only_the_fields_it_shows() {
    // an in-flight reaction and a finished one, whose unread fields can't be read as text
    let world = World::new("dr04").await;
    create_nodes_table(&world).await;
    clear_redis(&world).await;
    seed_holders(&world).await;
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let ns = &world.ns;
    let _: () = redis::cmd("HSET")
        .arg(format!("{ns}:reaction_data:corn:{LISTED}"))
        .arg("args")
        .arg(NOT_TEXT)
        .arg("samples")
        .arg(NOT_TEXT)
        .query_async(&mut *conn)
        .await
        .expect("the in-flight reaction's args are stored");
    let _: () = redis::cmd("HSET")
        .arg(format!("{ns}:reaction_data:corn:{FINISHED}"))
        .arg("id")
        .arg(NOT_TEXT)
        .arg("pipeline")
        .arg(NOT_TEXT)
        .arg("sla")
        .arg(NOT_TEXT)
        .arg("args")
        .arg(NOT_TEXT)
        .query_async(&mut *conn)
        .await
        .expect("the finished reaction's fields are stored");
    drop(conn);
    // the check still reads both in-flight reactions and both stage-path workers
    let wait = check(&world).await.report.wait.expect("the check waits");
    assert!(
        wait.reason
            .starts_with("2 reaction(s) are still Created or Started and 2 stage-path"),
        "{}",
        wait.reason
    );
    world.drop_keyspace().await;
}
