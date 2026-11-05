//! Tests that a tool instance with a ban never runs: an image's bans carry to the instance its
//! conversion makes, an admin bans an instance and lifts its bans, a ban follows the tool's new
//! versions and the instance can't be detached from its tool, by deleting its edge or by moving
//! the tool to another group, and every run path refuses a banned instance

mod common;

use common::cluster::Cluster;
use common::harness::{ADMIN, Harness};
use common::migrate::World;
use common::names::Names;
use common::seed;
use std::collections::HashMap;
use std::sync::Arc;
use thoradm::migrate::pipelines_to_tool_stages::PIPELINES_TO_TOOL_STAGES;
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thorium::Thorium;
use thorium::client::migrations::ToolIdMap;
use thorium::models::schema::{PassOutcome, PassStore, RunBackends, RunMode, RunOptions, run_pass};
use thorium::models::{
    AssociationDeleteRequest, AssociationKind, AssociationListOpts, AssociationTarget,
    EntityMetadata, EntityMetadataUpdate, EntityUpdate, GroupAllowed, Image, ImageBan,
    ImageBanKind, ImageScaler, PipelineBan, PipelineBanKind, PipelineRequest, PipelineUpdate,
    ReactionRequest, RunInputs, ToolRunRequest, ToolSpec, UserRole, WorkloadEnvironment,
    WorkloadSpec,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

/// Held by every test, because each runs migration passes or scans over the harness's one
/// namespace, which would take another test's lease or change its settings
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Get a registered migration by id
///
/// # Arguments
///
/// * `id` - The migration's id
fn migration(id: &str) -> &'static MigrationDef {
    // the migration is registered like every other
    MIGRATIONS
        .iter()
        .find(|def| def.id == id)
        .unwrap_or_else(|| panic!("{id} is registered"))
}

/// Run one pass of a migration over the world to the end in one slice
///
/// # Arguments
///
/// * `world` - The test world
/// * `def` - The migration to run
async fn run(world: &World, def: &'static MigrationDef) -> PassOutcome {
    // a pass with no accepted codes, released when done
    let spec = thoradm::migrate::context::spec_for(def, std::iter::empty::<String>(), None);
    let lease = world.lease("tool-instance-bans").await;
    let ctx = world.ctx(RunMode::Run, 100);
    let options = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let out = run_pass(def.runner, &spec, &options, &ctx, &world.store, &lease)
        .await
        .expect("the pass runs");
    lease.release().await;
    out
}

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

/// Run a Redis command against the world's Redis
///
/// # Arguments
///
/// * `world` - The test world
/// * `cmd` - The command to run
async fn redis_cmd<T: redis::FromRedisValue>(world: &World, cmd: redis::Cmd) -> T {
    // one pooled connection per command is plenty for a test
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    cmd.query_async::<T>(&mut *conn)
        .await
        .expect("the Redis command runs")
}

/// Build a K8s image created by the harness admin
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
    ADMIN.clone_into(&mut image.creator);
    image
}

/// Write an image, with its bans, the way the API before tools stored one
///
/// # Arguments
///
/// * `world` - The test world
/// * `image` - The image to store
async fn seed_image(world: &World, image: &Image) {
    let ns = &world.ns;
    let key = format!("{ns}:image_data:{}:{}", image.group, image.name);
    // every field the API's image creation writes, as JSON values
    let fields: Vec<(&str, String)> = vec![
        ("group", image.group.clone()),
        ("name", image.name.clone()),
        ("creator", image.creator.clone()),
        ("scaler", serde_json::to_string(&image.scaler).unwrap()),
        ("image", serde_json::to_string(&image.image).unwrap()),
        (
            "resources",
            serde_json::to_string(&image.resources).unwrap(),
        ),
        (
            "spawn_limit",
            serde_json::to_string(&image.spawn_limit).unwrap(),
        ),
        ("runtime", image.runtime.to_string()),
        ("volumes", serde_json::to_string(&image.volumes).unwrap()),
        ("env", serde_json::to_string(&image.env).unwrap()),
        ("args", serde_json::to_string(&image.args).unwrap()),
        (
            "security_context",
            serde_json::to_string(&image.security_context).unwrap(),
        ),
        (
            "collect_logs",
            serde_json::to_string(&image.collect_logs).unwrap(),
        ),
        (
            "generator",
            serde_json::to_string(&image.generator).unwrap(),
        ),
        (
            "dependencies",
            serde_json::to_string(&image.dependencies).unwrap(),
        ),
        (
            "display_type",
            serde_json::to_string(&image.display_type).unwrap(),
        ),
        (
            "output_collection",
            serde_json::to_string(&image.output_collection).unwrap(),
        ),
        (
            "child_filters",
            serde_json::to_string(&image.child_filters).unwrap(),
        ),
        (
            "network_policies",
            serde_json::to_string(&image.network_policies).unwrap(),
        ),
        ("bans", serde_json::to_string(&image.bans).unwrap()),
    ];
    let mut cmd = redis::cmd("HSET");
    cmd.arg(&key);
    for (field, value) in fields {
        cmd.arg(field).arg(value);
    }
    redis_cmd::<()>(world, cmd).await;
    // and its name in the group's set of images
    redis_cmd::<()>(
        world,
        redis::cmd("SADD")
            .arg(format!("{ns}:images:{}", image.group))
            .arg(&image.name)
            .to_owned(),
    )
    .await;
}

/// Write a pipeline the way the API before tools stored one: stage names only, with its bans
///
/// # Arguments
///
/// * `world` - The test world
/// * `group` - The pipeline's group
/// * `name` - The pipeline's name
/// * `order` - The pipeline's stages
/// * `bans` - The pipeline's bans
async fn seed_pipeline(
    world: &World,
    group: &str,
    name: &str,
    order: &[&[&str]],
    bans: &HashMap<Uuid, PipelineBan>,
) {
    let ns = &world.ns;
    // the pipeline's name in its group's set
    redis_cmd::<()>(
        world,
        redis::cmd("SADD")
            .arg(format!("{ns}:pipelines:{group}"))
            .arg(name)
            .to_owned(),
    )
    .await;
    // and its hash
    redis_cmd::<()>(
        world,
        redis::cmd("HSET")
            .arg(format!("{ns}:pipeline_data:{group}:{name}"))
            .arg("group")
            .arg(group)
            .arg("name")
            .arg(name)
            .arg("creator")
            .arg(ADMIN)
            .arg("order")
            .arg(serde_json::to_string(order).unwrap())
            .arg("sla")
            .arg("604800")
            .arg("triggers")
            .arg("{}")
            .arg("bans")
            .arg(serde_json::to_string(bans).unwrap())
            .to_owned(),
    )
    .await;
}

/// The ids of the bans a tool instance holds, as the API serves it
///
/// # Arguments
///
/// * `client` - An admin client
/// * `id` - The tool instance's id
async fn instance_ban_ids(client: &Thorium, id: Uuid) -> Vec<Uuid> {
    // read the instance and its bans field, which is left out when it has none
    let instance = client.entities.get(id).await.expect("the instance reads");
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        panic!("not a tool instance: {:?}", instance.metadata);
    };
    let json = serde_json::to_value(meta).expect("the instance serializes");
    let mut ids = json
        .get("bans")
        .and_then(|bans| bans.as_object())
        .map_or_else(Vec::new, |bans| {
            bans.keys()
                .map(|id| id.parse::<Uuid>().expect("a ban id"))
                .collect::<Vec<Uuid>>()
        });
    ids.sort();
    ids
}

/// Run a tool instance directly on a sample, returning the API's answer
///
/// # Arguments
///
/// * `client` - The client to run as
/// * `id` - The tool instance to run
/// * `sha` - The sample to run on
async fn run_tool(client: &Thorium, id: Uuid, sha: &str) -> Result<Uuid, String> {
    // a direct run on one sample
    client
        .reactions
        .run_tool(id, &ToolRunRequest::new(RunInputs::default().sample(sha)))
        .await
        .map(|created| created.id)
        .map_err(|err| err.to_string())
}

/// Create a reaction on a pipeline, returning the API's answer
///
/// # Arguments
///
/// * `client` - The client to run as
/// * `group` - The pipeline's group
/// * `pipeline` - The pipeline's name
/// * `sha` - The sample to run on
async fn react(client: &Thorium, group: &str, pipeline: &str, sha: &str) -> Result<Uuid, String> {
    // a reaction on one sample
    client
        .reactions
        .create(&ReactionRequest::new(group, pipeline).sample(sha))
        .await
        .map(|created| created.id)
        .map_err(|err| err.to_string())
}

/// Create reactions on a pipeline in bulk, returning whether the API created any
///
/// # Arguments
///
/// * `client` - The client to run as
/// * `group` - The pipeline's group
/// * `pipeline` - The pipeline's name
/// * `sha` - The sample to run on
async fn react_bulk(
    client: &Thorium,
    group: &str,
    pipeline: &str,
    sha: &str,
) -> Result<usize, String> {
    // a bulk request with one reaction
    let reqs = vec![ReactionRequest::new(group, pipeline).sample(sha)];
    client
        .reactions
        .create_bulk(&reqs)
        .await
        .map(|resp| resp.created.len())
        .map_err(|err| err.to_string())
}

/// Run the consistency scan the API also runs at startup
///
/// # Arguments
///
/// * `client` - An admin client
async fn scan(client: &Thorium) {
    // the manual scan route runs the same scan
    client
        .system
        .consistency_scan()
        .await
        .expect("the consistency scan runs");
}

/// A banned image converts to a tool instance that carries its ban, and no run path runs it: a
/// direct run, its converted pipeline, a new pipeline naming it, the pipeline with its stage
/// renamed, and a bulk reaction are all refused, before and after a consistency scan
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_banned_image_converts_to_an_instance_that_never_runs() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let world = harness_world(harness).await;
    let client = Cluster::of_harness(harness).client().await;
    let group = Names::new().group("banned");
    seed::group(&client, &group).await;
    // a legacy image an admin banned, and the pipeline the scan banned because of it
    let mut evil = image(&group, "evil");
    let ban = ImageBan::new(ImageBanKind::generic("exfiltrates samples"));
    let ban_id = ban.id;
    evil.bans.insert(ban.id, ban);
    seed_image(&world, &evil).await;
    let pban = PipelineBan::new(PipelineBanKind::image_ban("evil"));
    seed_pipeline(
        &world,
        &group,
        "evil-pipe",
        &[&["evil"]],
        &HashMap::from([(pban.id, pban)]),
    )
    .await;
    // the conversions
    let out = run(&world, migration("images-to-tools")).await;
    assert_eq!(out.counts.written, 1, "{out:?}");
    let out = run(&world, &PIPELINES_TO_TOOL_STAGES).await;
    assert_eq!(out.counts.written, 1, "{out:?}");
    // the instance carries the image's ban
    let instance = ToolIdMap::first_instance_id(&group, "evil");
    assert_eq!(instance_ban_ids(&client, instance).await, vec![ban_id]);
    let sha = seed::sample(&client, &group, "a.bin", b"banned image carry over").await;
    // a direct run is refused
    let err = run_tool(&client, instance, &sha)
        .await
        .expect_err("a banned instance's direct run is refused");
    assert!(err.contains("ban"), "{err}");
    // its converted pipeline is refused
    react(&client, &group, "evil-pipe", &sha)
        .await
        .expect_err("the banned pipeline's reaction is refused");
    // a new pipeline whose stage runs the instance is refused when it reacts
    let req = PipelineRequest::new(group.as_str(), "fresh-pipe", serde_json::json!([["scan"]]))
        .stage_tool("scan", instance);
    let created = client.pipelines.create(&req).await;
    if created.is_ok() {
        let err = react(&client, &group, "fresh-pipe", &sha)
            .await
            .expect_err("a reaction on a new pipeline running a banned instance is refused");
        assert!(err.contains("ban"), "{err}");
        react_bulk(&client, &group, "fresh-pipe", &sha)
            .await
            .expect_err("a bulk reaction running a banned instance is refused");
    }
    // renaming the converted pipeline's stage while keeping its tool still runs nothing
    let update = PipelineUpdate::default()
        .order(vec![vec!["renamed".to_owned()]])
        .add_stage_tool("renamed", instance);
    if client
        .pipelines
        .update(&group, "evil-pipe", &update)
        .await
        .is_ok()
    {
        let err = react(&client, &group, "evil-pipe", &sha)
            .await
            .expect_err("the renamed pipeline's reaction is refused");
        assert!(err.contains("ban"), "{err}");
    }
    // a consistency scan changes none of that
    scan(&client).await;
    run_tool(&client, instance, &sha)
        .await
        .expect_err("a banned instance's direct run is refused after a scan");
    react(&client, &group, "evil-pipe", &sha)
        .await
        .expect_err("the renamed pipeline's reaction is refused after a scan");
    assert_eq!(instance_ban_ids(&client, instance).await, vec![ban_id]);
}

/// Ban or lift the bans of a tool instance as an admin, returning the API's answer
///
/// # Arguments
///
/// * `client` - The client to update as
/// * `id` - The tool instance
/// * `ban` - The reason for a ban to add
/// * `unban` - The ids of the bans to lift
async fn set_bans(
    client: &Thorium,
    id: Uuid,
    ban: Option<&str>,
    unban: Vec<Uuid>,
) -> Result<(), String> {
    // a tool instance update that only changes its bans
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::ToolInstance {
        deprecated: None,
        ban: ban.map(str::to_owned),
        unban,
    });
    client
        .entities
        .update(id, update)
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// An admin bans a tool instance, which stops it running, and lifting the ban lets it run again;
/// lifting a ban the instance doesn't have is refused
#[tokio::test]
async fn an_admin_bans_and_lifts_a_tool_instance() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let client = Cluster::of_harness(harness).client().await;
    let group = Names::new().group("adminban");
    seed::group(&client, &group).await;
    let (_tool, instance) = generators::tool(
        &[group.as_str()],
        "1.0.0",
        &generators::gen_image_spec(),
        &client,
    )
    .await
    .expect("the tool publishes");
    let sha = seed::sample(&client, &group, "d.bin", b"admin bans").await;
    run_tool(&client, instance.id, &sha)
        .await
        .expect("an unbanned instance runs");
    // the admin bans it
    set_bans(
        &client,
        instance.id,
        Some("pulls an untrusted model"),
        Vec::new(),
    )
    .await
    .expect("the admin bans the instance");
    let bans = instance_ban_ids(&client, instance.id).await;
    assert_eq!(bans.len(), 1, "{bans:?}");
    let err = run_tool(&client, instance.id, &sha)
        .await
        .expect_err("a banned instance is refused");
    assert!(err.contains("pulls an untrusted model"), "{err}");
    // lifting a ban it doesn't have is refused and lifts nothing
    set_bans(&client, instance.id, None, vec![Uuid::new_v4()])
        .await
        .expect_err("lifting an unknown ban is refused");
    assert_eq!(instance_ban_ids(&client, instance.id).await, bans);
    // lifting its ban lets it run
    set_bans(&client, instance.id, None, bans)
        .await
        .expect("the admin lifts the ban");
    assert_eq!(
        instance_ban_ids(&client, instance.id).await,
        Vec::<Uuid>::new()
    );
    run_tool(&client, instance.id, &sha)
        .await
        .expect("a lifted instance runs");
}
/// A K8s image spec with one more CPU, so it publishes as a new version
///
/// # Arguments
///
/// * `spec` - The spec to change
fn more_cpu(spec: &ToolSpec) -> ToolSpec {
    // a changed resource request is the smallest change a new version needs
    let mut spec = spec.clone();
    let ToolSpec::Image(workload) = &mut spec else {
        panic!("not an image spec: {spec:?}")
    };
    let WorkloadEnvironment::K8s(env) = &mut workload.environment else {
        panic!("not a K8s spec")
    };
    env.scheduling.resources.cpu += 1;
    spec
}

/// Publish a new version of a tool, returning the new instance's id
///
/// # Arguments
///
/// * `client` - The client to publish as
/// * `tool` - The tool's id
/// * `version` - The new version's label
/// * `spec` - The new version's spec
async fn publish(client: &Thorium, tool: Uuid, version: &str, spec: &ToolSpec) -> Uuid {
    // a tool update with a spec publishes a new instance
    client
        .entities
        .update(tool, generators::gen_tool_publish(version, spec))
        .await
        .expect("the new version publishes");
    // the new instance becomes the tool's latest
    let entity = client.entities.get(tool).await.expect("the tool reads");
    let EntityMetadata::Tool(meta) = &entity.metadata else {
        panic!("not a tool: {:?}", entity.metadata);
    };
    meta.latest.expect("the tool has a latest instance")
}

/// An admin's ban follows a tool's new versions: a group manager who publishes a changed version
/// of a banned instance gets a banned instance that neither a direct run nor a stage bound to the
/// tool runs, can't delete a banned instance, and a version published after an admin lifts the
/// latest instance's ban starts unbanned
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_ban_follows_the_tools_new_versions() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let client = Cluster::of_harness(harness).client().await;
    // a developer who manages the group publishes the tool
    let role = UserRole::Developer {
        k8s: true,
        bare_metal: false,
        windows: false,
        external: false,
        kvm: false,
    };
    let dev = generators::user_with_role(&client, role)
        .await
        .expect("the developer is created");
    let group = Names::new().group("newversion");
    let roles = generators::GroupRoles::default().manager(&dev.username);
    generators::named_group_with_roles(&group, &roles, GroupAllowed::default(), &client)
        .await
        .expect("the group is created");
    let spec = generators::gen_image_spec();
    let (tool, v1) = generators::tool(&[group.as_str()], "1.0.0", &spec, &dev.client)
        .await
        .expect("the tool publishes");
    let sha = seed::sample(&dev.client, &group, "e.bin", b"bans follow new versions").await;
    // the admin bans the latest instance
    set_bans(&client, v1.id, Some("SECURITY: exfiltrates"), Vec::new())
        .await
        .expect("the admin bans v1");
    let bans = instance_ban_ids(&client, v1.id).await;
    assert_eq!(bans.len(), 1, "{bans:?}");
    // the developer publishes a version with one field changed, which carries the ban
    let v2 = publish(&dev.client, tool.id, "1.0.1", &more_cpu(&spec)).await;
    assert_eq!(instance_ban_ids(&client, v2).await, bans);
    let err = run_tool(&dev.client, v2, &sha)
        .await
        .expect_err("the new version of a banned instance is refused");
    assert!(err.contains("SECURITY: exfiltrates"), "{err}");
    // a stage bound to the tool follows its latest instance and is refused too
    let req = PipelineRequest::new(group.as_str(), "latest-pipe", serde_json::json!([["s"]]))
        .stage_tool("s", tool.id);
    dev.client
        .pipelines
        .create(&req)
        .await
        .expect("a pipeline bound to the tool creates");
    let err = react(&dev.client, &group, "latest-pipe", &sha)
        .await
        .expect_err("a stage bound to a banned tool's latest instance is refused");
    assert!(err.contains("ban"), "{err}");
    // the developer can't delete a banned instance to publish past it
    dev.client
        .entities
        .delete(v1.id)
        .await
        .expect_err("a non-admin can't delete a banned instance");
    assert_eq!(instance_ban_ids(&client, v1.id).await, bans);
    // once the admin lifts the latest instance's ban it runs, and versions after it start
    // unbanned
    set_bans(&client, v2, None, bans.clone())
        .await
        .expect("the admin lifts v2's ban");
    run_tool(&dev.client, v2, &sha)
        .await
        .expect("a lifted instance runs");
    let v3 = publish(&dev.client, tool.id, "1.0.2", &more_cpu(&more_cpu(&spec))).await;
    assert_eq!(instance_ban_ids(&client, v3).await, Vec::<Uuid>::new());
    // the version the ban was placed on keeps it
    assert_eq!(instance_ban_ids(&client, v1.id).await, bans);
}

/// A banned instance can't be detached from its tool: neither its publisher, an ordinary group
/// member, nor an admin can delete its `PublishedFrom` edge from either end, so the tool's next
/// version still inherits the ban
#[tokio::test]
async fn a_banned_instance_stays_linked_to_its_tool() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let client = Cluster::of_harness(harness).client().await;
    // an ordinary member of the group publishes the tool
    let role = UserRole::Developer {
        k8s: true,
        bare_metal: false,
        windows: false,
        external: false,
        kvm: false,
    };
    let dev = generators::user_with_role(&client, role)
        .await
        .expect("the developer is created");
    let group = Names::new().group("edgedelete");
    let roles = generators::GroupRoles::default().user(&dev.username);
    generators::named_group_with_roles(&group, &roles, GroupAllowed::default(), &client)
        .await
        .expect("the group is created");
    let spec = generators::gen_image_spec();
    let (tool, v1) = generators::tool(&[group.as_str()], "1.0.0", &spec, &dev.client)
        .await
        .expect("the tool publishes");
    // the admin bans the instance
    set_bans(&client, v1.id, Some("SECURITY: edge delete"), Vec::new())
        .await
        .expect("the admin bans v1");
    let bans = instance_ban_ids(&client, v1.id).await;
    assert_eq!(bans.len(), 1, "{bans:?}");
    // the edge can't be deleted from the instance's end or the tool's, by anyone
    let instance_end = AssociationTarget::Entity {
        id: v1.id,
        name: v1.name.clone(),
    };
    let tool_end = AssociationTarget::Entity {
        id: tool.id,
        name: tool.name.clone(),
    };
    let requests = [
        AssociationDeleteRequest::new(AssociationKind::PublishedFrom, instance_end.clone())
            .target(tool_end.clone()),
        AssociationDeleteRequest::new(AssociationKind::PublishedFrom, tool_end)
            .target(instance_end),
    ];
    for req in &requests {
        for (who, user) in [("the publisher", &dev.client), ("an admin", &client)] {
            let err = user
                .associations
                .delete(req)
                .await
                .expect_err("deleting a PublishedFrom edge is refused");
            assert_eq!(
                err.status().map(|status| status.as_u16()),
                Some(400),
                "{who}: {err}"
            );
        }
    }
    // the instance is still the tool's, so the next version inherits its ban
    let v2 = publish(&dev.client, tool.id, "1.0.1", &more_cpu(&spec)).await;
    assert_eq!(instance_ban_ids(&client, v2).await, bans);
}

/// List the ids at the other end of an entity's `PublishedFrom` edges in one group
///
/// # Arguments
///
/// * `client` - An admin client
/// * `id` - The entity whose edges to list
/// * `group` - The group to list the edges in
async fn published_from_in(client: &Thorium, id: Uuid, group: &str) -> Vec<Uuid> {
    // list the entity's edges stored in this group
    let opts = AssociationListOpts::default()
        .groups(vec![group.to_owned()])
        .limit(500);
    let cursor = client
        .associations
        .list_entity(id, &opts)
        .await
        .expect("the edges list");
    // keep the other end of each PublishedFrom edge
    let mut others = cursor
        .data
        .iter()
        .filter(|assoc| assoc.kind == AssociationKind::PublishedFrom)
        .map(|assoc| match &assoc.other {
            AssociationTarget::Entity { id, .. } => *id,
            other => panic!("a PublishedFrom edge to a non-entity: {other:?}"),
        })
        .collect::<Vec<Uuid>>();
    others.sort();
    others
}

/// Moving a tool to another group in one update moves its instances' `PublishedFrom` edges with
/// it: the tool's submitter, an ordinary member of both groups, adds one group and removes the
/// other, the edges end up only in the new group, and the tool's next version still inherits the
/// ban
#[tokio::test]
async fn a_group_swap_keeps_a_banned_instance_linked() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::get().await;
    let client = Cluster::of_harness(harness).client().await;
    // an ordinary member of two groups publishes the tool in the first
    let role = UserRole::Developer {
        k8s: true,
        bare_metal: false,
        windows: false,
        external: false,
        kvm: false,
    };
    let dev = generators::user_with_role(&client, role)
        .await
        .expect("the developer is created");
    let names = Names::new();
    let (old, new) = (names.group("swapold"), names.group("swapnew"));
    let roles = generators::GroupRoles::default().user(&dev.username);
    for group in [&old, &new] {
        generators::named_group_with_roles(group, &roles, GroupAllowed::default(), &client)
            .await
            .expect("the group is created");
    }
    let spec = generators::gen_image_spec();
    let (tool, v1) = generators::tool(&[old.as_str()], "1.0.0", &spec, &dev.client)
        .await
        .expect("the tool publishes");
    // the admin bans the instance
    set_bans(&client, v1.id, Some("SECURITY: group swap"), Vec::new())
        .await
        .expect("the admin bans v1");
    let bans = instance_ban_ids(&client, v1.id).await;
    assert_eq!(bans.len(), 1, "{bans:?}");
    // the developer moves the tool to the new group in one update
    dev.client
        .entities
        .update(
            tool.id,
            EntityUpdate::default()
                .group(new.as_str())
                .remove_group(old.as_str()),
        )
        .await
        .expect("the tool moves to the new group");
    for id in [tool.id, v1.id] {
        let entity = client.entities.get(id).await.expect("the entity reads");
        assert_eq!(entity.groups, vec![new.clone()], "{id}");
    }
    // the edge is in the new group from both ends and gone from the old one
    assert_eq!(published_from_in(&client, tool.id, &new).await, vec![v1.id]);
    assert_eq!(published_from_in(&client, v1.id, &new).await, vec![tool.id]);
    assert_eq!(
        published_from_in(&client, tool.id, &old).await,
        Vec::<Uuid>::new()
    );
    assert_eq!(
        published_from_in(&client, v1.id, &old).await,
        Vec::<Uuid>::new()
    );
    // the next version inherits the ban and is refused
    let v2 = publish(&dev.client, tool.id, "1.0.1", &more_cpu(&spec)).await;
    assert_eq!(instance_ban_ids(&client, v2).await, bans);
    let sha = seed::sample(&dev.client, &new, "swap.bin", b"group swap keeps the ban").await;
    let err = run_tool(&dev.client, v2, &sha)
        .await
        .expect_err("the new version of a banned instance is refused");
    assert!(err.contains("SECURITY: group swap"), "{err}");
}
