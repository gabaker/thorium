//! Tests of the `drain-reactions` migration driven in-process and through the CLI against Redis
//! and Scylla: its check waits while a reaction is in flight or a stage-path worker is registered,
//! skips when nothing is, names what holds it only on stderr, and never writes; its run records
//! each in-flight reaction's request, fails the reaction, cleans its jobs' indexes, and removes
//! the stage-path workers' registrations, reporting counts and the worker list only

mod common;

use chrono::{DateTime, Duration, Utc};
use common::migrate::World;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thoradm::migrate::{CancelFault, arm_cancel_fault, set_result_uid};
use thorium::models::ReactionStatus;
use thorium::models::schema::drain_requests::{
    ARGS_TTL_SECS, CreatorMembership, DrainRequest, DrainRequestRow, DrainRequestStore,
    DrainStatus, MAX_ARGS_BYTES, PipelineIdentity, SERVICE_USERS, args_sha256,
};
use thorium::models::schema::{
    DrainCancelReport, DrainWorker, MAX_DRAIN_WORKERS, MigrateError, PassOutcome, RunFlags,
    RunMode, RunOptions, SchemaObject, apply_objects, check_pass, lease_key, read_job_result,
    run_pass,
};
use uuid::Uuid;

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

/// The revision whose drain cancels reactions
const V04: &str = "2026-10-v04";

/// The `at` of the drain step's `Running` entry the run is given
const ATTEMPT: &str = "2026-11-05T01:00:00Z";

/// The `at` of an earlier drain attempt's `Running` entry
const EARLIER: &str = "2026-11-04T01:00:00Z";

/// The group the cancel tests' reactions run in
const GROUP: &str = "corn";

/// The pipeline the cancel tests' reactions run
const PIPELINE: &str = "unpack";

/// The user who created the pipeline
const PIPELINE_CREATOR: &str = "pcreator";

/// A normal user in the group's `users` role who created most reactions
const CREATOR: &str = "mcarson";

/// The pipeline's stage order, as the previous release stores it
const ORDER: &str = r#"[["plant"],["harvest"]]"#;

/// A value in every reaction's arguments that no output may hold
const SECRET: &str = "drain-secret-arg-5c1e";

/// A sample the reactions ran on
const SAMPLE: &str = "6f1ed002ab5595859014ebf0951522d9f2f2e4d1e4b6d3b0a3f6a9a5b1c2d3e4";

/// The stage-path worker most cancel tests register
const WORKER: &str = "corn-unpack-7f9c2";

/// The migration's registry entry
fn def() -> &'static MigrationDef {
    // the entry thoradm runs
    MIGRATIONS
        .iter()
        .find(|migration| migration.id == ID)
        .expect("the drain is registered")
}

/// Render a value as JSON, the way the previous release serializes hash fields
///
/// # Arguments
///
/// * `value` - The value to render
fn json<T: Serialize + ?Sized>(value: &T) -> String {
    // every value the tests store renders
    serde_json::to_string(value).expect("the value renders")
}

/// Run one Redis command against the test's namespace
///
/// # Arguments
///
/// * `world` - The test's world
/// * `cmd` - The command to run
async fn query<T: redis::FromRedisValue>(world: &World, cmd: &redis::Cmd) -> T {
    // the commands are the tests' own, so a failure fails the test
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    cmd.query_async(&mut *conn)
        .await
        .expect("a Redis command runs")
}

/// Run a Redis pipeline against the test's namespace
///
/// # Arguments
///
/// * `world` - The test's world
/// * `pipe` - The pipeline to run
async fn exec(world: &World, pipe: &redis::Pipeline) {
    // the commands are the tests' own, so a failure fails the test
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = pipe
        .query_async(&mut *conn)
        .await
        .expect("a Redis pipeline runs");
}

/// What a key holds, as sorted text: a set's or sorted set's members, a list's entries, a hash's
/// `field=value` pairs, or a string's value
///
/// # Arguments
///
/// * `world` - The test's world
/// * `key` - The key to read
async fn contents(world: &World, key: &str) -> Vec<String> {
    // read the key the way its type is read
    let kind: String = query(world, redis::cmd("TYPE").arg(key)).await;
    let mut values: Vec<String> = match kind.as_str() {
        "set" => query(world, redis::cmd("SMEMBERS").arg(key)).await,
        "zset" => query(world, redis::cmd("ZRANGE").arg(key).arg(0).arg(-1)).await,
        "list" => query(world, redis::cmd("LRANGE").arg(key).arg(0).arg(-1)).await,
        "hash" => {
            let map: BTreeMap<String, String> = query(world, redis::cmd("HGETALL").arg(key)).await;
            map.into_iter()
                .map(|(field, value)| format!("{field}={value}"))
                .collect()
        }
        _ => vec![query(world, redis::cmd("GET").arg(key)).await],
    };
    // sorted, so two reads compare equal whatever the order Redis returns
    values.sort();
    values
}

/// Every key of the test's namespace with what it holds, leaving out the writer lease a run holds
///
/// # Arguments
///
/// * `world` - The test's world
async fn dump(world: &World) -> BTreeMap<String, Vec<String>> {
    // the lease comes and goes with each run
    let lease = lease_key(&world.ns);
    let mut dumped = BTreeMap::new();
    for key in keys(world).await {
        if key != lease {
            let held = contents(world, &key).await;
            dumped.insert(key, held);
        }
    }
    dumped
}

/// Store a group with its roles the way the API stores it: in the group set, with its settings,
/// and each role's members in both its combined and direct sets
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The group's name
/// * `roles` - Each role (`owners`, `managers`, `users`, `monitors`) and its members
async fn seed_group(world: &World, group: &str, roles: &[(&str, &[&str])]) {
    // the group set and the settings hash
    let ns = &world.ns;
    let mut pipe = redis::pipe();
    pipe.cmd("SADD")
        .arg(format!("{ns}:groups"))
        .arg(group)
        .cmd("HSET")
        .arg(format!("{ns}:groups_data:{group}"))
        .arg("allowed")
        .arg(json(&thorium::models::GroupAllowed::default()));
    // each member is a direct member, so it is in the combined set as well
    for (role, members) in roles {
        for member in *members {
            pipe.cmd("SADD")
                .arg(format!("{ns}:groups_combined:{group}:{role}"))
                .arg(member)
                .cmd("SADD")
                .arg(format!("{ns}:groups_direct:{group}:{role}"))
                .arg(member);
        }
    }
    exec(world, &pipe).await;
}

/// Store a pipeline the way the previous release stores it, with no stage tools
///
/// # Arguments
///
/// * `world` - The test's world
async fn seed_pipeline(world: &World) {
    // the data hash and the group's pipeline set
    let ns = &world.ns;
    let mut pipe = redis::pipe();
    pipe.cmd("HSET")
        .arg(format!("{ns}:pipeline_data:{GROUP}:{PIPELINE}"))
        .arg("group")
        .arg(GROUP)
        .arg("name")
        .arg(PIPELINE)
        .arg("creator")
        .arg(PIPELINE_CREATOR)
        .arg("order")
        .arg(ORDER)
        .arg("sla")
        .arg(604_800)
        .arg("triggers")
        .arg("{}")
        .arg("bans")
        .arg("{}")
        .cmd("SADD")
        .arg(format!("{ns}:pipelines:{GROUP}"))
        .arg(PIPELINE);
    exec(world, &pipe).await;
}

/// Seed what every cancel test needs: the group, with the creator in its `users` role, and the
/// pipeline the reactions run
///
/// # Arguments
///
/// * `world` - The test's world
async fn seed_scene(world: &World) {
    // a group whose owner is someone else, and the pipeline
    create_nodes_table(world).await;
    clear_redis(world).await;
    seed_group(
        world,
        GROUP,
        &[("owners", &["gowner"]), ("users", &[CREATOR])],
    )
    .await;
    seed_pipeline(world).await;
}

/// The identity the cancel records for the seeded pipeline
fn identity() -> PipelineIdentity {
    // the order's sha256 is over its compact JSON
    PipelineIdentity {
        group: GROUP.to_owned(),
        name: PIPELINE.to_owned(),
        creator: PIPELINE_CREATOR.to_owned(),
        order_sha256: args_sha256(ORDER),
    }
}

/// A reaction's argument JSON as the previous release stores it, holding `value` as a positional
///
/// # Arguments
///
/// * `value` - The positional argument of the reaction's first stage
fn args_json(value: &str) -> String {
    // the stage's generic job arguments with every field the previous release serializes
    serde_json::json!({
        "plant": {
            "positionals": [value],
            "kwargs": {},
            "switches": [],
            "opts": {
                "override_positionals": false,
                "override_kwargs": false,
                "override_cmd": null,
            },
        },
    })
    .to_string()
}

/// A reaction as the previous release stores it
#[derive(Clone)]
struct OldReaction {
    /// The reaction's id
    id: String,
    /// The user who created it
    creator: String,
    /// Its status, as the status sets name it
    status: &'static str,
    /// Its raw argument JSON
    args: String,
    /// Its parent's id, if it is a sub reaction
    parent: Option<String>,
    /// How many sub reactions it has
    sub_reactions: u64,
    /// Whether it is in its pipeline's status set for its status
    listed: bool,
    /// The timestamp of its one status log entry, if it has one
    last_log: Option<DateTime<Utc>>,
    /// Its SLA deadline
    sla: DateTime<Utc>,
    /// The samples it runs on
    samples: Vec<String>,
    /// Its tags
    tags: Vec<String>,
}

impl OldReaction {
    /// A listed reaction in the scene's group and pipeline, active an hour ago, with arguments
    /// holding the secret
    ///
    /// # Arguments
    ///
    /// * `creator` - The user who created it
    /// * `status` - Its status
    fn new(creator: &str, status: &'static str) -> Self {
        // a fresh id and a deadline a day out
        let now = Utc::now();
        OldReaction {
            id: Uuid::new_v4().to_string(),
            creator: creator.to_owned(),
            status,
            args: args_json(SECRET),
            parent: None,
            sub_reactions: 0,
            listed: true,
            last_log: Some(now - Duration::hours(1)),
            sla: now + Duration::days(1),
            samples: vec![SAMPLE.to_owned()],
            tags: vec!["harvest".to_owned()],
        }
    }

    /// The key of the reaction's data hash
    ///
    /// # Arguments
    ///
    /// * `world` - The test's world
    fn data(&self, world: &World) -> String {
        // every reaction is in the scene's group
        format!("{}:reaction_data:{GROUP}:{}", world.ns, self.id)
    }

    /// The key of the pipeline's status set for a status
    ///
    /// # Arguments
    ///
    /// * `world` - The test's world
    /// * `status` - The status
    fn status_set(world: &World, status: &str) -> String {
        // every reaction runs the scene's pipeline
        format!("{}:reactions:{GROUP}:{PIPELINE}:{status}", world.ns)
    }
}

/// Store a reaction the way the previous release's create does: its data hash (the fields its
/// build writes), its pipeline set, group sorted set, status set when listed, status log, sub
/// reaction sets when it has a parent, and tag sets
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction to store
async fn seed_old_reaction(world: &World, reaction: &OldReaction) {
    // the data hash, with the status, SLA, and lists as JSON
    let ns = &world.ns;
    let id = &reaction.id;
    let data = reaction.data(world);
    let mut pipe = redis::pipe();
    pipe.cmd("HSET")
        .arg(&data)
        .arg("id")
        .arg(id)
        .arg("group")
        .arg(GROUP)
        .arg("pipeline")
        .arg(PIPELINE)
        .arg("creator")
        .arg(&reaction.creator)
        .arg("status")
        .arg(json(reaction.status))
        .arg("current_stage")
        .arg(0)
        .arg("current_stage_progress")
        .arg(0)
        .arg("current_stage_length")
        .arg(1)
        .arg("args")
        .arg(&reaction.args)
        .arg("sla")
        .arg(json(&reaction.sla))
        .arg("tags")
        .arg(json(&reaction.tags))
        .arg("sub_reactions")
        .arg(reaction.sub_reactions)
        .arg("completed_sub_reactions")
        .arg(0)
        .arg("samples")
        .arg(json(&reaction.samples))
        .arg("ephemeral")
        .arg("[]")
        .arg("parent_ephemeral")
        .arg("{}")
        .arg("repos")
        .arg("[]");
    // the pipeline's set and the group's sorted set always hold it
    pipe.cmd("SADD")
        .arg(format!("{ns}:reactions:{GROUP}:{PIPELINE}"))
        .arg(id)
        .cmd("ZADD")
        .arg(format!("{ns}:reactions_group:{GROUP}:{}", reaction.status))
        .arg(reaction.sla.timestamp())
        .arg(id);
    // only a listed reaction is in its status set
    if reaction.listed {
        pipe.cmd("SADD")
            .arg(OldReaction::status_set(world, reaction.status))
            .arg(id);
    }
    // its status log entry is a JSON status update
    if let Some(at) = reaction.last_log {
        let update = serde_json::json!({
            "group": GROUP,
            "pipeline": PIPELINE,
            "reaction": id,
            "action": "ReactionCreated",
            "timestamp": at,
            "msg": null,
            "update": {},
        });
        pipe.cmd("RPUSH")
            .arg(format!("{ns}:logs:{GROUP}:{id}"))
            .arg(update.to_string());
    }
    // a sub reaction names its parent and is in the parent's sub reaction sets
    if let Some(parent) = &reaction.parent {
        pipe.cmd("HSET")
            .arg(&data)
            .arg("parent")
            .arg(json(parent))
            .cmd("SADD")
            .arg(format!("{ns}:sub_reactions:{GROUP}:{parent}"))
            .arg(id)
            .cmd("SADD")
            .arg(format!(
                "{ns}:sub_reactions:{GROUP}:{parent}:{}",
                reaction.status
            ))
            .arg(id);
    }
    // each tag's set holds it
    for tag in &reaction.tags {
        pipe.cmd("SADD")
            .arg(format!("{ns}:reaction_tags:{GROUP}:{tag}"))
            .arg(id);
    }
    exec(world, &pipe).await;
}

/// A job of the previous release, which has no tool instance
struct OldJob<'a> {
    /// The job's id
    id: String,
    /// The reaction the job is part of
    reaction: &'a OldReaction,
    /// The job's stage
    stage: &'a str,
    /// The job's status, `Created` or `Running`
    status: &'a str,
    /// The worker running it, if it is running
    worker: Option<&'a str>,
}

impl<'a> OldJob<'a> {
    /// A job of a reaction's first stage
    ///
    /// # Arguments
    ///
    /// * `reaction` - The reaction the job is part of
    /// * `worker` - The worker running it, which makes it `Running`, or nothing for `Created`
    fn new(reaction: &'a OldReaction, worker: Option<&'a str>) -> Self {
        // a running job was popped from its created queue into its running queue
        OldJob {
            id: Uuid::new_v4().to_string(),
            reaction,
            stage: "plant",
            status: if worker.is_some() {
                "Running"
            } else {
                "Created"
            },
            worker,
        }
    }

    /// The keys only this job holds: its data hash, its status queue (it is the only member),
    /// and its stage's log key
    ///
    /// # Arguments
    ///
    /// * `world` - The test's world
    fn keys(&self, world: &World) -> Vec<String> {
        // the queue is the previous release's, named by pipeline and stage
        let ns = &world.ns;
        vec![
            format!("{ns}:job_data:{}", self.id),
            format!(
                "{ns}:job_queue:{GROUP}:{PIPELINE}:{}:{}:{}",
                self.stage, self.reaction.creator, self.status
            ),
            format!("{ns}:stage_logs:{}:{}", self.reaction.id, self.stage),
        ]
    }
}

/// Store a job the way the previous release does: its data hash, its reaction's job set, its
/// status queue entry, its scaler's deadline entry, a running job's running entry, and a stage
/// log key
///
/// # Arguments
///
/// * `world` - The test's world
/// * `job` - The job to store
async fn seed_old_job(world: &World, job: &OldJob<'_>) {
    // the data hash, with the scaler, status, deadline, and worker as JSON
    let ns = &world.ns;
    let reaction = job.reaction;
    let deadline = reaction.sla.timestamp();
    let keys = job.keys(world);
    let mut pipe = redis::pipe();
    pipe.cmd("HSET")
        .arg(&keys[0])
        .arg("reaction")
        .arg(&reaction.id)
        .arg("id")
        .arg(&job.id)
        .arg("group")
        .arg(GROUP)
        .arg("pipeline")
        .arg(PIPELINE)
        .arg("stage")
        .arg(job.stage)
        .arg("creator")
        .arg(&reaction.creator)
        .arg("scaler")
        .arg("\"K8s\"")
        .arg("args")
        .arg(
            serde_json::json!({"positionals": [SECRET], "kwargs": {}, "switches": [], "opts": {}})
                .to_string(),
        )
        .arg("status")
        .arg(json(job.status))
        .arg("deadline")
        .arg(json(&reaction.sla))
        .arg("worker")
        .arg(json(&job.worker))
        .arg("generator")
        .arg("false")
        .arg("samples")
        .arg(json(&reaction.samples));
    // the reaction's job set, the status queue's claim entry, and the scaler's deadline entry,
    // which the previous release formats with no tool instance
    pipe.cmd("SADD")
        .arg(format!("{ns}:reaction_jobs:{GROUP}:{}", reaction.id))
        .arg(&job.id)
        .cmd("ZADD")
        .arg(&keys[1])
        .arg(deadline)
        .arg(format!(
            "{{\"job\":\"{}\",\"reaction\":\"{}\"}}",
            job.id, reaction.id
        ))
        .cmd("ZADD")
        .arg(format!("{ns}:streams:system:K8s:deadlines"))
        .arg(deadline)
        .arg(format!(
            "{{\"group\":\"{GROUP}\",\"pipeline\":\"{PIPELINE}\",\"stage\":\"{}\",\
             \"creator\":\"{}\",\"job_id\":\"{}\",\"reaction\":\"{}\"}}",
            job.stage, reaction.creator, job.id, reaction.id
        ));
    // a running job has its worker's running entry
    if let Some(worker) = job.worker {
        pipe.cmd("ZADD")
            .arg(format!("{ns}:streams:system:K8s:running"))
            .arg(Utc::now().timestamp())
            .arg(format!(
                "{{\"job_id\":\"{}\",\"worker\":\"{worker}\"}}",
                job.id
            ));
    }
    // the stage's logs in the key the previous release kept them in
    pipe.cmd("RPUSH").arg(&keys[2]).arg("planting");
    exec(world, &pipe).await;
}

/// Register stage-path workers the way the previous release does: a data hash each, and the
/// cluster, node, and scaler's worker set
///
/// # Arguments
///
/// * `world` - The test's world
/// * `names` - The workers' names
/// * `cluster` - Their cluster
/// * `node` - Their node
async fn seed_registered_workers(world: &World, names: &[String], cluster: &str, node: &str) {
    // one pipeline for every worker, so thousands register quickly
    let ns = &world.ns;
    let mut pipe = redis::pipe();
    for name in names {
        pipe.cmd("HSET")
            .arg(format!("{ns}:worker_data:{name}"))
            .arg("cluster")
            .arg(cluster)
            .arg("node")
            .arg(node)
            .arg("scaler")
            .arg("\"K8s\"")
            .arg("name")
            .arg(name)
            .arg("user")
            .arg(CREATOR)
            .arg("pipeline")
            .arg(PIPELINE)
            .arg("group")
            .arg(GROUP)
            .arg("stage")
            .arg("plant")
            .arg("status")
            .arg("\"Running\"")
            .arg("spawned")
            .arg(json(&Utc::now()))
            .arg("heart_beat")
            .arg("null")
            .arg("resources")
            .arg(r#"{"cpu":1000,"memory":1024,"ephemeral_storage":0,"worker_slots":1,"nvidia_gpu":0,"amd_gpu":0}"#)
            .arg("pool")
            .arg("\"Deadline\"")
            .cmd("SADD")
            .arg(format!("{ns}:workers:{cluster}:{node}:K8s"))
            .arg(name);
    }
    exec(world, &pipe).await;
}

/// The worker entry the cancel reports for a registered worker of the scene
///
/// # Arguments
///
/// * `name` - The worker's name
/// * `cluster` - Its cluster
/// * `node` - Its node
fn drain_worker(name: &str, cluster: &str, node: &str) -> DrainWorker {
    // every seeded worker runs for the scene's group under K8s
    DrainWorker {
        group: GROUP.to_owned(),
        name: name.to_owned(),
        scaler: "K8s".to_owned(),
        cluster: cluster.to_owned(),
        node: node.to_owned(),
    }
}

/// The flags the operator gives the cancel's Job: the drain step's attempt
fn flags() -> RunFlags {
    // no cap override
    RunFlags {
        step_attempt: Some(ATTEMPT.to_owned()),
        ..RunFlags::default()
    }
}

/// Run the drain's cancel in one slice with the writer lease, released after
///
/// # Arguments
///
/// * `world` - The test's world
/// * `flags` - The run's flags
/// * `batch_rows` - The records of each batch
async fn run_with(
    world: &World,
    flags: RunFlags,
    batch_rows: usize,
) -> Result<PassOutcome, MigrateError> {
    // the registry's spec and a context carrying the flags
    let spec = World::spec(def());
    let lease = world.lease("drain-test").await;
    let mut ctx = world.ctx(RunMode::Run, batch_rows);
    ctx.flags = flags;
    let options = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let outcome = run_pass(def().runner, &spec, &options, &ctx, &world.store, &lease).await;
    lease.release().await;
    outcome
}

/// Run the cancel with the drain step's attempt, failing the test if the pass fails
///
/// # Arguments
///
/// * `world` - The test's world
async fn run_ok(world: &World) -> PassOutcome {
    // a pass that fails fails the test
    let outcome = run_with(world, flags(), 100)
        .await
        .expect("the cancel runs");
    assert_eq!(outcome.failure, None, "the cancel failed");
    outcome
}

/// Why a pass failed, whether it ended in an error or in an outcome naming its failure
///
/// # Arguments
///
/// * `result` - What the run returned
fn failure_of(result: &Result<PassOutcome, MigrateError>) -> Option<String> {
    // either way is a failure
    match result {
        Ok(outcome) => outcome.failure.clone(),
        Err(err) => Some(err.to_string()),
    }
}

/// The cancel's report from a pass
///
/// # Arguments
///
/// * `outcome` - The pass's outcome
fn drained(outcome: &PassOutcome) -> &DrainCancelReport {
    // a run of the cancel always reports
    outcome
        .report
        .drain
        .as_ref()
        .expect("the cancel reports what it did")
}

/// Prepare the statements on drain request rows
///
/// # Arguments
///
/// * `world` - The test's world
async fn store(world: &World) -> DrainRequestStore {
    // over the test's keyspace
    DrainRequestStore::prepare(world.backends.scylla.clone(), &world.ns)
        .await
        .expect("the drain statements prepare")
}

/// Read a reaction's drain request row, which must exist
///
/// # Arguments
///
/// * `world` - The test's world
/// * `id` - The reaction's id
async fn request_row(world: &World, id: &str) -> DrainRequestRow {
    // every row a test reads was recorded
    store(world)
        .await
        .get(id)
        .await
        .expect("the row reads")
        .unwrap_or_else(|| panic!("no drain request row for {id}"))
}

/// Read a reaction's args row
///
/// # Arguments
///
/// * `world` - The test's world
/// * `id` - The reaction's id
async fn args_row(world: &World, id: &str) -> Option<String> {
    // the args row is apart from the request row
    store(world)
        .await
        .get_args(id)
        .await
        .expect("the args row reads")
}

/// The TTL left on a `schema_migrations` row's `value`, if it has one
///
/// # Arguments
///
/// * `world` - The test's world
/// * `kind` - The row's kind
/// * `id` - The row's id
async fn value_ttl(world: &World, kind: &str, id: &str) -> Option<i32> {
    // a row written without a TTL reads as null
    let rows = world
        .session()
        .query_unpaged(
            format!(
                "SELECT TTL(value) FROM {}.schema_migrations WHERE kind = ? AND id = ?",
                world.ns
            ),
            (kind, id),
        )
        .await
        .expect("the TTL reads")
        .into_rows_result()
        .expect("rows");
    rows.maybe_first_row::<(Option<i32>,)>()
        .expect("a typed row")
        .and_then(|(ttl,)| ttl)
}

/// Every args row's id and the TTL left on its value
///
/// # Arguments
///
/// * `world` - The test's world
async fn args_ttls(world: &World) -> Vec<(String, Option<i32>)> {
    // the args kind is one partition
    let rows = world
        .session()
        .query_unpaged(
            format!(
                "SELECT id, TTL(value) FROM {}.schema_migrations \
                 WHERE kind = 'drain_request_args'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the args rows read")
        .into_rows_result()
        .expect("rows");
    rows.rows::<(String, Option<i32>)>()
        .expect("typed rows")
        .map(|row| row.expect("a row"))
        .collect()
}

/// Assert a TTL was set by a write within the last ten minutes with the args TTL
///
/// # Arguments
///
/// * `ttl` - The TTL left
/// * `what` - What the TTL is on, for the message
fn assert_fresh_ttl(ttl: Option<i32>, what: &str) {
    // the args TTL less the test's own run time
    let ttl = ttl.unwrap_or_else(|| panic!("{what} has no TTL"));
    assert!(
        ttl > ARGS_TTL_SECS - 600 && ttl <= ARGS_TTL_SECS,
        "{what} TTL {ttl}"
    );
}

/// A reaction's stored status, as JSON
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction
async fn status_of(world: &World, reaction: &OldReaction) -> Option<String> {
    // the status field of the data hash
    query(
        world,
        redis::cmd("HGET").arg(reaction.data(world)).arg("status"),
    )
    .await
}

/// A reaction's newest status log entry
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction
async fn last_log(world: &World, reaction: &OldReaction) -> String {
    // the log list's last entry
    let entry: Option<String> = query(
        world,
        redis::cmd("LINDEX")
            .arg(format!("{}:logs:{GROUP}:{}", world.ns, reaction.id))
            .arg(-1),
    )
    .await;
    entry.unwrap_or_default()
}

/// Assert the cancel failed a reaction: its status is `Failed`, it moved from its status set to
/// the failed one, and its newest log names the upgrade
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction
async fn assert_cancelled(world: &World, reaction: &OldReaction) {
    // the data hash and the status sets
    assert_eq!(
        status_of(world, reaction).await.as_deref(),
        Some("\"Failed\""),
        "{} isn't failed",
        reaction.id
    );
    let failed: bool = query(
        world,
        redis::cmd("SISMEMBER")
            .arg(OldReaction::status_set(world, "Failed"))
            .arg(&reaction.id),
    )
    .await;
    assert!(failed, "{} isn't in the failed set", reaction.id);
    let before: bool = query(
        world,
        redis::cmd("SISMEMBER")
            .arg(OldReaction::status_set(world, reaction.status))
            .arg(&reaction.id),
    )
    .await;
    assert!(!before, "{} is still in its old status set", reaction.id);
    // the failure's log message says why
    let log = last_log(world, reaction).await;
    assert!(
        log.contains("cancelled by the upgrade to 2026-10-v04"),
        "{log}"
    );
}

/// A request as an earlier cancel run recorded it for a reaction of the scene
///
/// # Arguments
///
/// * `reaction` - The reaction
/// * `tag` - A tag that marks the request as the earlier run's
fn earlier_request(reaction: &OldReaction, tag: &str) -> DrainRequest {
    // the request as the earlier run saw the reaction, with a marking tag
    DrainRequest {
        group: GROUP.to_owned(),
        pipeline: PIPELINE.to_owned(),
        samples: reaction.samples.clone(),
        repos: Vec::new(),
        tags: vec![tag.to_owned()],
        parent: None,
        trigger_depth: None,
        creator: reaction.creator.clone(),
        old_status: ReactionStatus::Started,
        last_activity: reaction.last_log,
        orphan: false,
        args_sha256: Some(args_sha256(&reaction.args)),
        args_bytes: u64::try_from(reaction.args.len()).expect("a small size"),
        buffers: false,
        cache: false,
        identity: identity(),
        membership: CreatorMembership {
            held_role: true,
            role: "user".to_owned(),
        },
        binding: None,
    }
}

/// Record a request the way an earlier cancel run did, with no args row
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction
/// * `tag` - A tag that marks the request as the earlier run's
async fn seed_earlier_row(world: &World, reaction: &OldReaction, tag: &str) -> DrainRequestRow {
    // a recorded row of the earlier attempt
    let now = Utc::now();
    let row = DrainRequestRow {
        id: reaction.id.clone(),
        value: earlier_request(reaction, tag)
            .to_json()
            .expect("the request renders"),
        attempt: Some(EARLIER.to_owned()),
        status: DrainStatus::Recorded,
        entry: None,
        result: None,
        revision: Some(V04.to_owned()),
        started: Some(now),
        finished: None,
        updated: Some(now),
    };
    let inserted = store(world)
        .await
        .insert_if_absent(&row)
        .await
        .expect("the earlier row inserts");
    assert!(inserted, "the earlier row is new");
    row
}

/// Assert a reaction's request is recorded by this run: with the step's attempt and the
/// revision, the reaction's group, pipeline, creator, status, parent, samples, and tags, the
/// pipeline's identity, the creator's `users` role, and its args' hash, with the args themselves
/// in their own row
///
/// # Arguments
///
/// * `world` - The test's world
/// * `reaction` - The reaction
/// * `old_status` - The status the reaction had when it was cancelled
/// * `parent` - The reaction's parent's id, if it is a sub reaction
async fn assert_recorded(
    world: &World,
    reaction: &OldReaction,
    old_status: &ReactionStatus,
    parent: Option<&str>,
) {
    // the row is recorded by this attempt and isn't finished
    let row = request_row(world, &reaction.id).await;
    assert_eq!(row.status, DrainStatus::Recorded);
    assert_eq!(row.attempt.as_deref(), Some(ATTEMPT));
    assert_eq!(row.revision.as_deref(), Some(V04));
    assert_eq!(row.entry, None);
    assert!(row.started.is_some() && row.finished.is_none(), "{row:?}");
    assert!(!row.value.contains(SECRET), "the request holds the args");
    // the request is the reaction as it was
    let request = row.request().expect("the request parses");
    assert_eq!(request.group, GROUP);
    assert_eq!(request.pipeline, PIPELINE);
    assert_eq!(request.creator, reaction.creator);
    assert_eq!(&request.old_status, old_status);
    assert_eq!(request.parent.map(|id| id.to_string()).as_deref(), parent);
    assert_eq!(request.samples, reaction.samples);
    assert_eq!(request.tags, reaction.tags);
    assert!(!request.orphan);
    assert_eq!(request.args_sha256, Some(args_sha256(&reaction.args)));
    assert_eq!(request.args_bytes, reaction.args.len() as u64);
    assert_eq!(request.identity, identity());
    assert_eq!(
        request.membership,
        CreatorMembership {
            held_role: true,
            role: "user".to_owned()
        }
    );
    assert_eq!(request.binding, None);
    // the args are in their own row, exactly as the reaction stored them
    assert_eq!(
        args_row(world, &reaction.id).await,
        Some(reaction.args.clone())
    );
}

/// A run cancels every in-flight reaction of the previous release, children before parents:
/// it records each request with the step's attempt and its args apart, fails the reaction,
/// removes its jobs from every index the previous release kept them in, and removes the
/// stage-path workers' registrations, while finished reactions and tool-path workers are untouched
#[tokio::test]
async fn a_run_cancels_records_and_cleans_every_index() {
    // a started parent with a running job, its created child with a created job, a finished
    // reaction, a stage-path worker running the parent's job, and a tool-path worker
    let world = World::new_full("dr05").await;
    seed_scene(&world).await;
    let parent = OldReaction {
        sub_reactions: 1,
        ..OldReaction::new(CREATOR, "Started")
    };
    let child = OldReaction {
        parent: Some(parent.id.clone()),
        tags: Vec::new(),
        ..OldReaction::new(CREATOR, "Created")
    };
    let finished = OldReaction::new(CREATOR, "Completed");
    for reaction in [&parent, &child, &finished] {
        seed_old_reaction(&world, reaction).await;
    }
    let running = OldJob::new(&parent, Some(WORKER));
    let created = OldJob::new(&child, None);
    seed_old_job(&world, &running).await;
    seed_old_job(&world, &created).await;
    seed_registered_workers(&world, &[WORKER.to_owned()], "prod", "node-3").await;
    seed_worker(
        &world,
        "corn-tool-b2c3d",
        Some("3b8e7d14-52a6-4c0f-9e73-a1c2b4d5e608"),
    )
    .await;
    let before = dump(&world).await;
    let outcome = run_ok(&world).await;
    // the report counts both reactions and lists the one worker
    let report = drained(&outcome);
    assert_eq!(
        (
            report.cancelled,
            report.recorded,
            report.refused,
            report.stale,
            report.kept
        ),
        (2, 2, 0, 0, 0),
        "{report:?}"
    );
    assert_eq!(report.workers_removed, 1);
    assert_eq!(report.workers_not_listed, 0);
    assert_eq!(report.workers, vec![drain_worker(WORKER, "prod", "node-3")]);
    // each request is recorded with the attempt, the pipeline's identity, the creator's role,
    // and its args' hash, with the args themselves in their own row
    for (reaction, old_status, parent_id) in [
        (&parent, ReactionStatus::Started, None),
        (&child, ReactionStatus::Created, Some(parent.id.clone())),
    ] {
        assert_recorded(&world, reaction, &old_status, parent_id.as_deref()).await;
        assert_cancelled(&world, reaction).await;
    }
    let after = dump(&world).await;
    // the finished reaction and the tool-path worker are as they were
    assert_eq!(
        after.get(&finished.data(&world)),
        before.get(&finished.data(&world))
    );
    let tool = format!("{}:worker_data:corn-tool-b2c3d", world.ns);
    assert_eq!(after.get(&tool), before.get(&tool));
    // every key that held only the jobs or the worker is gone
    let ns = &world.ns;
    let mut gone = running.keys(&world);
    gone.extend(created.keys(&world));
    gone.extend([
        format!("{ns}:streams:system:K8s:deadlines"),
        format!("{ns}:streams:system:K8s:running"),
        format!("{ns}:worker_data:{WORKER}"),
        format!("{ns}:workers:prod:node-3:K8s"),
    ]);
    for key in &gone {
        assert!(before.contains_key(key), "{key} wasn't seeded");
        assert!(
            !after.contains_key(key),
            "{key} is left: {:?}",
            after.get(key)
        );
    }
    // no index left names either job, outside the failed reactions' own job sets and logs
    for (key, held) in &after {
        if key.contains(":reaction_jobs:") || key.contains(":logs:") {
            continue;
        }
        for job in [&running.id, &created.id] {
            assert!(!key.contains(job.as_str()), "{key} names a job");
            assert!(
                !held.iter().any(|value| value.contains(job.as_str())),
                "{key} holds job {job}: {held:?}"
            );
        }
    }
    // the only new keys are the failed sets and the expire list; failing the parent last
    // proceeded nothing, so no job was created
    for key in after.keys().filter(|key| !before.contains_key(*key)) {
        assert!(
            key.contains(":Failed") || key.ends_with(":expire"),
            "unexpected new key {key}"
        );
        assert!(!key.contains(":job_data:"), "a new job: {key}");
    }
    world.drop_keyspace().await;
}

/// A cancel interrupted after a reaction's request is recorded and before the reaction is failed
/// leaves the row and the reaction's jobs in place, and a rerun keeps the row and finishes the
/// cancel
#[tokio::test]
async fn an_interrupted_cancel_finishes_on_rerun() {
    // a started reaction with a created job, whose cancel fails once after its record
    let world = World::new_full("dr06").await;
    seed_scene(&world).await;
    let reaction = OldReaction::new(CREATOR, "Started");
    seed_old_reaction(&world, &reaction).await;
    let job = OldJob::new(&reaction, None);
    seed_old_job(&world, &job).await;
    arm_cancel_fault(&world.ns, &reaction.id, CancelFault::AfterRecord);
    let first = run_with(&world, flags(), 100).await;
    assert!(
        failure_of(&first).is_some(),
        "the armed fault didn't stop the run"
    );
    // the request is recorded, but the reaction still runs and its job is still queued
    let recorded = request_row(&world, &reaction.id).await;
    assert_eq!(recorded.status, DrainStatus::Recorded);
    assert_eq!(recorded.attempt.as_deref(), Some(ATTEMPT));
    assert_eq!(
        status_of(&world, &reaction).await.as_deref(),
        Some("\"Started\"")
    );
    let keys_now = keys(&world).await;
    for key in job.keys(&world) {
        assert!(
            keys_now.contains(&key),
            "{key} went before the reaction failed"
        );
    }
    // the rerun keeps the row, fails the reaction, and cleans the job
    let second = run_ok(&world).await;
    let report = drained(&second);
    assert_eq!(
        (report.cancelled, report.kept, report.recorded),
        (1, 1, 0),
        "{report:?}"
    );
    let kept = request_row(&world, &reaction.id).await;
    assert_eq!(
        (&kept.value, &kept.attempt, kept.status),
        (&recorded.value, &recorded.attempt, recorded.status)
    );
    assert_cancelled(&world, &reaction).await;
    let keys_now = keys(&world).await;
    for key in job.keys(&world) {
        assert!(!keys_now.contains(&key), "{key} is left");
    }
    assert_eq!(
        args_row(&world, &reaction.id).await,
        Some(reaction.args.clone())
    );
    world.drop_keyspace().await;
}

/// A cancel that failed partway and its retry under the same step attempt, as the operator runs
/// them, count every request of the attempt in the retry's report: the one it cancelled again and
/// the one whose reaction the failed run had already failed, but no other attempt's request
#[tokio::test]
async fn a_retried_cancel_counts_the_failed_runs_requests() {
    // a started parent and its created child, cancelled child first, with the cancel failing
    // once after the parent's record, and a finished reaction an earlier attempt recorded
    let world = World::new_full("dr19").await;
    seed_scene(&world).await;
    let parent = OldReaction {
        sub_reactions: 1,
        ..OldReaction::new(CREATOR, "Started")
    };
    let child = OldReaction {
        parent: Some(parent.id.clone()),
        tags: Vec::new(),
        ..OldReaction::new(CREATOR, "Created")
    };
    let finished = OldReaction::new(CREATOR, "Completed");
    for reaction in [&parent, &child, &finished] {
        seed_old_reaction(&world, reaction).await;
    }
    seed_earlier_row(&world, &finished, "earlier-attempt").await;
    arm_cancel_fault(&world.ns, &parent.id, CancelFault::AfterRecord);
    let first = run_with(&world, flags(), 100).await;
    assert!(
        failure_of(&first).is_some(),
        "the armed fault didn't stop the run"
    );
    // the failed run recorded both requests and failed only the child
    for reaction in [&parent, &child] {
        let row = request_row(&world, &reaction.id).await;
        assert_eq!(row.status, DrainStatus::Recorded);
        assert_eq!(row.attempt.as_deref(), Some(ATTEMPT));
    }
    assert_cancelled(&world, &child).await;
    // the retry cancels the parent and counts both requests as kept from the failed run
    let second = run_ok(&world).await;
    let report = drained(&second);
    assert_eq!(
        (report.cancelled, report.recorded, report.kept),
        (1, 0, 2),
        "{report:?}"
    );
    assert_cancelled(&world, &parent).await;
    world.drop_keyspace().await;
}

/// A cancel run whose reaction already has a request row, as when an earlier run's lease was
/// broken, keeps that row's value and attempt and still fails the reaction
#[tokio::test]
async fn a_record_insert_keeps_an_existing_row() {
    // a started reaction an earlier attempt already recorded
    let world = World::new_full("dr07").await;
    seed_scene(&world).await;
    let reaction = OldReaction::new(CREATOR, "Started");
    seed_old_reaction(&world, &reaction).await;
    let earlier = seed_earlier_row(&world, &reaction, "earlier-run").await;
    let outcome = run_ok(&world).await;
    // the earlier row is kept as it was
    let report = drained(&outcome);
    assert_eq!(
        (report.cancelled, report.kept, report.recorded),
        (1, 1, 0),
        "{report:?}"
    );
    let kept = request_row(&world, &reaction.id).await;
    assert_eq!(kept.attempt.as_deref(), Some(EARLIER));
    assert_eq!(kept.value, earlier.value);
    assert_eq!(kept.status, DrainStatus::Recorded);
    // the reaction is cancelled all the same, and its args, whose hash matches the row, are kept
    assert_cancelled(&world, &reaction).await;
    assert_eq!(
        args_row(&world, &reaction.id).await,
        Some(reaction.args.clone())
    );
    world.drop_keyspace().await;
}

/// Every args row the cancel writes carries the args TTL, for a new request and for one an
/// earlier run recorded without writing its args
#[tokio::test]
async fn args_rows_are_always_written_with_a_ttl() {
    // a new reaction, and one whose earlier record lost its args row
    let world = World::new_full("dr08").await;
    seed_scene(&world).await;
    let new = OldReaction::new(CREATOR, "Started");
    let earlier = OldReaction::new(CREATOR, "Created");
    seed_old_reaction(&world, &new).await;
    seed_old_reaction(&world, &earlier).await;
    seed_earlier_row(&world, &earlier, "earlier-run").await;
    run_ok(&world).await;
    // both args rows exist, each with a fresh TTL
    let ttls = args_ttls(&world).await;
    let ids = ttls
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from([new.id.clone(), earlier.id.clone()]));
    for (id, ttl) in ttls {
        assert_fresh_ttl(ttl, &format!("args row {id}"));
    }
    world.drop_keyspace().await;
}

/// The args are written to their own row with a TTL, exactly as the reaction stored them, while
/// the request row holds only their hash and size and carries no TTL
#[tokio::test]
async fn args_are_written_with_a_ttl_apart_from_the_request() {
    // one in-flight reaction with args
    let world = World::new_full("dr09").await;
    seed_scene(&world).await;
    let reaction = OldReaction::new(CREATOR, "Started");
    seed_old_reaction(&world, &reaction).await;
    run_ok(&world).await;
    // the request row names the args only by hash and size, and never expires
    let row = request_row(&world, &reaction.id).await;
    assert!(!row.value.contains(SECRET), "{}", row.value);
    assert!(!row.value.contains("positionals"), "{}", row.value);
    let request = row.request().expect("the request parses");
    assert_eq!(request.args_sha256, Some(args_sha256(&reaction.args)));
    assert_eq!(request.args_bytes, reaction.args.len() as u64);
    assert_eq!(value_ttl(&world, "drain_request", &reaction.id).await, None);
    // the args row holds the raw text and expires
    assert_eq!(
        args_row(&world, &reaction.id).await,
        Some(reaction.args.clone())
    );
    assert_fresh_ttl(
        value_ttl(&world, "drain_request_args", &reaction.id).await,
        "the args row",
    );
    world.drop_keyspace().await;
}

/// A reaction idle longer than the cutoff is recorded stale, an orphan with no log by its SLA
/// and a listed one by its newest log, while a recently active orphan is recorded; every one is
/// still cancelled
#[tokio::test]
async fn an_old_orphan_is_recorded_stale() {
    // an orphan with no log whose SLA passed 40 days ago, a listed reaction last active 40 days
    // ago, and an orphan active an hour ago
    let world = World::new_full("dr10").await;
    seed_scene(&world).await;
    let long_ago = Utc::now() - Duration::days(40);
    let old_orphan = OldReaction {
        listed: false,
        last_log: None,
        sla: long_ago,
        ..OldReaction::new(CREATOR, "Created")
    };
    let old_listed = OldReaction {
        last_log: Some(long_ago),
        ..OldReaction::new(CREATOR, "Started")
    };
    let new_orphan = OldReaction {
        listed: false,
        ..OldReaction::new(CREATOR, "Created")
    };
    for reaction in [&old_orphan, &old_listed, &new_orphan] {
        seed_old_reaction(&world, reaction).await;
    }
    let outcome = run_ok(&world).await;
    let report = drained(&outcome);
    assert_eq!(
        (report.cancelled, report.stale, report.recorded),
        (3, 2, 1),
        "{report:?}"
    );
    // the idle ones are stale, finished, with a reason, and keep their args for a hand run
    for (reaction, orphan) in [(&old_orphan, true), (&old_listed, false)] {
        let row = request_row(&world, &reaction.id).await;
        assert_eq!(row.status, DrainStatus::Stale, "{row:?}");
        assert!(row.finished.is_some(), "{row:?}");
        assert!(
            row.result
                .as_deref()
                .is_some_and(|result| !result.is_empty()),
            "{row:?}"
        );
        let request = row.request().expect("the request parses");
        assert_eq!(request.orphan, orphan);
        assert!(args_row(&world, &reaction.id).await.is_some());
        assert_cancelled(&world, reaction).await;
        assert!(last_log(&world, reaction).await.contains("not resubmitted"));
    }
    // the orphan's activity is its SLA, since it has no log
    let request = request_row(&world, &old_orphan.id)
        .await
        .request()
        .expect("parses");
    assert_eq!(
        request.last_activity.map(|at| at.timestamp()),
        Some(long_ago.timestamp())
    );
    // the active orphan is recorded, marked as an orphan
    let row = request_row(&world, &new_orphan.id).await;
    assert_eq!(row.status, DrainStatus::Recorded);
    assert!(row.request().expect("parses").orphan);
    assert_cancelled(&world, &new_orphan).await;
    world.drop_keyspace().await;
}

/// More in-flight reactions than the cap make the run fail before it records, fails, or removes
/// anything, naming the count and the way past it; a cap that holds them lets it run
#[tokio::test]
async fn the_cancel_refuses_over_the_cap_before_failing_anything() {
    // three in-flight reactions with jobs and a stage-path worker, over a cap of two
    let world = World::new_full("dr11").await;
    seed_scene(&world).await;
    let reactions = [
        OldReaction::new(CREATOR, "Started"),
        OldReaction::new(CREATOR, "Created"),
        OldReaction::new(CREATOR, "Started"),
    ];
    for reaction in &reactions {
        seed_old_reaction(&world, reaction).await;
        seed_old_job(&world, &OldJob::new(reaction, None)).await;
    }
    seed_registered_workers(&world, &[WORKER.to_owned()], "prod", "node-3").await;
    let before = dump(&world).await;
    let capped = RunFlags {
        max_requests: Some(2),
        ..flags()
    };
    let result = run_with(&world, capped, 100).await;
    // the failure names the count and the override
    let failure = failure_of(&result).expect("the cancel refuses over the cap");
    assert!(failure.contains('3'), "{failure}");
    assert!(failure.contains("--max-requests"), "{failure}");
    // nothing in Redis changed and nothing was recorded
    assert_eq!(dump(&world).await, before);
    let rows = store(&world)
        .await
        .list_page(None, 100)
        .await
        .expect("the rows list");
    assert!(rows.is_empty(), "{rows:?}");
    // a cap that holds them all lets the cancel run
    let roomy = RunFlags {
        max_requests: Some(3),
        ..flags()
    };
    let outcome = run_with(&world, roomy, 100).await.expect("the cancel runs");
    assert_eq!(outcome.failure, None);
    assert_eq!(drained(&outcome).cancelled, 3);
    world.drop_keyspace().await;
}

/// A reaction whose args are over the size limit is recorded refused with their hash and size
/// but no args row, and is still cancelled
#[tokio::test]
async fn oversize_args_are_refused_and_the_hash_kept() {
    // args larger than the limit
    let world = World::new_full("dr12").await;
    seed_scene(&world).await;
    let big = format!("{SECRET}{}", "x".repeat(MAX_ARGS_BYTES));
    let reaction = OldReaction {
        args: args_json(&big),
        ..OldReaction::new(CREATOR, "Started")
    };
    seed_old_reaction(&world, &reaction).await;
    let outcome = run_ok(&world).await;
    assert_eq!(drained(&outcome).refused, 1);
    // refused for the size, keeping the hash and size but not the args
    let row = request_row(&world, &reaction.id).await;
    assert_eq!(row.status, DrainStatus::Refused);
    assert!(
        row.result
            .as_deref()
            .is_some_and(|result| result.contains("too large")),
        "{:?}",
        row.result
    );
    assert!(!row.value.contains(SECRET));
    let request = row.request().expect("the request parses");
    assert_eq!(request.args_sha256, Some(args_sha256(&reaction.args)));
    assert_eq!(request.args_bytes, reaction.args.len() as u64);
    assert_eq!(args_row(&world, &reaction.id).await, None);
    assert_eq!(args_ttls(&world).await, Vec::new());
    // the reaction is cancelled and its log says it won't be resubmitted
    assert_cancelled(&world, &reaction).await;
    assert!(
        last_log(&world, &reaction)
            .await
            .contains("not resubmitted")
    );
    world.drop_keyspace().await;
}

/// A reaction created by a system account is recorded refused with no args row, even when the
/// account holds a role in the group, and is still cancelled
#[tokio::test]
async fn a_system_account_reaction_is_recorded_refused() {
    // a reaction by each system account, each in the group's users role, and one by a user
    let world = World::new_full("dr13").await;
    seed_scene(&world).await;
    seed_group(&world, GROUP, &[("users", &SERVICE_USERS)]).await;
    let system = SERVICE_USERS
        .iter()
        .map(|account| OldReaction::new(account, "Started"))
        .collect::<Vec<_>>();
    let normal = OldReaction::new(CREATOR, "Started");
    for reaction in system.iter().chain([&normal]) {
        seed_old_reaction(&world, reaction).await;
    }
    let outcome = run_ok(&world).await;
    let report = drained(&outcome);
    assert_eq!(
        (report.cancelled, report.refused, report.recorded),
        (4, 3, 1),
        "{report:?}"
    );
    // each system account's request is refused, naming why, without its args
    for reaction in &system {
        let row = request_row(&world, &reaction.id).await;
        assert_eq!(row.status, DrainStatus::Refused, "{}", reaction.creator);
        assert!(
            row.result
                .as_deref()
                .is_some_and(|result| result.contains("system account")),
            "{:?}",
            row.result
        );
        assert!(row.finished.is_some());
        assert_eq!(args_row(&world, &reaction.id).await, None);
        assert_cancelled(&world, reaction).await;
    }
    // the user's request is recorded
    assert_eq!(
        request_row(&world, &normal.id).await.status,
        DrainStatus::Recorded
    );
    world.drop_keyspace().await;
}

/// The creator's membership is read from the group's combined owner, manager, user, and monitor
/// roles: the first three hold a role and are recorded, while a monitor, an analyst outside the
/// group, and an outsider hold none and are refused
#[tokio::test]
async fn the_cancel_records_membership_from_the_group_roles() {
    // a creator in each role, one through a metagroup, an analyst, and an outsider
    let world = World::new_full("dr14").await;
    seed_scene(&world).await;
    seed_group(
        &world,
        GROUP,
        &[
            ("owners", &["io-owner"]),
            ("managers", &["europa-manager"]),
            ("users", &["ganymede-user"]),
            ("monitors", &["callisto-monitor"]),
        ],
    )
    .await;
    let mut pipe = redis::pipe();
    pipe.cmd("SADD")
        .arg(format!("{}:groups_combined:{GROUP}:users", world.ns))
        .arg("amalthea-meta")
        .cmd("SADD")
        .arg(format!("{}:groups_metagroups:{GROUP}:users", world.ns))
        .arg("amalthea-meta")
        .cmd("SADD")
        .arg(format!("{}:analysts", world.ns))
        .arg("himalia-analyst");
    exec(&world, &pipe).await;
    let expected = [
        ("io-owner", "owner", true),
        ("europa-manager", "manager", true),
        ("ganymede-user", "user", true),
        ("amalthea-meta", "user", true),
        ("callisto-monitor", "monitor", false),
        ("himalia-analyst", "none", false),
        ("thebe-outsider", "none", false),
    ];
    let mut reactions = Vec::new();
    for (creator, role, held) in expected {
        let reaction = OldReaction::new(creator, "Started");
        seed_old_reaction(&world, &reaction).await;
        reactions.push((reaction, role, held));
    }
    run_ok(&world).await;
    // each row records the role, and only a held role is recorded rather than refused
    for (reaction, role, held) in &reactions {
        let row = request_row(&world, &reaction.id).await;
        let request = row.request().expect("the request parses");
        assert_eq!(
            request.membership,
            CreatorMembership {
                held_role: *held,
                role: (*role).to_owned()
            },
            "{}",
            reaction.creator
        );
        let status = if *held {
            DrainStatus::Recorded
        } else {
            DrainStatus::Refused
        };
        assert_eq!(row.status, status, "{}", reaction.creator);
        assert_cancelled(&world, reaction).await;
    }
    world.drop_keyspace().await;
}

/// The run's result holds counts and the worker list only: no reaction or job id, creator,
/// pipeline, group outside the worker list, or argument
#[tokio::test]
async fn the_job_result_holds_no_reaction_id_creator_or_argument() {
    // a parent and child with jobs, and the worker running the parent's job
    let world = World::new_full("dr15").await;
    seed_scene(&world).await;
    let parent = OldReaction {
        sub_reactions: 1,
        ..OldReaction::new(CREATOR, "Started")
    };
    let child = OldReaction {
        parent: Some(parent.id.clone()),
        ..OldReaction::new(CREATOR, "Created")
    };
    seed_old_reaction(&world, &parent).await;
    seed_old_reaction(&world, &child).await;
    let running = OldJob::new(&parent, Some(WORKER));
    let created = OldJob::new(&child, None);
    seed_old_job(&world, &running).await;
    seed_old_job(&world, &created).await;
    seed_registered_workers(&world, &[WORKER.to_owned()], "prod", "node-3").await;
    let outcome = run_ok(&world).await;
    // the worker list is the one list, and nothing is skipped or noted by name
    assert_eq!(
        drained(&outcome).workers,
        vec![drain_worker(WORKER, "prod", "node-3")]
    );
    assert!(
        outcome.report.skipped.is_empty(),
        "{:?}",
        outcome.report.skipped
    );
    assert!(
        outcome.report.notes.is_empty(),
        "{:?}",
        outcome.report.notes
    );
    // without the worker list, the result names nothing
    let mut result = serde_json::to_value(&outcome.report).expect("the report renders");
    result["drain"]
        .as_object_mut()
        .expect("the drain report")
        .remove("workers");
    let text = result.to_string();
    print!("{text}");
    for named in [
        parent.id.as_str(),
        child.id.as_str(),
        running.id.as_str(),
        created.id.as_str(),
        CREATOR,
        PIPELINE,
        PIPELINE_CREATOR,
        GROUP,
        SECRET,
        SAMPLE,
        WORKER,
        "node-3",
    ] {
        assert!(!text.contains(named), "{named} in {text}");
    }
    world.drop_keyspace().await;
}

/// More stage-path workers than the list cap all lose their registrations, the report lists the
/// cap's worth, and counts the rest
#[tokio::test]
async fn a_long_worker_list_is_chunked_and_capped() {
    // three more workers than the cap
    let world = World::new_full("dr16").await;
    seed_scene(&world).await;
    let names = (0..MAX_DRAIN_WORKERS + 3)
        .map(|n| format!("corn-unpack-{n:05}"))
        .collect::<Vec<_>>();
    seed_registered_workers(&world, &names, "prod", "node-1").await;
    let outcome = run_with(&world, flags(), 1_000)
        .await
        .expect("the cancel runs");
    assert_eq!(outcome.failure, None);
    // every registration is gone, the cap's worth are listed once each, and the rest counted
    let report = drained(&outcome);
    assert_eq!(report.workers_removed, (MAX_DRAIN_WORKERS + 3) as u64);
    assert_eq!(report.workers.len(), MAX_DRAIN_WORKERS);
    assert_eq!(report.workers_not_listed, 3);
    let seeded = names.iter().collect::<BTreeSet<_>>();
    let listed = report
        .workers
        .iter()
        .map(|worker| &worker.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(listed.len(), MAX_DRAIN_WORKERS);
    assert!(listed.is_subset(&seeded));
    let left = keys(&world).await;
    assert!(
        !left
            .iter()
            .any(|key| key.contains(":worker_data:") || key.contains(":workers:")),
        "registrations are left"
    );
    world.drop_keyspace().await;
}

/// A run whose batches are smaller than what it cancels reads every reaction and worker across
/// batches, still failing each child before its parent
#[tokio::test]
async fn reads_are_paged() {
    // a parent with two children, two other reactions, and three workers, in batches of two
    let world = World::new_full("dr17").await;
    seed_scene(&world).await;
    let parent = OldReaction {
        sub_reactions: 2,
        ..OldReaction::new(CREATOR, "Started")
    };
    let mut reactions = vec![parent.clone()];
    for _ in 0..2 {
        reactions.push(OldReaction {
            parent: Some(parent.id.clone()),
            ..OldReaction::new(CREATOR, "Created")
        });
    }
    reactions.push(OldReaction::new(CREATOR, "Started"));
    reactions.push(OldReaction::new(CREATOR, "Created"));
    for reaction in &reactions {
        seed_old_reaction(&world, reaction).await;
        seed_old_job(&world, &OldJob::new(reaction, None)).await;
    }
    let workers = (0..3)
        .map(|n| format!("corn-unpack-p{n}"))
        .collect::<Vec<_>>();
    seed_registered_workers(&world, &workers, "prod", "node-2").await;
    let outcome = run_with(&world, flags(), 2).await.expect("the cancel runs");
    assert_eq!(outcome.failure, None);
    // every reaction and worker was read and handled
    let report = drained(&outcome);
    assert_eq!((report.cancelled, report.recorded), (5, 5), "{report:?}");
    assert_eq!(report.workers_removed, 3);
    assert!(outcome.counts.read >= 8, "{:?}", outcome.counts);
    for reaction in &reactions {
        assert_cancelled(&world, reaction).await;
    }
    // failing the parent last proceeded nothing, so no job is left or created
    let left = keys(&world).await;
    assert!(
        !left.iter().any(|key| key.contains(":job_data:")),
        "{left:?}"
    );
    // the rows page back by id, two at a time
    let store = store(&world).await;
    let mut after: Option<String> = None;
    let mut read = BTreeSet::new();
    loop {
        let page = store
            .list_page(after.as_deref(), 2)
            .await
            .expect("a page reads");
        let Some(last) = page.last() else {
            break;
        };
        after = Some(last.id.clone());
        read.extend(page.into_iter().map(|row| row.id));
    }
    let seeded = reactions
        .iter()
        .map(|reaction| reaction.id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(read, seeded);
    world.drop_keyspace().await;
}

/// A run in a Job leaves its applied report, bounded, as the Job's result row, with the counts and
/// the worker list under the field names the status Job and the operator read
#[tokio::test]
async fn a_run_leaves_its_report_as_the_jobs_result() {
    // a Started reaction whose job a registered worker runs
    let world = World::new_full("dr18").await;
    seed_scene(&world).await;
    let reaction = OldReaction::new(CREATOR, "Started");
    seed_old_reaction(&world, &reaction).await;
    let running = OldJob::new(&reaction, Some(WORKER));
    seed_old_job(&world, &running).await;
    seed_registered_workers(&world, &[WORKER.to_owned()], "prod", "node-3").await;
    // the run is in a Job
    let uid = Uuid::new_v4().to_string();
    set_result_uid(&world.ns, &uid);
    let outcome = run_ok(&world).await;
    // the row holds the bounded report
    let row = read_job_result(&world.backends.scylla, &world.conf, &uid)
        .await
        .expect("the result row reads")
        .expect("the run left its result");
    let mut bounded = outcome.report.clone();
    bounded.bound();
    assert_eq!(
        row,
        serde_json::to_value(&bounded).expect("the report renders")
    );
    // a status Job reads a row with these as an applied report
    assert!(
        row.get("skipped_total").is_some() && row.get("groups").is_some(),
        "{row}"
    );
    // the operator reads the counts and workers under these names
    let drain = &row["drain"];
    assert_eq!(drain["cancelled"], 1, "{row}");
    assert_eq!(drain["recorded"], 1, "{row}");
    assert_eq!(drain["workers_removed"], 1, "{row}");
    assert_eq!(drain["workers_not_listed"], 0, "{row}");
    assert_eq!(
        drain["workers"],
        serde_json::json!([{"group": GROUP, "name": WORKER, "scaler": "K8s", "cluster": "prod",
                            "node": "node-3"}])
    );
    world.drop_keyspace().await;
}
