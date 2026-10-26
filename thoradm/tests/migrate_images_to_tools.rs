//! Tests of the `images-to-tools` migration: it reads the images of every group from Redis,
//! converts each with the pure conversion, and classifies what a run would do. A check writes
//! nothing; a run creates each tool and its first instance through the API's own tool creation

mod common;

use async_trait::async_trait;
use common::cluster::Cluster;
use common::harness::{ADMIN, Harness};
use common::migrate::World;
use common::names::Names;
use common::seed;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thorium::client::migrations::{
    MIGRATED_FROM_IMAGE_TAG, MIGRATION_OPERATOR, ToolIdMap, migrated_at,
};
use thorium::models::backends::db;
use thorium::models::schema::{
    Batch, CheckRun, MigrateError, MigrationRunStatus, MigrationRunner, Outcome, PassOutcome,
    PassStore, RunBackends, RunContext, RunMode, RunOptions, Slice, check_pass, run_pass,
};
use thorium::models::upgrades::Record;
use thorium::models::{
    Entity, EntityMetadata, EntityUpdate, Image, ImageScaler, SystemSettings, ToolSpec, User,
    UserRole, UserSettings, Volume, VolumeTypes, WorkloadEnvironment, WorkloadSpec,
};
use thorium::test_utilities::{faults, generators};
use thorium::utils::{Shared, protected_names};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The migration under test
fn def() -> &'static MigrationDef {
    // the migration is registered like every other
    MIGRATIONS
        .iter()
        .find(|def| def.id == "images-to-tools")
        .expect("images-to-tools is registered")
}

/// Build a K8s image with nothing but its group, name, creator, and container image set
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
fn image(group: &str, name: &str) -> Image {
    // start from the projection of an empty spec and make it a K8s image
    let mut image = WorkloadSpec::default().project_image(group, name, 600.0);
    image.scaler = ImageScaler::K8s;
    image.image = Some(format!("repo/{name}:1.0"));
    "io".clone_into(&mut image.creator);
    image
}

/// Run a Redis command against the world's Redis
///
/// # Arguments
///
/// * `world` - The test world
/// * `cmd` - The command to run
async fn redis_cmd(world: &World, cmd: redis::Cmd) {
    // one pooled connection per command is plenty for a test
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    cmd.query_async::<()>(&mut *conn)
        .await
        .expect("the Redis command runs");
}

/// Add a group with its members to Redis, and the users as existing users
///
/// # Arguments
///
/// * `world` - The test world
/// * `group` - The group's name
/// * `members` - The users in the group
async fn seed_group(world: &World, group: &str, members: &[&str]) {
    let ns = &world.ns;
    // the group is in the set of groups
    redis_cmd(
        world,
        redis::cmd("SADD")
            .arg(format!("{ns}:groups"))
            .arg(group)
            .to_owned(),
    )
    .await;
    // its members hold the user role, and each exists as a user
    for member in members {
        redis_cmd(
            world,
            redis::cmd("SADD")
                .arg(format!("{ns}:groups_combined:{group}:users"))
                .arg(member)
                .to_owned(),
        )
        .await;
        redis_cmd(
            world,
            redis::cmd("HSET")
                .arg(format!("{ns}:user_data:{member}"))
                .arg("username")
                .arg(member)
                .to_owned(),
        )
        .await;
    }
}

/// Write an image the way the API stores one: a hash of JSON values in the image's data key and
/// its name in the group's set
///
/// # Arguments
///
/// * `world` - The test world
/// * `image` - The image to store
async fn seed_image(world: &World, image: &Image) {
    let ns = &world.ns;
    let key = format!("{ns}:image_data:{}:{}", image.group, image.name);
    // every field the API's image creation writes
    let mut cmd = redis::cmd("HSET");
    cmd.arg(&key)
        .arg("group")
        .arg(&image.group)
        .arg("name")
        .arg(&image.name)
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
    // the optional fields are written only when set
    if let Some(version) = &image.version {
        cmd.arg("version")
            .arg(serde_json::to_string(version).unwrap());
    }
    if let Some(container) = &image.image {
        cmd.arg("image")
            .arg(serde_json::to_string(container).unwrap());
    }
    if let Some(description) = &image.description {
        cmd.arg("description")
            .arg(serde_json::to_string(description).unwrap());
    }
    redis_cmd(world, cmd).await;
    redis_cmd(
        world,
        redis::cmd("SADD")
            .arg(format!("{ns}:images:{}", image.group))
            .arg(&image.name)
            .to_owned(),
    )
    .await;
}

/// Write a Tool entity row as a migration of an image would have left it, so the image counts
/// as already migrated
///
/// # Arguments
///
/// * `world` - The test world
/// * `group` - The group the tool is in
/// * `name` - The tool's name
/// * `id` - The tool's id
async fn seed_tool(world: &World, group: &str, name: &str, id: Uuid) {
    let ns = &world.ns;
    // the `entities` table keys rows by kind, group, year, bucket, created, and id
    world
        .session()
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.entities (kind, group, year, bucket, created, id, name, submitter, \
                 kind_data) VALUES ('Tool', ?, 2026, 0, toTimestamp(now()), {id}, ?, 'io', '{{}}')"
            ),
            (group, name),
        )
        .await
        .expect("the tool row writes");
}

/// Write the tag a migration puts on the tool it makes from an image
///
/// # Arguments
///
/// * `world` - The test world
/// * `group` - The group the tool is in
/// * `id` - The tool's id
/// * `image` - The image the tool was migrated from
async fn seed_migrated_tag(world: &World, group: &str, id: Uuid, image: &str) {
    let ns = &world.ns;
    // the tag row is keyed by type, group, year, bucket, key, and value
    world
        .session()
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, \
                 key_lower, value_lower) VALUES ('Entities', ?, 2026, 0, ?, ?, toTimestamp(now()), \
                 '{id}', ?, ?)"
            ),
            (
                group,
                MIGRATED_FROM_IMAGE_TAG,
                image,
                MIGRATED_FROM_IMAGE_TAG.to_lowercase(),
                image.to_lowercase(),
            ),
        )
        .await
        .expect("the tag row writes");
}

/// A world with the whole schema, and the groups jupiter (io is a member) and callisto (nobody is)
///
/// # Arguments
///
/// * `suffix` - What makes the test's keyspace unique
async fn world(suffix: &str) -> World {
    // the entities table is part of the declared schema
    let world = World::new_full(suffix).await;
    seed_group(&world, "jupiter", &["io"]).await;
    seed_group(&world, "callisto", &[]).await;
    world
}

/// Check the migration and return its run
///
/// # Arguments
///
/// * `world` - The test world
async fn check(world: &World) -> CheckRun {
    // a check never stops early and has nothing stored
    let ctx = world.ctx(RunMode::Check, 100);
    check_pass(
        def().runner,
        &World::spec(def()),
        &RunOptions::default(),
        &ctx,
    )
    .await
    .expect("the check runs")
}

/// Every Redis key in the namespace with its dump, to prove a check changed nothing
///
/// # Arguments
///
/// * `world` - The test world
async fn redis_snapshot(world: &World) -> BTreeMap<String, Vec<u8>> {
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let pattern = format!("{}:*", world.ns);
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(&pattern)
        .query_async(&mut *conn)
        .await
        .expect("the keys list");
    let mut snapshot = BTreeMap::new();
    for key in keys {
        let dump: Vec<u8> = redis::cmd("DUMP")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .expect("the key dumps");
        snapshot.insert(key, dump);
    }
    snapshot
}

/// The rows of every Scylla table the migration could touch, as text, to prove nothing was written
///
/// # Arguments
///
/// * `world` - The test world
async fn entity_count(world: &World) -> usize {
    let ns = &world.ns;
    world
        .session()
        .query_unpaged(format!("SELECT id FROM {ns}.entities_by_id"), ())
        .await
        .expect("the view reads")
        .into_rows_result()
        .expect("the rows read")
        .rows_num()
}

/// Every convertible image is counted as one a run would convert, every image the conversion
/// refuses is an issue with its code, and the issue groups name the images by `group/name`
#[tokio::test]
async fn check_counts_and_codes() {
    let world = world("itt_counts").await;
    // convertible images in two groups
    seed_image(&world, &image("jupiter", "alpha")).await;
    seed_image(&world, &image("callisto", "echo")).await;
    // a K8s image with no container image can't convert
    let mut bare = image("jupiter", "bare");
    bare.image = None;
    seed_image(&world, &bare).await;
    // an image whose only dependency names an image that doesn't exist would end up empty
    let mut lonely = image("jupiter", "lonely");
    lonely.dependencies.results.images = vec!["ghost".to_owned()];
    seed_image(&world, &lonely).await;
    // an image that names one real and one missing dependency converts, dropping the missing one
    let mut partial = image("jupiter", "partial");
    partial.dependencies.results.images = vec!["alpha".to_owned(), "ghost".to_owned()];
    seed_image(&world, &partial).await;
    // an image whose stored runtime doesn't parse
    seed_image(&world, &image("jupiter", "broken")).await;
    redis_cmd(
        &world,
        redis::cmd("HSET")
            .arg(format!("{}:image_data:jupiter:broken", world.ns))
            .arg("runtime")
            .arg("not-a-number")
            .to_owned(),
    )
    .await;
    // check
    let run = check(&world).await;
    let counts = run.report.counts;
    // six images were read; alpha, echo and partial convert
    assert_eq!(counts.read, 6, "{counts:?}");
    assert_eq!(counts.migratable, 3, "{counts:?}");
    assert_eq!(counts.current, 0, "{counts:?}");
    // bare, lonely, and broken are issues with their own codes
    assert_eq!(counts.unrecognized, 3, "{counts:?}");
    let codes = run
        .report
        .reasons
        .iter()
        .map(|reason| (reason.code.clone(), reason.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(codes.get("missing_field:image"), Some(&1), "{codes:?}");
    assert_eq!(
        codes.get("invalid_value:dependencies.results.images"),
        Some(&1),
        "{codes:?}"
    );
    assert_eq!(codes.get("malformed_json"), Some(&1), "{codes:?}");
    // the sample keys name the image by group and name
    let samples = run
        .groups
        .iter()
        .flat_map(|group| group.samples.clone())
        .collect::<Vec<_>>();
    assert!(samples.contains(&"jupiter/bare".to_owned()), "{samples:?}");
    assert!(
        samples.contains(&"jupiter/lonely".to_owned()),
        "{samples:?}"
    );
}

/// An image whose tool, first instance, and edge exist under its deterministic ids is current,
/// and a later image of the same group still converts
#[tokio::test]
async fn already_migrated_images_are_current() {
    let (world, _shared) = Box::pin(writable("itt_current")).await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    // alpha was migrated by an earlier run
    run(&world, &[], 100).await;
    seed_image(&world, &image("jupiter", "beta")).await;
    let run = check(&world).await;
    let counts = run.report.counts;
    assert_eq!(counts.read, 2, "{counts:?}");
    assert_eq!(counts.current, 1, "{counts:?}");
    assert_eq!(counts.migratable, 1, "{counts:?}");
    assert_eq!(counts.unrecognized, 0, "{counts:?}");
    world.drop_keyspace().await;
}

/// A tool whose first instance is missing was only half made by an earlier run, so the image
/// isn't migrated and is reported with the missing instance
#[tokio::test]
async fn a_tool_without_its_first_instance_is_an_issue() {
    let world = world("itt_half").await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    seed_tool(
        &world,
        "jupiter",
        "alpha",
        ToolIdMap::tool_id("jupiter", "alpha"),
    )
    .await;
    let run = check(&world).await;
    let counts = run.report.counts;
    assert_eq!(counts.current, 0, "{counts:?}");
    assert_eq!(counts.migratable, 0, "{counts:?}");
    assert_eq!(counts.unrecognized, 1, "{counts:?}");
    let codes = run
        .report
        .reasons
        .iter()
        .map(|reason| (reason.code.clone(), reason.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        codes.get("missing_field:tool_instance"),
        Some(&1),
        "{codes:?}"
    );
}

/// A tool of the image's name that isn't a migration of any image would collide with the tool
/// the image becomes, so the image is refused until one is renamed
#[tokio::test]
async fn a_tool_with_an_images_name_is_a_conflict() {
    let world = world("itt_conflict").await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    seed_image(&world, &image("jupiter", "beta")).await;
    // someone made a tool called alpha by hand
    seed_tool(&world, "jupiter", "alpha", Uuid::new_v4()).await;
    let run = check(&world).await;
    let counts = run.report.counts;
    assert_eq!(counts.migratable, 1, "{counts:?}");
    assert_eq!(counts.unrecognized, 1, "{counts:?}");
    let codes = run
        .report
        .reasons
        .iter()
        .map(|reason| (reason.code.clone(), reason.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(codes.get("invalid_value:name"), Some(&1), "{codes:?}");
    let samples = run
        .groups
        .iter()
        .flat_map(|group| group.samples.clone())
        .collect::<Vec<_>>();
    assert!(samples.contains(&"jupiter/alpha".to_owned()), "{samples:?}");
}

/// A same-named tool in another group, or one carrying the migration's tag, isn't a conflict
#[tokio::test]
async fn a_same_named_tool_elsewhere_or_tagged_is_not_a_conflict() {
    let world = world("itt_no_conflict").await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    seed_image(&world, &image("jupiter", "beta")).await;
    // alpha exists in another group only
    seed_tool(&world, "callisto", "alpha", Uuid::new_v4()).await;
    // beta's name is held by a tool a migration made from the image
    let held = Uuid::new_v4();
    seed_tool(&world, "jupiter", "beta", held).await;
    seed_migrated_tag(&world, "jupiter", held, "beta").await;
    let run = check(&world).await;
    let counts = run.report.counts;
    assert_eq!(counts.unrecognized, 0, "{counts:?}");
    assert_eq!(counts.migratable, 2, "{counts:?}");
}

/// Classifying the same image twice in one pass, as a retried batch does, leaves one note for it
#[tokio::test]
async fn an_image_classified_twice_has_one_note() {
    let world = world("itt_dupe").await;
    let mut partial = image("jupiter", "partial");
    partial.dependencies.results.images = vec!["ghost".to_owned(), "other".to_owned()];
    seed_image(&world, &partial).await;
    seed_image(&world, &image("jupiter", "other")).await;
    let ctx = world.ctx(RunMode::Check, 100);
    let runner = def().runner;
    // start the pass, then classify the image's record twice
    runner.slices(1, &ctx).await.expect("slices");
    let key = format!("{}:image_data:jupiter:partial", world.ns);
    let fields: BTreeMap<String, String> = {
        let mut conn = world.backends.redis.get().await.expect("a connection");
        redis::cmd("HGETALL")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .expect("the image reads")
    };
    let record = Record::Redis { key, fields };
    for _ in 0..2 {
        runner
            .process(&record, RunMode::Check, &ctx)
            .await
            .expect("classifies");
    }
    let mut report = thorium::models::schema::AppliedReport::default();
    runner
        .finish_report(&mut report, &ctx)
        .await
        .expect("the report finishes");
    let dropped = report
        .notes
        .iter()
        .filter(|note| note.item == "partial" && note.note.contains("ghost"))
        .count();
    assert_eq!(dropped, 1, "{:?}", report.notes);
}

/// A check changes nothing: no Redis key, no entity row, no pass row beyond what a check writes
#[tokio::test]
async fn check_never_writes() {
    let world = world("itt_readonly").await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    let mut bare = image("jupiter", "bare");
    bare.image = None;
    seed_image(&world, &bare).await;
    let before = redis_snapshot(&world).await;
    let entities = entity_count(&world).await;
    check(&world).await;
    assert_eq!(redis_snapshot(&world).await, before, "Redis changed");
    assert_eq!(entity_count(&world).await, entities, "entities changed");
}

/// Run the migration to the end in one slice, with the lease released after
///
/// # Arguments
///
/// * `world` - The test world
/// * `accept` - The reason codes the approval accepts
/// * `batch_rows` - The images of each batch
async fn run(world: &World, accept: &[&str], batch_rows: usize) -> PassOutcome {
    // the pass's spec carries the approval's accepted codes
    let spec = thoradm::migrate::context::spec_for(
        def(),
        accept.iter().map(|code| (*code).to_owned()),
        None,
    );
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, batch_rows);
    let options = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let out = run_pass(def().runner, &spec, &options, &ctx, &world.store, &lease)
        .await
        .expect("the pass runs");
    lease.release().await;
    out
}

/// The `Shared` the API's own reads go through, built the way the writer builds its own, with the
/// system settings the API writes at its first start
///
/// # Arguments
///
/// * `world` - The test world
async fn shared(world: &World) -> Shared {
    // the writer's constructor is the only one that needs no Elastic or OAuth
    let shared = Box::pin(Shared::for_offline_writer(
        world.conf.clone(),
        world.backends.redis.clone(),
    ))
    .await
    .expect("the offline shared builds");
    // a cluster has had its settings written by its API; a new namespace has not
    db::system::update_settings(&SystemSettings::default(), &shared)
        .await
        .expect("the settings write");
    shared
}

/// A world whose settings are written and a `Shared` over it, for the tests that run the writer
///
/// # Arguments
///
/// * `suffix` - What makes the test's keyspace unique
async fn writable(suffix: &str) -> (World, Shared) {
    let world = world(suffix).await;
    let shared = Box::pin(shared(&world)).await;
    (world, shared)
}

/// A member of a group who can read what the group holds
///
/// # Arguments
///
/// * `name` - The user's name
/// * `group` - The group the user is in
fn member(name: &str, group: &str) -> User {
    // a plain user of one group, with nothing a read doesn't use
    User {
        username: name.to_owned(),
        password: None,
        email: String::new(),
        role: UserRole::User,
        groups: vec![group.to_owned()],
        actual_groups: vec![group.to_owned()],
        token: String::new(),
        token_expiration: chrono::Utc::now(),
        unix: None,
        settings: UserSettings::default(),
        verified: true,
        verification_token: None,
        verification_sent: None,
        aliases: std::collections::HashMap::new(),
        image: None,
    }
}

/// How many rows each table a tool's creation writes holds, and how many events are queued
///
/// # Arguments
///
/// * `world` - The test world
async fn rows(world: &World) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    // the Scylla tables a creation writes to
    for table in ["entities", "tags", "associations"] {
        let rows = world
            .session()
            .query_unpaged(format!("SELECT * FROM {}.{table}", world.ns), ())
            .await
            .expect("the table reads")
            .into_rows_result()
            .expect("the rows read")
            .rows_num();
        counts.insert(table, rows);
    }
    // the queued workflow graph events
    let mut conn = world.backends.redis.get().await.expect("a connection");
    let events: usize = redis::cmd("ZCARD")
        .arg(format!("{}:event-handler:queue:WorkflowGraph", world.ns))
        .query_async(&mut *conn)
        .await
        .expect("the queue reads");
    counts.insert("events", events);
    counts
}

/// A run writes every convertible image as a tool in its group, with its first instance, tags,
/// edge, runtime, and event, and the tool reads back through the API's own read path with the
/// image's settings
#[tokio::test]
async fn a_run_writes_tools_that_read_back_with_their_images_settings() {
    let (world, shared) = Box::pin(writable("itt_write")).await;
    // alpha has settings of its own; beta depends on it and has a version
    let mut alpha = image("jupiter", "alpha");
    alpha
        .env
        .insert("LEVEL".to_owned(), Some("debug".to_owned()));
    alpha.runtime = 120.0;
    alpha.description = Some("the first".to_owned());
    seed_image(&world, &alpha).await;
    let mut beta = image("jupiter", "beta");
    beta.dependencies.results.images = vec!["alpha".to_owned()];
    beta.version = Some(thorium::models::ToolVersion::from("1.2.3"));
    seed_image(&world, &beta).await;
    // a creator outside the group is published as the operator
    seed_image(&world, &image("callisto", "orphan")).await;
    let out = run(&world, &[], 100).await;
    assert_eq!(out.status, MigrationRunStatus::Done, "{out:?}");
    assert_eq!(out.counts.read, 3, "{:?}", out.counts);
    assert_eq!(out.counts.written, 3, "{:?}", out.counts);
    assert_eq!(out.counts.flagged, 0, "{:?}", out.counts);
    assert_eq!(
        out.report.groups["images-to-tools"].changed, 3,
        "{:?}",
        out.report
    );
    // read the tools back as a member of their group
    let io = member("io", "jupiter");
    let tool = Entity::get(&io, ToolIdMap::tool_id("jupiter", "alpha"), &shared)
        .await
        .expect("the tool reads");
    assert_eq!(tool.name, "alpha");
    assert_eq!(tool.description.as_deref(), Some("the first"));
    assert_eq!(tool.submitter, "io");
    assert_eq!(tool.created, migrated_at());
    assert!(
        tool.groups.contains(&"jupiter".to_owned()),
        "{:?}",
        tool.groups
    );
    let first = ToolIdMap::first_instance_id("jupiter", "alpha");
    let EntityMetadata::Tool(root) = &tool.metadata else {
        panic!("alpha is a tool: {:?}", tool.metadata);
    };
    // its first instance is its latest and is listed under it by the edge
    assert_eq!(root.latest, Some(first));
    assert_eq!(root.instances.len(), 1, "{:?}", root.instances);
    assert_eq!(root.instances[0].id, first);
    // the tag that finds the tool again is on it
    assert!(
        tool.tags[MIGRATED_FROM_IMAGE_TAG].contains_key("alpha"),
        "{:?}",
        tool.tags
    );
    // the instance holds the image's conversion
    let instance = Entity::get(&io, first, &shared)
        .await
        .expect("the instance reads");
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        panic!("a tool instance: {:?}", instance.metadata);
    };
    let ToolSpec::Image(workload) = &meta.spec else {
        panic!("an image spec: {:?}", meta.spec);
    };
    assert_eq!(*workload, WorkloadSpec::from_image(&alpha).unwrap().spec);
    let WorkloadEnvironment::K8s(env) = &workload.environment else {
        panic!("a K8s environment: {:?}", workload.environment);
    };
    assert_eq!(env.env["LEVEL"], Some("debug".to_owned()));
    assert_eq!(env.image, "repo/alpha:1.0");
    assert_eq!(meta.version.to_string(), "migrated");
    // beta's dependency names alpha's tool and its version is the image's
    let beta_first = ToolIdMap::first_instance_id("jupiter", "beta");
    let instance = Entity::get(&io, beta_first, &shared)
        .await
        .expect("beta reads");
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        panic!("a tool instance: {:?}", instance.metadata);
    };
    let ToolSpec::Image(workload) = &meta.spec else {
        panic!("an image spec: {:?}", meta.spec);
    };
    assert_eq!(
        workload.data.dependencies.results.tools,
        vec![ToolIdMap::tool_id("jupiter", "alpha")]
    );
    assert_eq!(meta.version.to_string(), "1.2.3");
    // the workload's runtime was seeded for the first instance
    let runtimes = db::jobs::backup_workload_runtimes(&shared)
        .await
        .expect("the runtimes read");
    assert!(runtimes.contains_key(&first), "{runtimes:?}");
    // a tool of a group with no member among its creators is published as the operator
    let operator = member(MIGRATION_OPERATOR, "callisto");
    let orphan = Entity::get(&operator, ToolIdMap::tool_id("callisto", "orphan"), &shared)
        .await
        .expect("the orphan reads");
    assert_eq!(orphan.submitter, MIGRATION_OPERATOR);
    // a tool made three rows of events, one per tool
    let counts = rows(&world).await;
    assert_eq!(counts["events"], 3, "{counts:?}");
    world.drop_keyspace().await;
}

/// Running again after a finished run changes nothing: every image is already migrated, no row is
/// added, and no second event is queued
#[tokio::test]
async fn a_rerun_writes_nothing() {
    let (world, _shared) = Box::pin(writable("itt_rerun")).await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    seed_image(&world, &image("jupiter", "beta")).await;
    let first = run(&world, &[], 100).await;
    assert_eq!(first.counts.written, 2, "{:?}", first.counts);
    let before = rows(&world).await;
    // the second pass finds both migrated
    let second = run(&world, &[], 100).await;
    assert_eq!(second.counts.current, 2, "{:?}", second.counts);
    assert_eq!(second.counts.written, 0, "{:?}", second.counts);
    assert_eq!(second.report.groups["images-to-tools"].already_migrated, 2);
    assert_eq!(rows(&world).await, before);
    world.drop_keyspace().await;
}

/// A tool cut short at each point of its creation is made whole by the next run, writing the same
/// rows an uninterrupted run writes and no more
#[tokio::test]
async fn a_cut_short_tool_is_finished_without_duplicates() {
    // the control: both images written in one run
    let (control, _control_shared) = Box::pin(writable("itt_cut_c")).await;
    seed_image(&control, &image("jupiter", "alpha")).await;
    seed_image(&control, &image("jupiter", "beta")).await;
    run(&control, &[], 100).await;
    let expected = rows(&control).await;
    control.drop_keyspace().await;
    let (world, _shared) = Box::pin(writable("itt_cut")).await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    seed_image(&world, &image("jupiter", "beta")).await;
    // alpha's creation stops right after its instance is written
    faults::fail_after_instance_write("alpha");
    let ctx = world.ctx(RunMode::Run, 100);
    let runner = def().runner;
    runner.slices(1, &ctx).await.expect("slices");
    let key = format!("{}:image_data:jupiter:alpha", world.ns);
    let record = image_record(&world, &key).await;
    let outcome = runner
        .process(&record, RunMode::Run, &ctx)
        .await
        .expect("classifies");
    assert!(matches!(outcome, Outcome::Write(_)), "{outcome:?}");
    let cut = runner.write(&record, &ctx).await;
    assert!(matches!(cut, Err(MigrateError::Backend(_))), "{cut:?}");
    runner.finish(&ctx).await.expect("finishes");
    // the next run finishes alpha and writes beta
    let out = run(&world, &[], 100).await;
    assert_eq!(out.counts.written, 2, "{:?}", out.counts);
    assert_eq!(rows(&world).await, expected);
    // alpha's edge lost after a complete write is found missing and written again
    let alpha_instance = ToolIdMap::first_instance_id("jupiter", "alpha");
    let at = migrated_at();
    world
        .session()
        .query_unpaged(
            format!(
                "DELETE FROM {}.associations WHERE group = 'jupiter' AND year = {} AND bucket = {} \
                 AND source = ?",
                world.ns,
                chrono::Datelike::year(&at),
                thorium::utils::helpers::partition(
                    at,
                    chrono::Datelike::year(&at),
                    world.conf.thorium.associations.partition_size
                ),
            ),
            (
                serde_json::to_string(&thorium::models::AssociationTargetColumn::Entity(
                    alpha_instance,
                ))
                .unwrap(),
            ),
        )
        .await
        .expect("the edge rows delete");
    let cut = check(&world).await.report.counts;
    assert_eq!(cut.migratable, 1, "{cut:?}");
    assert_eq!(cut.current, 1, "{cut:?}");
    let out = run(&world, &[], 100).await;
    assert_eq!(out.counts.written, 1, "{:?}", out.counts);
    assert_eq!(out.counts.current, 1, "{:?}", out.counts);
    // the rows are the same ones again; only a second event for the tool was queued
    let mut again = rows(&world).await;
    assert_eq!(again.remove("events"), Some(expected["events"] + 1));
    let mut same = expected.clone();
    same.remove("events");
    assert_eq!(again, same);
    world.drop_keyspace().await;
}

/// Read an image's hash as a record
///
/// # Arguments
///
/// * `world` - The test world
/// * `key` - The image's data key
async fn image_record(world: &World, key: &str) -> Record {
    // the hash the migration's batches read
    let mut conn = world.backends.redis.get().await.expect("a connection");
    let fields: BTreeMap<String, String> = redis::cmd("HGETALL")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .expect("the image reads");
    Record::Redis {
        key: key.to_owned(),
        fields,
    }
}

/// A runner that cancels the run after some images are written, to stop a pass partway through
struct StopAfterWrites {
    /// The migration it wraps
    inner: &'static dyn MigrationRunner,
    /// How many writes to allow before cancelling
    after: usize,
    /// How many writes were made
    seen: AtomicUsize,
    /// The token the run's context holds
    cancel: CancellationToken,
}

#[async_trait]
impl MigrationRunner for StopAfterWrites {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        self.inner.slices(n, ctx).await
    }
    /// Read a batch as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        self.inner.next_batch(slice, after, ctx).await
    }
    /// Classify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        self.inner.process(record, mode, ctx).await
    }
    /// Write, then cancel the run once enough images are written
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.write(record, ctx).await?;
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 >= self.after {
            self.cancel.cancel();
        }
        Ok(())
    }
    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.finish(ctx).await
    }
}

/// A pass stopped partway resumes from its checkpoint and ends with the rows and counts an
/// uninterrupted pass has
#[tokio::test]
async fn an_interrupted_pass_resumes_to_the_same_result() {
    let names = ["a", "b", "c", "d", "e", "f"];
    // the control: one uninterrupted pass
    let (control, _control_shared) = Box::pin(writable("itt_res_c")).await;
    for name in names {
        seed_image(&control, &image("jupiter", name)).await;
    }
    let whole = run(&control, &[], 2).await;
    let expected = rows(&control).await;
    control.drop_keyspace().await;
    // a pass cancelled after its second image, in batches of two
    let (world, _shared) = Box::pin(writable("itt_res")).await;
    for name in names {
        seed_image(&world, &image("jupiter", name)).await;
    }
    let cancel = CancellationToken::new();
    let runner = StopAfterWrites {
        inner: def().runner,
        after: 2,
        seen: AtomicUsize::new(0),
        cancel: cancel.clone(),
    };
    let mut ctx = world.ctx(RunMode::Run, 2);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let options = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let first = run_pass(
        &runner,
        &World::spec(def()),
        &options,
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the pass runs");
    lease.release().await;
    assert!(first.interrupted, "{first:?}");
    // the next run resumes the attempt and finishes it
    let second = run(&world, &[], 2).await;
    assert!(second.resumed, "{second:?}");
    assert_eq!(second.attempt, first.attempt);
    // the resumed pass's counts include what the interrupted run had already written
    assert!(first.counts.written < 6, "{first:?}");
    assert_eq!(second.counts.written, 6, "{second:?}");
    assert_eq!(second.counts, whole.counts);
    assert_eq!(rows(&world).await, expected);
    world.drop_keyspace().await;
}

/// An image the publish refuses is recorded as a `write_failed` issue and nothing of it is left,
/// and the images around it still migrate
#[tokio::test]
async fn an_image_the_publish_refuses_is_an_issue_and_leaves_no_tool() {
    let (world, _shared) = Box::pin(writable("itt_refused")).await;
    seed_image(&world, &image("jupiter", "alpha")).await;
    // the tool's volume names start with the prefix Thorium reserves, which the publish refuses
    let mut reserved = image("jupiter", "reserved");
    reserved.volumes = vec![Volume {
        name: "thorium-cache".into(),
        archetype: VolumeTypes::ConfigMap,
        mount_path: "/cache".into(),
        sub_path: None,
        read_only: true,
        kustomize: false,
        host_path: None,
        config_map: None,
        secret: None,
        nfs: None,
    }];
    seed_image(&world, &reserved).await;
    let out = run(&world, &[], 100).await;
    assert_eq!(out.counts.written, 1, "{:?}", out.counts);
    assert_eq!(out.counts.error, 1, "{:?}", out.counts);
    let codes = out
        .groups
        .iter()
        .map(|group| (group.reason.clone(), group.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(codes.get("write_failed:rejected"), Some(&1), "{codes:?}");
    // the refused image has no tool under its deterministic id
    let ns = &world.ns;
    let id = ToolIdMap::tool_id("jupiter", "reserved");
    let found = world
        .session()
        .query_unpaged(
            format!("SELECT id FROM {ns}.entities_by_id WHERE id = {id} AND group = 'jupiter'"),
            (),
        )
        .await
        .expect("the view reads")
        .into_rows_result()
        .expect("rows")
        .rows_num();
    assert_eq!(found, 0);
    world.drop_keyspace().await;
}

/// Images the conversion skips are issues with their codes and nothing is written for them, a
/// group of one code over the limit fails the pass, and the approval's accepted code lets the
/// same pass finish with the convertible images written
#[tokio::test]
async fn skipped_images_follow_the_group_limit_and_the_accepted_codes() {
    // six images without a container image is over the limit of five
    let (world, _shared) = Box::pin(writable("itt_limit")).await;
    seed_image(&world, &image("jupiter", "good")).await;
    for n in 0..6 {
        let mut bare = image("jupiter", &format!("bare{n}"));
        bare.image = None;
        seed_image(&world, &bare).await;
    }
    let failed = run(&world, &[], 100).await;
    assert_eq!(failed.status, MigrationRunStatus::Failed, "{failed:?}");
    // accepting the code finishes the pass: the six are issues, the good image is a tool
    let out = run(&world, &["missing_field:image"], 100).await;
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues, "{out:?}");
    assert_eq!(out.counts.flagged, 6, "{:?}", out.counts);
    assert_eq!(out.counts.written, 1, "{:?}", out.counts);
    assert_eq!(
        out.report.groups["images-to-tools"].changed, 1,
        "{:?}",
        out.report
    );
    assert_eq!(
        out.report.groups["images-to-tools"].skipped, 6,
        "{:?}",
        out.report
    );
    assert_eq!(out.report.skipped_total, 6, "{:?}", out.report);
    // the good image is the one item the report counts as mapped to a tool
    assert_eq!(out.report.mappings_total, 1, "{:?}", out.report);
    let codes = out
        .groups
        .iter()
        .map(|group| (group.reason.clone(), group.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(codes.get("missing_field:image"), Some(&6), "{codes:?}");
    // the six skipped images wrote nothing: the one event is the good image's
    let after = rows(&world).await;
    assert_eq!(after["events"], 1, "{after:?}");
    world.drop_keyspace().await;
}

/// A run's report carries the notes and warnings for every image it wrote, naming a secret-like
/// variable and never its value
#[tokio::test]
async fn a_run_reports_its_notes_and_warnings_without_values() {
    let (world, _shared) = Box::pin(writable("itt_runnotes")).await;
    let mut secret = image("jupiter", "secretive");
    secret.env.insert(
        "API_TOKEN".to_owned(),
        Some("super-secret-value".to_owned()),
    );
    seed_image(&world, &secret).await;
    let out = run(&world, &[], 100).await;
    assert_eq!(out.counts.written, 1, "{:?}", out.counts);
    let notes = format!("{:?}", out.report.notes);
    assert!(notes.contains("API_TOKEN"), "{notes}");
    assert!(!notes.contains("super-secret-value"), "{notes}");
    assert!(out.report.warnings_total >= 1, "{:?}", out.report);
    world.drop_keyspace().await;
}

/// Held by the tests that run the pass over the harness's shared namespace, because a second pass
/// would migrate the images of the first and take its lease
static HARNESS_PASS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A world over the test harness's namespace, where a running API serves what the writer writes
///
/// # Arguments
///
/// * `harness` - The harness whose namespace and API to use
async fn harness_world(harness: &Harness) -> World {
    // connect to the same backends the harness's API uses
    let conf = harness.conf.clone();
    let ns = conf.thorium.namespace.clone();
    let session = Arc::new(
        thorium::models::schema::connect(&conf)
            .await
            .expect("Scylla is reachable"),
    );
    let redis = common::harness::redis_pool(&conf)
        .await
        .expect("Redis is reachable");
    let backends = RunBackends {
        partition_sizes: thoradm::migrate::context::partition_sizes(&conf),
        namespace: ns.clone(),
        scylla: session.clone(),
        redis,
        s3: thoradm::migrate::context::s3_client(&conf),
        conf: Arc::new(conf.clone()),
    };
    let store = PassStore::prepare(session, &ns)
        .await
        .expect("statements prepare");
    World {
        ns,
        conf,
        backends,
        store,
    }
}

/// A tool a run wrote is served by the running API: it reads back by id with its first instance,
/// tag, and the image's settings, and the instance is listed under the tool
#[tokio::test]
async fn a_migrated_tool_reads_back_through_the_api() {
    // one pass at a time runs over the harness's namespace
    let _serial = HARNESS_PASS.lock().await;
    // an API with a group, and an image in that group that only Redis holds
    let harness = Harness::get().await;
    let world = harness_world(harness).await;
    let client = Cluster::of_harness(harness).client().await;
    let group = Names::new().group("vesta");
    seed::group(&client, &group).await;
    let mut legacy = image(&group, "scanner");
    ADMIN.clone_into(&mut legacy.creator);
    legacy.description = Some("scans files".to_owned());
    seed_image(&world, &legacy).await;
    let out = run(&world, &[], 100).await;
    assert_eq!(out.counts.written, 1, "{out:?}");
    // the API serves the tool by its deterministic id
    let tool = client
        .entities
        .get(ToolIdMap::tool_id(&group, "scanner"))
        .await
        .expect("the API serves the tool");
    assert_eq!(tool.name, "scanner");
    assert_eq!(tool.description.as_deref(), Some("scans files"));
    assert_eq!(tool.submitter, ADMIN);
    let first = ToolIdMap::first_instance_id(&group, "scanner");
    let EntityMetadata::Tool(root) = &tool.metadata else {
        panic!("a tool: {:?}", tool.metadata);
    };
    assert_eq!(root.latest, Some(first));
    assert_eq!(root.instances.len(), 1, "{:?}", root.instances);
    assert!(
        tool.tags.contains_key(MIGRATED_FROM_IMAGE_TAG),
        "{:?}",
        tool.tags
    );
    // and the first instance with the image's workload
    let instance = client
        .entities
        .get(first)
        .await
        .expect("the API serves the instance");
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        panic!("an instance: {:?}", instance.metadata);
    };
    assert_eq!(meta.tool, tool.id);
    let ToolSpec::Image(workload) = &meta.spec else {
        panic!("an image spec: {:?}", meta.spec);
    };
    assert_eq!(*workload, WorkloadSpec::from_image(&legacy).unwrap().spec);
}

/// A legacy image named like an entity kind converts to a tool of that name, which the API keeps
/// serving, updating, and deleting even though it refuses to create a new tool with that name
#[tokio::test]
async fn an_image_with_a_protected_name_converts_and_keeps_working() {
    // one pass at a time runs over the harness's namespace
    let _serial = HARNESS_PASS.lock().await;
    // an API with a group, and images in it named like entity kinds that only Redis holds
    let harness = Harness::get().await;
    let world = harness_world(harness).await;
    let client = Cluster::of_harness(harness).client().await;
    let group = Names::new().group("pallas");
    seed::group(&client, &group).await;
    for name in ["tool", "workflows"] {
        let mut legacy = image(&group, name);
        ADMIN.clone_into(&mut legacy.creator);
        seed_image(&world, &legacy).await;
    }
    let out = run(&world, &[], 100).await;
    assert_eq!(out.counts.written, 2, "{out:?}");
    assert_eq!(out.counts.flagged, 0, "{out:?}");
    for name in ["tool", "workflows"] {
        // the converted tool reads back under its name
        let id = ToolIdMap::tool_id(&group, name);
        let tool = client
            .entities
            .get(id)
            .await
            .expect("the API serves the tool");
        assert_eq!(tool.name, name);
        // an update that repeats the name still works
        let update = EntityUpdate::default().name(name).description("kept");
        client
            .entities
            .update(id, update)
            .await
            .expect("an update keeps working");
        let tool = client.entities.get(id).await.expect("the tool reads");
        assert_eq!(tool.description.as_deref(), Some("kept"));
        // a new tool can't take the name, and the refusal names the word and the list
        let mut req = generators::gen_tool(&[&group], "2.0.0", &generators::gen_image_spec());
        name.clone_into(&mut req.name);
        let err = client
            .entities
            .create(req)
            .await
            .expect_err("a new tool can't take the name");
        assert_eq!(err.status().map(|status| status.as_u16()), Some(400));
        assert!(
            err.msg()
                .is_some_and(|msg| msg.contains(&protected_names::refusal(name))),
            "{err}"
        );
        // the converted tool still deletes once its instance is gone
        let instance = ToolIdMap::first_instance_id(&group, name);
        client
            .entities
            .delete(instance)
            .await
            .expect("the instance deletes");
        client.entities.delete(id).await.expect("the tool deletes");
    }
}

/// The report notes who each tool is published as, its version, the fields its environment drops,
/// the dropped dependency names, the bans, and the env variable names that look like secrets,
/// never a value
#[tokio::test]
async fn notes_and_warnings_name_things_but_never_values() {
    let world = world("itt_notes").await;
    // io is in jupiter; nobody is in callisto, so its images are published as the operator
    let mut secret = image("jupiter", "secretive");
    secret.env.insert(
        "API_TOKEN".to_owned(),
        Some("super-secret-value".to_owned()),
    );
    seed_image(&world, &secret).await;
    seed_image(&world, &image("callisto", "orphan")).await;
    let mut partial = image("jupiter", "partial");
    partial.dependencies.results.images = vec!["secretive".to_owned(), "ghost".to_owned()];
    seed_image(&world, &partial).await;
    // a check finishes the report itself, and a hand-run check prints it in full
    let report = check(&world).await.applied;
    let notes = report
        .notes
        .iter()
        .map(|note| format!("{}/{}: {}", note.group, note.item, note.note))
        .collect::<Vec<_>>()
        .join("\n");
    // the secret-like variable is named in a warning, its value nowhere
    assert!(notes.contains("API_TOKEN"), "{notes}");
    assert!(!notes.contains("super-secret-value"), "{notes}");
    assert!(report.warnings_total >= 1, "{report:?}");
    // an image in a group nobody belongs to is published as the operator
    assert!(
        notes.contains("callisto/orphan: published as the operator account"),
        "{notes}"
    );
    // a member of the group publishes their own image
    assert!(
        notes.contains("jupiter/secretive: published as its creator `io`"),
        "{notes}"
    );
    // the dropped dependency name is a note
    assert!(
        notes.contains("jupiter/partial: dropped `ghost`"),
        "{notes}"
    );
    // each tag is the one that finds the migrated tool again
    assert_eq!(MIGRATED_FROM_IMAGE_TAG, "MigratedFromImage");
}

/// An image flagged by a run, fixed by hand, and named by its issue key with `--objects` is
/// converted and leaves its issue group
#[tokio::test]
async fn an_objects_run_converts_a_fixed_image_and_credits_its_group() {
    // a convertible image and one whose only dependency is missing, which the run flags
    let (world, _shared) = Box::pin(writable("itt_objects")).await;
    seed_image(&world, &image("jupiter", "good")).await;
    let mut lonely = image("jupiter", "lonely");
    lonely.dependencies.results.images = vec!["ghost".to_owned()];
    seed_image(&world, &lonely).await;
    let code = "invalid_value:dependencies.results.images";
    let first = run(&world, &[code], 100).await;
    assert_eq!(
        first.status,
        MigrationRunStatus::DoneWithIssues,
        "{first:?}"
    );
    let group = &first.groups[0];
    assert!(group.samples.contains("jupiter/lonely"), "{group:?}");
    // the image is fixed by hand and named by the key its issue group lists
    seed_image(&world, &image("jupiter", "lonely")).await;
    let spec = thoradm::migrate::context::spec_for(def(), [code.to_owned()], None);
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 100);
    let options = RunOptions {
        slices: 1,
        objects: vec!["jupiter/lonely".to_owned()],
        reason: None,
    };
    let out = run_pass(def().runner, &spec, &options, &ctx, &world.store, &lease)
        .await
        .expect("the objects run");
    lease.release().await;
    // the image was read and written, and its group credits it
    assert_eq!(out.counts.written, 1, "{:?}", out.counts);
    let group = &out.groups[0];
    assert_eq!((group.count, group.resolved), (0, 1), "{group:?}");
    assert!(!group.samples.contains("jupiter/lonely"), "{group:?}");
    world.drop_keyspace().await;
}
