//! Tests of the `pipelines-to-tool-stages` migration driven in-process against Scylla and Redis:
//! converted, unresolved, and already converted pipelines, checks, slices, batches, and reruns

mod common;

use common::migrate::World;
use std::collections::BTreeMap;
use thoradm::migrate::pipelines_to_tool_stages::PIPELINES_TO_TOOL_STAGES;
use thorium::client::migrations::ToolIdMap;
use thorium::models::schema::{PassOutcome, RunMode, RunOptions, check_pass, run_pass};
use thorium::models::{ImageScaler, WorkloadSpec};
use uuid::Uuid;

/// Options for a pass with the given number of slices
///
/// # Arguments
///
/// * `slices` - How many slices to run
fn slices(slices: u32) -> RunOptions {
    // everything else stays at its default
    RunOptions {
        slices,
        ..RunOptions::default()
    }
}

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
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{}:*", world.ns))
        .query_async(&mut *conn)
        .await
        .expect("keys list");
    for key in keys {
        let _: () = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .expect("a key deletes");
    }
}

/// Store a pipeline the way the API does
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The pipeline's group
/// * `name` - The pipeline's name
/// * `order` - The stages in order, one list per position
/// * `stage_tools` - The pipeline's `stage_tools`, when it has any
async fn seed_pipeline(
    world: &World,
    group: &str,
    name: &str,
    order: &[&[&str]],
    stage_tools: Option<&BTreeMap<String, Uuid>>,
) {
    // the set of the group's pipelines and the hash of this one
    let ns = &world.ns;
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = redis::cmd("SADD")
        .arg(format!("{ns}:pipelines:{group}"))
        .arg(name)
        .query_async(&mut *conn)
        .await
        .expect("the set adds");
    let mut cmd = redis::cmd("HSET");
    cmd.arg(format!("{ns}:pipeline_data:{group}:{name}"))
        .arg("group")
        .arg(group)
        .arg("name")
        .arg(name)
        .arg("creator")
        .arg("tester")
        .arg("order")
        .arg(serde_json::to_string(order).unwrap())
        .arg("sla")
        .arg("604800")
        .arg("triggers")
        .arg("{}")
        .arg("bans")
        .arg("{}");
    if let Some(tools) = stage_tools {
        cmd.arg("stage_tools")
            .arg(serde_json::to_string(tools).unwrap());
    }
    let _: () = cmd.query_async(&mut *conn).await.expect("the hash sets");
}

/// Store an image's name in its group's set, as the API does
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The image's group
/// * `name` - The image's name
async fn seed_image(world: &World, group: &str, name: &str) {
    // only the set matters to a check
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = redis::cmd("SADD")
        .arg(format!("{}:images:{group}", world.ns))
        .arg(name)
        .query_async(&mut *conn)
        .await
        .expect("the set adds");
}

/// Store a K8s image with every field the API's image creation writes, with or without a
/// container image reference (the conversion refuses a K8s image that has none)
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The image's group
/// * `name` - The image's name
/// * `container` - The container image the image runs, when it has one
async fn seed_full_image(world: &World, group: &str, name: &str, container: Option<&str>) {
    // start from the projection of an empty spec and make it a K8s image
    let mut image = WorkloadSpec::default().project_image(group, name, 600.0);
    image.scaler = ImageScaler::K8s;
    image.image = container.map(str::to_owned);
    "io".clone_into(&mut image.creator);
    // store every field the image's hash holds, as the API does
    let ns = &world.ns;
    let mut cmd = redis::cmd("HSET");
    cmd.arg(format!("{ns}:image_data:{group}:{name}"))
        .arg("group")
        .arg(group)
        .arg("name")
        .arg(name)
        .arg("creator")
        .arg(&image.creator)
        .arg("scaler")
        .arg(serde_json::to_string(&image.scaler).unwrap())
        .arg("resources")
        .arg(serde_json::to_string(&image.resources).unwrap())
        .arg("spawn_limit")
        .arg(serde_json::to_string(&image.spawn_limit).unwrap())
        .arg("runtime")
        .arg(image.runtime)
        .arg("volumes")
        .arg(serde_json::to_string(&image.volumes).unwrap())
        .arg("env")
        .arg(serde_json::to_string(&image.env).unwrap())
        .arg("args")
        .arg(serde_json::to_string(&image.args).unwrap())
        .arg("security_context")
        .arg(serde_json::to_string(&image.security_context).unwrap())
        .arg("collect_logs")
        .arg(serde_json::to_string(&image.collect_logs).unwrap())
        .arg("generator")
        .arg(serde_json::to_string(&image.generator).unwrap())
        .arg("dependencies")
        .arg(serde_json::to_string(&image.dependencies).unwrap())
        .arg("display_type")
        .arg(serde_json::to_string(&image.display_type).unwrap())
        .arg("output_collection")
        .arg(serde_json::to_string(&image.output_collection).unwrap())
        .arg("child_filters")
        .arg(serde_json::to_string(&image.child_filters).unwrap())
        .arg("network_policies")
        .arg(serde_json::to_string(&image.network_policies).unwrap());
    if let Some(container) = &image.image {
        cmd.arg("image")
            .arg(serde_json::to_string(container).unwrap());
    }
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = cmd.query_async(&mut *conn).await.expect("the hash sets");
    // and its name in the group's set
    seed_image(world, group, name).await;
}

/// Store the first tool instance the image conversion would publish for an image
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The image's group
/// * `name` - The image's name
async fn seed_instance(world: &World, group: &str, name: &str) {
    // the instance belongs to the tool derived from the image
    seed_instance_of(world, group, name, ToolIdMap::tool_id(group, name)).await;
}

/// Store the first tool instance of an image, belonging to the given tool
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The image's group
/// * `name` - The image's name
/// * `tool` - The tool the instance says it belongs to
async fn seed_instance_of(world: &World, group: &str, name: &str, tool: Uuid) {
    // the instance entity, keyed the way the conversion derives it
    let id = ToolIdMap::first_instance_id(group, name);
    let kind_data = serde_json::json!({"ToolInstance": {"tool": tool}}).to_string();
    let now = chrono::Utc::now();
    world
        .session()
        .query_unpaged(
            format!(
                "INSERT INTO {}.entities (kind, group, year, bucket, created, id, name, \
                 submitter, kind_data) VALUES (?, ?, 2026, 0, ?, ?, ?, 'tester', ?)",
                world.ns
            ),
            (
                "\"ToolInstance\"",
                group,
                scylla::value::CqlTimestamp(now.timestamp_millis()),
                id,
                format!("{name}-v1"),
                kind_data,
            ),
        )
        .await
        .expect("the instance stores");
    // the view fills asynchronously, so wait until the id reads back
    for _ in 0..100 {
        let rows = world
            .session()
            .query_unpaged(
                format!("SELECT id FROM {}.entities_by_id WHERE id = ?", world.ns),
                (id,),
            )
            .await
            .expect("the view reads")
            .into_rows_result()
            .expect("rows");
        if rows.rows_num() > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the view never showed the instance");
}

/// Read a pipeline's stored `stage_tools`
///
/// # Arguments
///
/// * `world` - The test's world
/// * `group` - The pipeline's group
/// * `name` - The pipeline's name
async fn stage_tools(world: &World, group: &str, name: &str) -> Option<BTreeMap<String, Uuid>> {
    // an absent field is a pipeline not converted
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let raw: Option<String> = redis::cmd("HGET")
        .arg(format!("{}:pipeline_data:{group}:{name}", world.ns))
        .arg("stage_tools")
        .query_async(&mut *conn)
        .await
        .expect("the field reads");
    raw.map(|raw| serde_json::from_str(&raw).expect("stage_tools parses"))
}

/// Run a pass of the migration to the end
///
/// # Arguments
///
/// * `world` - The test's world
/// * `slice_count` - How many slices to run
/// * `batch_rows` - The pipelines of each batch
async fn run(world: &World, slice_count: u32, batch_rows: usize) -> PassOutcome {
    // a lease of the test's own
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, batch_rows);
    let out = run_pass(
        PIPELINES_TO_TOOL_STAGES.runner,
        &World::spec(&PIPELINES_TO_TOOL_STAGES),
        &slices(slice_count),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the pass runs");
    lease.release().await;
    out
}

/// The expected `stage_tools` of stages in a group
///
/// # Arguments
///
/// * `group` - The group
/// * `stages` - The stage names
fn expected(group: &str, stages: &[&str]) -> BTreeMap<String, Uuid> {
    // every stage is pinned to the first instance of the tool derived from its image
    stages
        .iter()
        .map(|stage| {
            (
                (*stage).to_owned(),
                ToolIdMap::first_instance_id(group, stage),
            )
        })
        .collect()
}

/// A pipeline whose stages all have instances converts, one with a stage that doesn't is left
/// untouched as an issue, and one that is converted already is current
#[tokio::test]
async fn pipelines_convert_whole_or_not_at_all() {
    // p1 resolves, p2 has a stage c with no image or instance, p3 is converted
    let world = World::new_full("pp01").await;
    clear_redis(&world).await;
    seed_instance(&world, "corn", "a").await;
    seed_instance(&world, "corn", "b").await;
    seed_pipeline(&world, "corn", "p1", &[&["a"], &["b"]], None).await;
    seed_pipeline(&world, "corn", "p2", &[&["a"], &["c"]], None).await;
    seed_pipeline(
        &world,
        "corn",
        "p3",
        &[&["a"]],
        Some(&expected("corn", &["a"])),
    )
    .await;
    let out = run(&world, 1, 10).await;
    assert_eq!(out.counts.read, 3);
    assert_eq!(out.counts.written, 1);
    assert_eq!(out.counts.unrecognized, 1);
    assert_eq!(out.counts.current, 1);
    // the report counts the one converted pipeline as mapped to its stage tools
    assert_eq!(out.report.mappings_total, 1, "{:?}", out.report);
    // only p1 changed
    assert_eq!(
        stage_tools(&world, "corn", "p1").await,
        Some(expected("corn", &["a", "b"]))
    );
    assert_eq!(stage_tools(&world, "corn", "p2").await, None);
    assert_eq!(
        stage_tools(&world, "corn", "p3").await,
        Some(expected("corn", &["a"]))
    );
}

/// An instance whose tool isn't the derived one doesn't resolve its stage
#[tokio::test]
async fn an_instance_of_another_tool_does_not_resolve_a_stage() {
    // the instance entity at the derived id points at some other tool
    let world = World::new_full("pp02").await;
    clear_redis(&world).await;
    seed_instance_of(&world, "corn", "a", Uuid::new_v4()).await;
    seed_pipeline(&world, "corn", "p1", &[&["a"]], None).await;
    let out = run(&world, 1, 10).await;
    assert_eq!(out.counts.unrecognized, 1);
    assert_eq!(stage_tools(&world, "corn", "p1").await, None);
}

/// A check counts a stage whose image exists as planned, resolves nothing else, and writes nothing
#[tokio::test]
async fn a_check_plans_from_images_and_writes_nothing() {
    // p1's image exists but its instance doesn't yet; p2's stage has neither
    let world = World::new_full("pp03").await;
    clear_redis(&world).await;
    seed_image(&world, "corn", "a").await;
    seed_pipeline(&world, "corn", "p1", &[&["a"]], None).await;
    seed_pipeline(&world, "corn", "p2", &[&["z"]], None).await;
    let ctx = world.ctx(RunMode::Check, 10);
    let run = check_pass(
        PIPELINES_TO_TOOL_STAGES.runner,
        &World::spec(&PIPELINES_TO_TOOL_STAGES),
        &slices(1),
        &ctx,
    )
    .await
    .expect("the check runs");
    assert_eq!(run.report.counts.read, 2);
    assert_eq!(run.report.counts.migratable, 1);
    assert_eq!(run.report.counts.unrecognized, 1);
    assert_eq!(stage_tools(&world, "corn", "p1").await, None);
    assert_eq!(stage_tools(&world, "corn", "p2").await, None);
}

/// A check doesn't plan a stage whose image the image conversion refuses, since that image never
/// gets an instance: the pipeline that runs it is an issue before any run
#[tokio::test]
async fn a_check_does_not_plan_a_stage_of_a_refused_image() {
    // p1 runs an image that converts; p2 also runs a K8s image with no container image
    let world = World::new_full("pp07").await;
    clear_redis(&world).await;
    seed_full_image(&world, "corn", "ok", Some("repo/ok:1.0")).await;
    seed_full_image(&world, "corn", "bare", None).await;
    seed_pipeline(&world, "corn", "p1", &[&["ok"]], None).await;
    seed_pipeline(&world, "corn", "p2", &[&["ok"], &["bare"]], None).await;
    let ctx = world.ctx(RunMode::Check, 10);
    let run = check_pass(
        PIPELINES_TO_TOOL_STAGES.runner,
        &World::spec(&PIPELINES_TO_TOOL_STAGES),
        &slices(1),
        &ctx,
    )
    .await
    .expect("the check runs");
    assert_eq!(run.report.counts.read, 2);
    assert_eq!(run.report.counts.migratable, 1);
    assert_eq!(run.report.counts.unrecognized, 1);
}

/// A rerun finds everything current, and a pipeline that couldn't convert converts once its
/// instance exists
#[tokio::test]
async fn a_rerun_finishes_what_the_first_run_could_not() {
    // p1 waits on b's instance
    let world = World::new_full("pp04").await;
    clear_redis(&world).await;
    seed_instance(&world, "corn", "a").await;
    seed_pipeline(&world, "corn", "p1", &[&["a", "b"]], None).await;
    seed_pipeline(&world, "corn", "p2", &[&["a"]], None).await;
    let first = run(&world, 1, 10).await;
    assert_eq!(first.counts.written, 1);
    assert_eq!(first.counts.unrecognized, 1);
    // b's instance appears, then the rerun converts p1 and leaves p2
    seed_instance(&world, "corn", "b").await;
    let second = run(&world, 1, 10).await;
    assert_eq!(second.counts.written, 1);
    assert_eq!(second.counts.current, 1);
    assert_eq!(second.counts.unrecognized, 0);
    assert_eq!(
        stage_tools(&world, "corn", "p1").await,
        Some(expected("corn", &["a", "b"]))
    );
    // a third run changes nothing
    let third = run(&world, 1, 10).await;
    assert_eq!(third.counts.written, 0);
    assert_eq!(third.counts.current, 2);
}

/// Several groups and slices convert every pipeline exactly once
#[tokio::test]
async fn slices_cover_every_group() {
    // twelve pipelines over three groups, three slices, batches of two
    let world = World::new_full("pp05").await;
    clear_redis(&world).await;
    for group in ["corn", "wheat", "rye"] {
        seed_instance(&world, group, "s").await;
        for n in 0..4 {
            seed_pipeline(&world, group, &format!("p{n}"), &[&["s"]], None).await;
        }
    }
    let out = run(&world, 3, 2).await;
    assert_eq!(out.counts.read, 12);
    assert_eq!(out.counts.written, 12);
    for group in ["corn", "wheat", "rye"] {
        for n in 0..4 {
            assert_eq!(
                stage_tools(&world, group, &format!("p{n}")).await,
                Some(expected(group, &["s"]))
            );
        }
    }
}

/// Batches resume from the position the last one returned, in a stable order, with dangling set
/// members skipped
#[tokio::test]
async fn batches_walk_a_slice_in_order() {
    // five pipelines in one slice and a set member without a hash
    let world = World::new_full("pp06").await;
    clear_redis(&world).await;
    for n in 0..5 {
        seed_pipeline(&world, "corn", &format!("p{n}"), &[&["s"]], None).await;
    }
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = redis::cmd("SADD")
        .arg(format!("{}:pipelines:corn", world.ns))
        .arg("ghost")
        .query_async(&mut *conn)
        .await
        .expect("the set adds");
    let ctx = world.ctx(RunMode::Run, 2);
    let runner = PIPELINES_TO_TOOL_STAGES.runner;
    let slices = runner.slices(1, &ctx).await.expect("slices");
    assert_eq!(slices.len(), 1);
    let mut after = slices[0].start.clone();
    let mut seen = Vec::new();
    loop {
        let batch = runner
            .next_batch(&slices[0], &after, &ctx)
            .await
            .expect("a batch");
        assert!(batch.records.len() <= 2);
        seen.extend(
            batch
                .records
                .iter()
                .map(|record| thorium::models::schema::record_key(record).to_owned()),
        );
        after = batch.next;
        if batch.done {
            break;
        }
    }
    assert_eq!(
        seen,
        ["corn/p0", "corn/p1", "corn/p2", "corn/p3", "corn/p4"]
    );
}
