//! Tests of the `resubmit-reactions` migration driven in-process over the test harness's
//! namespace: it resubmits each recorded request of the drain's attempt once as its creator,
//! refuses what the create route or the identity checks refuse, binds and recreates reservations
//! only with the in-pause flag, keeps transient failures for the next run, and never logs or
//! prints a request's args

mod common;

use chrono::{DateTime, Duration, Utc};
use common::harness::{ADMIN, Harness};
use common::migrate::World;
use common::names::Names;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thoradm::migrate::{ResubmitFault, arm_resubmit_fault, reserved_id};
use thorium::Thorium;
use thorium::models::schema::drain_requests::{
    CreatorMembership, DrainRequest, DrainRequestRow, DrainRequestStore, DrainStatus,
    PipelineIdentity, args_sha256,
};
use thorium::models::schema::{
    MigrateError, PassOutcome, PassStore, RecordStatus, RunBackends, RunFlags, RunMode, RunOptions,
    check_pass, read_status, run_pass, seed_from_state,
};
use thorium::models::upgrades::{AppliedStep, Catalog, RESTORED_STEP, RevisionId, StepState};
use thorium::models::{
    GenericJobArgs, GroupAllowed, GroupUpdate, GroupUsersUpdate, Pipeline, PipelineBan,
    PipelineBanKind, PipelineBanUpdate, PipelineUpdate, ReactionArgs, ReactionStatus, UserRole,
    UserUpdate,
};
use thorium::test_utilities::generators::{self, GroupRoles, TestUser};
use uuid::Uuid;

/// The migration's id
const ID: &str = "resubmit-reactions";

/// The revision whose drain records the requests
const V04: &str = "2026-10-v04";

/// The `at` of the drain step's `Running` entry of the attempt the tests run in, spelled as the
/// operator writes it (fractional seconds, `+00:00`): the rows and the in-pause flag carry this
/// text while the record keeps the entry's time in its own normalized spelling
const ATTEMPT: &str = "2026-11-05T01:00:00.250+00:00";

/// The `at` of the drain step's `Running` entry of an earlier, stopped attempt
const EARLIER: &str = "2026-11-04T01:00:00Z";

/// A positional argument value that must never reach a log line or a report
const ARG_SECRET: &str = "resubmit-secret-arg-7f3a";

/// Held by every test, since they all share the harness's namespace and its record
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The migration under test
fn def() -> &'static MigrationDef {
    // the registry entry thoradm runs
    MIGRATIONS
        .iter()
        .find(|def| def.id == ID)
        .expect("resubmit-reactions is registered")
}

/// Parse an RFC 3339 time
///
/// # Arguments
///
/// * `at` - The time's text
fn at(at: &str) -> DateTime<Utc> {
    // every time the tests use is valid
    DateTime::parse_from_rfc3339(at)
        .expect("a time")
        .with_timezone(&Utc)
}

/// A history entry of revision `2026-10-v04`
///
/// # Arguments
///
/// * `step` - The entry's step
/// * `outcome` - The step's state
/// * `when` - When it was written
fn entry(step: &str, outcome: StepState, when: DateTime<Utc>) -> AppliedStep {
    // only the fields the window reads
    AppliedStep {
        revision: V04.to_owned(),
        step: step.to_owned(),
        outcome,
        at: Some(when.to_rfc3339()),
        ..AppliedStep::default()
    }
}

/// A drain attempt that finished: its `Running` entry at `start` and its `Done` entry after it
///
/// # Arguments
///
/// * `start` - The `at` of the drain's `Running` entry
fn drain(start: &str) -> Vec<AppliedStep> {
    // the drain ran, holding the conversion pause, and finished ten minutes later
    let start = at(start);
    let mut running = entry("drain-reactions", StepState::Running, start);
    running.paused.insert("api".to_owned(), 1);
    vec![
        running,
        entry(
            "drain-reactions",
            StepState::Done,
            start + Duration::minutes(10),
        ),
    ]
}

/// The history of the current attempt: the drain finished and so did `finalize-tool-only`, and
/// no paused component has been restored
fn window() -> Vec<AppliedStep> {
    // the drain, then the step that records the revision
    let mut history = drain(ATTEMPT);
    history.push(entry(
        "finalize-tool-only",
        StepState::Done,
        at(ATTEMPT) + Duration::minutes(20),
    ));
    history
}

/// A world over the harness's namespace, where the running API serves what the run writes
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

/// Run one CQL statement against the namespace's keyspace
///
/// # Arguments
///
/// * `world` - The test world
/// * `cql` - The statement
async fn cql(world: &World, cql: &str) {
    // the statements are the tests' own, so a failure fails the test
    world
        .session()
        .query_unpaged(cql, ())
        .await
        .unwrap_or_else(|err| panic!("{cql}: {err}"));
}

/// Remove every drain request, args row, history entry, recorded revision, and pass row of this
/// migration, then record a revision and a history
///
/// # Arguments
///
/// * `world` - The test world
/// * `revision` - The revision to record
/// * `history` - The history to record, oldest first
async fn reset(world: &World, revision: &str, history: &[AppliedStep]) {
    // the kinds the resubmission reads, whole partitions at a time
    let ns = &world.ns;
    for kind in ["drain_request", "drain_request_args", "history", "revision"] {
        cql(
            world,
            &format!("DELETE FROM {ns}.schema_migrations WHERE kind = '{kind}'"),
        )
        .await;
    }
    // this migration's pass rows, so every test starts a fresh pass
    let rows = world
        .session()
        .query_unpaged(
            format!("SELECT id FROM {ns}.schema_migrations WHERE kind = 'run'"),
            (),
        )
        .await
        .expect("the pass rows list")
        .into_rows_result()
        .expect("rows");
    let ids = rows
        .rows::<(String,)>()
        .expect("ids")
        .map(|row| row.expect("an id").0)
        .filter(|id| id.starts_with(ID))
        .collect::<Vec<_>>();
    for id in ids {
        cql(
            world,
            &format!("DELETE FROM {ns}.schema_migrations WHERE kind = 'run' AND id = '{id}'"),
        )
        .await;
    }
    // the revision and history the test runs against
    let revision = revision.parse::<RevisionId>().expect("a revision");
    seed_from_state(world.session(), &world.conf, &revision, "test", history)
        .await
        .expect("the record seeds");
}

/// One test's setup: the harness's namespace, an admin client, a group whose `users` role holds
/// a creator, and a converted pipeline in it
struct Scene {
    /// The world over the harness's namespace
    world: World,
    /// A client of the harness's admin
    admin: Thorium,
    /// The drain request rows
    store: DrainRequestStore,
    /// The names this test uses
    names: Names,
    /// The group the requests run in
    group: String,
    /// The creator of the requests, a plain user in the group's `users` role
    creator: TestUser,
    /// The converted pipeline the requests run
    pipeline: Pipeline,
    /// Keeps the other tests out of the shared namespace while this one runs
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

/// Build a test's scene over a recorded revision and history
///
/// # Arguments
///
/// * `revision` - The revision to record
/// * `history` - The history to record, oldest first
async fn scene_with(revision: &str, history: &[AppliedStep]) -> Scene {
    // one test at a time over the shared namespace, starting from an empty record
    let serial = SERIAL.lock().await;
    let harness = Harness::get_without_tablets().await;
    let world = harness_world(harness).await;
    reset(&world, revision, history).await;
    let admin = harness.client().await;
    let store = DrainRequestStore::prepare(world.backends.scylla.clone(), &world.ns)
        .await
        .expect("the drain statements prepare");
    // a creator in the users role of a group with a converted pipeline
    let names = Names::new();
    let creator = generators::named_user_with_role(&admin, &names.user("io"), UserRole::User)
        .await
        .expect("the creator is created");
    let roles = GroupRoles {
        users: vec![creator.username.clone()],
        ..GroupRoles::default()
    };
    let group = generators::named_group_with_roles(
        &names.group("ceres"),
        &roles,
        GroupAllowed::default(),
        &admin,
    )
    .await
    .expect("the group is created");
    let request = generators::pipelines(&group, 1, false, &admin)
        .await
        .expect("the pipeline is created")
        .remove(0);
    let pipeline = admin
        .pipelines
        .get(&group, &request.name)
        .await
        .expect("the pipeline reads");
    Scene {
        world,
        admin,
        store,
        names,
        group,
        creator,
        pipeline,
        _serial: serial,
    }
}

/// Build a test's scene over revision `2026-10-v04` and the current attempt's history
async fn scene() -> Scene {
    // the drain of the current attempt finished and so did the conversions
    Box::pin(scene_with(V04, &window())).await
}

/// The flags of the operator's Job created inside the conversion pause
fn in_pause() -> RunFlags {
    // the current attempt's drain `Running` entry
    RunFlags {
        in_pause: Some(ATTEMPT.to_owned()),
        ..RunFlags::default()
    }
}

/// The flags of a hand run that takes stale rows too
fn include_stale() -> RunFlags {
    // no in-pause flag
    RunFlags {
        include_stale: true,
        ..RunFlags::default()
    }
}

/// The argument JSON every request carries: one positional for the pipeline's first stage
///
/// # Arguments
///
/// * `pipeline` - The pipeline the request runs
fn args_json(pipeline: &Pipeline) -> String {
    // the secret value marks the args in any output
    let mut args = ReactionArgs::new();
    args.insert(
        pipeline.order[0][0].clone(),
        GenericJobArgs::default().positionals(vec![ARG_SECRET]),
    );
    serde_json::to_string(&args).expect("args serialize")
}

/// The pipeline's stage tool ids by stage, as a request's binding holds them
///
/// # Arguments
///
/// * `pipeline` - The converted pipeline
fn binding(pipeline: &Pipeline) -> Vec<Vec<String>> {
    // each stage's tool id, in the pipeline's order
    pipeline
        .order
        .iter()
        .map(|stage| {
            stage
                .iter()
                .map(|name| pipeline.stage_tools[name].to_string())
                .collect()
        })
        .collect()
}

/// A request the scene's creator made on the scene's pipeline, bound and with args
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `creator` - The request's creator
fn request(scene: &Scene, creator: &str) -> DrainRequest {
    // what the cancel records for a reaction the creator ran on the pipeline
    let args = args_json(&scene.pipeline);
    DrainRequest {
        group: scene.group.clone(),
        pipeline: scene.pipeline.name.clone(),
        samples: Vec::new(),
        repos: Vec::new(),
        tags: vec!["mars".to_owned()],
        parent: None,
        trigger_depth: None,
        creator: creator.to_owned(),
        old_status: ReactionStatus::Started,
        last_activity: Some(Utc::now()),
        orphan: false,
        args_sha256: Some(args_sha256(&args)),
        args_bytes: args.len() as u64,
        buffers: false,
        cache: false,
        identity: PipelineIdentity::of(&scene.pipeline),
        membership: CreatorMembership {
            held_role: true,
            role: "user".to_owned(),
        },
        binding: Some(binding(&scene.pipeline)),
    }
}

/// A request row as the cancel records it
///
/// # Arguments
///
/// * `id` - The cancelled reaction's id
/// * `request` - The request
/// * `status` - The row's status
/// * `attempt` - The drain attempt that recorded it
/// * `entry` - The reserved new reaction id, if any
fn row(
    id: Uuid,
    request: &DrainRequest,
    status: DrainStatus,
    attempt: Option<&str>,
    entry: Option<Uuid>,
) -> DrainRequestRow {
    // a row recorded an hour ago by the revision's drain
    DrainRequestRow {
        id: id.to_string(),
        value: request.to_json().expect("the request serializes"),
        attempt: attempt.map(ToOwned::to_owned),
        status,
        entry: entry.map(|entry| entry.to_string()),
        result: None,
        revision: Some(V04.to_owned()),
        started: Some(Utc::now() - Duration::hours(1)),
        finished: None,
        updated: None,
    }
}

/// Record a request and its args row, returning the cancelled reaction's id
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `request` - The request
/// * `status` - The row's status
/// * `attempt` - The drain attempt that recorded it
/// * `entry` - The reserved new reaction id, if any
async fn seed_row(
    scene: &Scene,
    request: &DrainRequest,
    status: DrainStatus,
    attempt: Option<&str>,
    entry: Option<Uuid>,
) -> Uuid {
    // a fresh id for the cancelled reaction
    let id = Uuid::new_v4();
    seed_row_as(scene, id, request, status, attempt, entry).await;
    id
}

/// Record a request and its args row under a given id
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `id` - The cancelled reaction's id
/// * `request` - The request
/// * `status` - The row's status
/// * `attempt` - The drain attempt that recorded it
/// * `entry` - The reserved new reaction id, if any
async fn seed_row_as(
    scene: &Scene,
    id: Uuid,
    request: &DrainRequest,
    status: DrainStatus,
    attempt: Option<&str>,
    entry: Option<Uuid>,
) {
    // the row, then its args when it names some
    let inserted = scene
        .store
        .insert_if_absent(&row(id, request, status, attempt, entry))
        .await
        .expect("the row records");
    assert!(inserted, "{id} was recorded before");
    if request.args_sha256.is_some() {
        scene
            .store
            .put_args(&id.to_string(), &args_json(&scene.pipeline))
            .await
            .expect("the args record");
    }
}

/// Record an in-window request of the scene's creator, returning its id
///
/// # Arguments
///
/// * `scene` - The test's scene
async fn seed_recorded(scene: &Scene) -> Uuid {
    // the creator's bound request, recorded by the current attempt
    let request = request(scene, &scene.creator.username);
    seed_row(scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await
}

/// Run the migration in-process with the given flags, failing fast on a backend error
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `flags` - The run's flags
async fn run(scene: &Scene, flags: RunFlags) -> Result<PassOutcome, MigrateError> {
    // a single slice, no retries, and the writer lease for the run
    let spec = thoradm::migrate::context::spec_for(def(), std::iter::empty(), None);
    let lease = scene.world.lease("resubmit-test").await;
    let mut ctx = scene.world.ctx(RunMode::Run, 100);
    ctx.limits.retries = 0;
    ctx.flags = flags;
    let options = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let out = run_pass(
        def().runner,
        &spec,
        &options,
        &ctx,
        &scene.world.store,
        &lease,
    )
    .await;
    lease.release().await;
    out
}

/// Run the migration and expect it to finish
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `flags` - The run's flags
async fn run_ok(scene: &Scene, flags: RunFlags) -> PassOutcome {
    // a pass that fails fails the test
    run(scene, flags).await.expect("the resubmission runs")
}

/// Read a request's row
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `id` - The cancelled reaction's id
async fn get(scene: &Scene, id: Uuid) -> DrainRequestRow {
    // every row a test reads was recorded
    scene
        .store
        .get(&id.to_string())
        .await
        .expect("the row reads")
        .expect("the row exists")
}

/// Expect a row refused with a reason holding some text
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `id` - The cancelled reaction's id
/// * `reason` - Text the reason must hold
async fn assert_refused(scene: &Scene, id: Uuid, reason: &str) -> DrainRequestRow {
    // refused, with the reason in `result`
    let row = get(scene, id).await;
    assert_eq!(row.status, DrainStatus::Refused, "{row:?}");
    let result = row.result.clone().unwrap_or_default();
    assert!(
        result.to_lowercase().contains(&reason.to_lowercase()),
        "{reason:?} not in {result:?}"
    );
    row
}

/// Expect a row resubmitted, returning the new reaction's id
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `id` - The cancelled reaction's id
async fn assert_resubmitted(scene: &Scene, id: Uuid) -> Uuid {
    // resubmitted, naming the new id in both `entry` and `result`
    let row = get(scene, id).await;
    assert_eq!(row.status, DrainStatus::Resubmitted, "{row:?}");
    assert_eq!(row.entry, row.result, "{row:?}");
    row.result
        .as_deref()
        .expect("a new id")
        .parse()
        .expect("a uuid")
}

/// The ids of every reaction the scene's group holds, read from their data keys
///
/// # Arguments
///
/// * `scene` - The test's scene
async fn reactions(scene: &Scene) -> BTreeSet<String> {
    // the group is unique to the test
    let mut conn = scene
        .world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let prefix = format!("{}:reaction_data:{}:", scene.world.ns, scene.group);
    let keys = common::harness::scan_keys(&mut *conn, &format!("{prefix}*"))
        .await
        .expect("the keys scan");
    keys.into_iter()
        .filter_map(|key| key.strip_prefix(&prefix).map(ToOwned::to_owned))
        .collect()
}

/// Set a field of a Redis hash
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `key` - The hash's key, without the namespace
/// * `field` - The field
/// * `value` - The value
async fn hset(scene: &Scene, key: &str, field: &str, value: &str) {
    // the namespace prefixes every key
    let mut conn = scene
        .world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = redis::cmd("HSET")
        .arg(format!("{}:{key}", scene.world.ns))
        .arg(field)
        .arg(value)
        .query_async(&mut *conn)
        .await
        .expect("the field is set");
}

/// Create a user with a system role, without adding them to any group
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `moon` - The moon the user is named after
/// * `role` - The user's system role
async fn user(scene: &Scene, moon: &str, role: UserRole) -> TestUser {
    // a readable name unique to the test
    generators::named_user_with_role(&scene.admin, &scene.names.user(moon), role)
        .await
        .expect("the user is created")
}

/// Add a user to the scene's group in its `users` role
///
/// # Arguments
///
/// * `scene` - The test's scene
/// * `name` - The user's name
async fn add_user(scene: &Scene, name: &str) {
    // a direct member of the users role
    let update = GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(name));
    scene
        .admin
        .groups
        .update(&scene.group, &update)
        .await
        .expect("the group updates");
}

/// Read the newest history entry of the resubmission, if one was written
///
/// # Arguments
///
/// * `scene` - The test's scene
async fn resubmit_entry(scene: &Scene) -> Option<AppliedStep> {
    // the record holds the newest history entries, oldest first
    let status = read_status(
        scene.world.session(),
        &scene.world.conf,
        &Catalog::PRODUCTION,
    )
    .await
    .expect("the record reads");
    let RecordStatus::Recorded { record, .. } = status else {
        panic!("no record: {status:?}");
    };
    record
        .history
        .into_iter()
        .rev()
        .find(|entry| entry.step == ID)
}

/// Each recorded request of the attempt is resubmitted once as its creator, tagged with the old
/// id, with its args, no parent, and a status log line on the old reaction
#[tokio::test]
async fn resubmits_once_as_the_creator() {
    // a reaction the creator really made, cancelled and recorded by the current attempt
    let scene = Box::pin(scene()).await;
    let mut created = generators::gen_reaction(&scene.group, &scene.pipeline, None);
    created.args = serde_json::from_str(&args_json(&scene.pipeline)).expect("args parse");
    let old = scene
        .creator
        .client
        .reactions
        .create(&created)
        .await
        .expect("the old reaction is created")
        .id;
    let request = request(&scene, &scene.creator.username);
    seed_row_as(
        &scene,
        old,
        &request,
        DrainStatus::Recorded,
        Some(ATTEMPT),
        None,
    )
    .await;
    // the operator's run resubmits it
    let out = run_ok(&scene, in_pause()).await;
    assert_eq!(out.counts.written, 1, "{:?}", out.counts);
    let new = assert_resubmitted(&scene, old).await;
    assert_ne!(new, old);
    let reaction = scene
        .admin
        .reactions
        .get(&scene.group, new)
        .await
        .expect("the new reaction reads");
    assert_eq!(reaction.creator, scene.creator.username);
    assert_eq!(reaction.pipeline, scene.pipeline.name);
    assert_eq!(reaction.parent, None);
    assert!(
        reaction.tags.contains(&format!("resubmitted-from:{old}")),
        "{:?}",
        reaction.tags
    );
    assert!(
        reaction.tags.contains(&"mars".to_owned()),
        "{:?}",
        reaction.tags
    );
    let args: ReactionArgs = serde_json::from_str(&args_json(&scene.pipeline)).expect("args");
    assert_eq!(reaction.args, args);
    // the old reaction's status log names the new one
    let logs = scene
        .admin
        .reactions
        .status_logs(&scene.group, &old)
        .await
        .expect("the status logs read");
    assert!(
        logs.iter().any(|log| log
            .msg
            .as_deref()
            .is_some_and(|msg| msg.contains(&new.to_string()))),
        "{logs:?}"
    );
    // the run recorded its own history entry for the revision
    let entry = resubmit_entry(&scene).await.expect("a resubmission entry");
    assert_eq!(entry.outcome, StepState::Done);
    assert_eq!(entry.revision, V04);
    assert!(
        entry
            .work
            .as_deref()
            .unwrap_or_default()
            .contains("resubmitted"),
        "{entry:?}"
    );
    // a second run creates nothing
    let before = reactions(&scene).await;
    run_ok(&scene, in_pause()).await;
    assert_eq!(reactions(&scene).await, before);
    assert_eq!(assert_resubmitted(&scene, old).await, new);
}

/// A creator who can't read a sample of the request is refused with the create route's reason
#[tokio::test]
async fn refuses_a_creator_without_access() {
    // a sample in a group the creator isn't in
    let scene = Box::pin(scene()).await;
    let other = scene.names.group("vulcan");
    common::seed::group(&scene.admin, &other).await;
    let sha = common::seed::sample(&scene.admin, &other, "hidden", b"hidden sample").await;
    let mut request = request(&scene, &scene.creator.username);
    request.samples = vec![sha];
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // the request is refused and nothing is created
    run_ok(&scene, in_pause()).await;
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Refused, "{row:?}");
    assert!(row.result.is_some(), "{row:?}");
    assert!(reactions(&scene).await.is_empty());
}

/// A pipeline with a stage not converted to a tool is refused
#[tokio::test]
async fn refuses_an_unconverted_pipeline() {
    // an unbound request on a pipeline that lost its stage tools
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.binding = None;
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    hset(
        &scene,
        &format!("pipeline_data:{}:{}", scene.group, scene.pipeline.name),
        "stage_tools",
        "{}",
    )
    .await;
    // even the operator's run refuses it, and binds nothing
    run_ok(&scene, in_pause()).await;
    let row = assert_refused(&scene, id, "converted").await;
    assert_eq!(row.request().expect("a request").binding, None);
    assert!(reactions(&scene).await.is_empty());
}

/// A rerun after a resubmission finds the reserved id and creates nothing
#[tokio::test]
async fn a_rerun_finds_the_reserved_id_and_creates_nothing() {
    // one request resubmitted by the operator's run
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    run_ok(&scene, in_pause()).await;
    let new = assert_resubmitted(&scene, id).await;
    let before = reactions(&scene).await;
    assert_eq!(before, BTreeSet::from([new.to_string()]));
    // a hand run and another flagged run leave everything as it was
    let out = run_ok(&scene, RunFlags::default()).await;
    assert_eq!(out.counts.written, 0, "{:?}", out.counts);
    run_ok(&scene, in_pause()).await;
    assert_eq!(reactions(&scene).await, before);
    assert_eq!(assert_resubmitted(&scene, id).await, new);
}

/// A sub reaction whose parent is in the set is refused, since the parent's resubmission makes
/// it again; one whose parent isn't is resubmitted as a root
#[tokio::test]
async fn sub_reactions_are_not_resubmitted() {
    // a parent and its child, and a child whose parent finished before the cancel
    let scene = Box::pin(scene()).await;
    let parent = seed_recorded(&scene).await;
    let mut child = request(&scene, &scene.creator.username);
    child.parent = Some(parent);
    let child = seed_row(&scene, &child, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    let mut orphan = request(&scene, &scene.creator.username);
    orphan.parent = Some(Uuid::new_v4());
    let orphan = seed_row(&scene, &orphan, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // the parent and the orphaned child are resubmitted, the child is refused
    run_ok(&scene, in_pause()).await;
    assert_resubmitted(&scene, parent).await;
    assert_refused(&scene, child, "sub reaction").await;
    let root = assert_resubmitted(&scene, orphan).await;
    let reaction = scene
        .admin
        .reactions
        .get(&scene.group, root)
        .await
        .expect("the root reads");
    assert_eq!(reaction.parent, None);
    assert_eq!(reactions(&scene).await.len(), 2);
}

/// A disabled creator is refused with the create route's reason
#[tokio::test]
async fn refuses_a_disabled_creator() {
    // the creator was disabled after the cancel
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let disable = UserUpdate {
        password: None,
        email: None,
        role: Some(UserRole::Disabled),
        settings: None,
    };
    scene
        .admin
        .users
        .update(&scene.creator.username, disable)
        .await
        .expect("the user is disabled");
    // refused as disabled
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "disabled").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A creator whose email isn't verified is refused with the create route's reason
#[tokio::test]
async fn refuses_an_unverified_creator() {
    // the creator's email isn't verified
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    hset(
        &scene,
        &format!("user_data:{}", scene.creator.username),
        "verified",
        "false",
    )
    .await;
    // refused as unverified
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "verified").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A creator removed from the group since the cancel is refused
#[tokio::test]
async fn refuses_a_creator_outside_the_group() {
    // the creator left the group after the cancel
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let update = GroupUpdate::default()
        .users(GroupUsersUpdate::default().direct_remove(&scene.creator.username));
    scene
        .admin
        .groups
        .update(&scene.group, &update)
        .await
        .expect("the group updates");
    // refused for holding no role in the group
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "role").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A banned pipeline is refused with the create route's reason
#[tokio::test]
async fn refuses_a_banned_pipeline() {
    // the pipeline was banned after the cancel
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let ban = PipelineUpdate::default().bans(PipelineBanUpdate::default().add_ban(
        PipelineBan::new(PipelineBanKind::generic("Resubmission test ban!")),
    ));
    scene
        .admin
        .pipelines
        .update(&scene.group, &scene.pipeline.name, &ban)
        .await
        .expect("the pipeline is banned");
    // refused for the ban
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "ban").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A reaction someone else tagged with the old id doesn't stop the resubmission
#[tokio::test]
async fn a_forged_tag_does_not_suppress_the_resubmission() {
    // another member creates a reaction tagged as if it were the resubmission
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let member = user(&scene, "europa", UserRole::User).await;
    add_user(&scene, &member.username).await;
    let tagged = generators::gen_reaction(
        &scene.group,
        &scene.pipeline,
        Some(&format!("resubmitted-from:{id}")),
    );
    let forged = member
        .client
        .reactions
        .create(&tagged)
        .await
        .expect("the forged reaction is created")
        .id;
    // the request is still resubmitted, as a reaction of its own
    run_ok(&scene, in_pause()).await;
    let new = assert_resubmitted(&scene, id).await;
    assert_ne!(new, forged);
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([new.to_string(), forged.to_string()])
    );
}

/// A run before revision `2026-10-v04` is recorded is refused and leaves every row recorded
#[tokio::test]
async fn an_early_run_keeps_rows_recorded() {
    // the conversions haven't finished: the recorded revision is the one before
    let scene = Box::pin(scene_with("2026-10-v03", &drain(ATTEMPT))).await;
    let id = seed_recorded(&scene).await;
    // the run and the check both refuse
    let err = run(&scene, in_pause())
        .await
        .expect_err("the run is refused");
    assert!(err.to_string().contains(V04), "{err}");
    let spec = thoradm::migrate::context::spec_for(def(), std::iter::empty(), None);
    let ctx = scene.world.ctx(RunMode::Check, 100);
    let checked = check_pass(def().runner, &spec, &RunOptions::default(), &ctx).await;
    assert!(checked.is_err(), "the check ran before the revision");
    // the row is untouched and nothing is created
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Recorded, "{row:?}");
    assert_eq!(row.entry, None);
    assert!(reactions(&scene).await.is_empty());
    assert!(resubmit_entry(&scene).await.is_none());
}

/// A transient failure leaves the row recorded, fails the run, and the next run resubmits it
#[tokio::test]
async fn a_transient_error_keeps_the_row_recorded() {
    // the backend fails before the reservation
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    arm_resubmit_fault(
        &scene.world.ns,
        &id.to_string(),
        ResubmitFault::BeforeReserve,
    );
    let err = run(&scene, in_pause()).await.expect_err("the run fails");
    assert!(matches!(err, MigrateError::Backend(_)), "{err}");
    // the row is as it was, never refused
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Recorded, "{row:?}");
    assert_eq!(row.entry, None);
    assert!(reactions(&scene).await.is_empty());
    assert!(resubmit_entry(&scene).await.is_none());
    // the next run resubmits it
    run_ok(&scene, in_pause()).await;
    let new = assert_resubmitted(&scene, id).await;
    assert_eq!(reactions(&scene).await, BTreeSet::from([new.to_string()]));
}

/// A run killed after the create and before the row's update finds the reserved reaction and
/// creates no second one
#[tokio::test]
async fn a_resubmitting_row_finds_its_reserved_id() {
    // the run stops right after the reaction is created
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    arm_resubmit_fault(&scene.world.ns, &id.to_string(), ResubmitFault::AfterCreate);
    run(&scene, in_pause()).await.expect_err("the run fails");
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Resubmitting, "{row:?}");
    let reserved = row.entry.clone().expect("a reserved id");
    assert_eq!(reactions(&scene).await, BTreeSet::from([reserved.clone()]));
    // a hand run marks it resubmitted with that id and creates nothing
    run_ok(&scene, RunFlags::default()).await;
    assert_eq!(assert_resubmitted(&scene, id).await.to_string(), reserved);
    assert_eq!(reactions(&scene).await, BTreeSet::from([reserved]));
}

/// A request whose pipeline was recreated (another creator) or changed (other stages) since
/// the cancel is refused
#[tokio::test]
async fn refuses_a_recreated_pipeline() {
    // one request recorded against another creator's pipeline, one against other stages
    let scene = Box::pin(scene()).await;
    let mut recreated = request(&scene, &scene.creator.username);
    recreated.identity.creator = "someone-else".to_owned();
    let recreated = seed_row(
        &scene,
        &recreated,
        DrainStatus::Recorded,
        Some(ATTEMPT),
        None,
    )
    .await;
    let mut changed = request(&scene, &scene.creator.username);
    changed.identity.order_sha256 = args_sha256("[[\"other-stage\"]]");
    let changed = seed_row(&scene, &changed, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // both are refused
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, recreated, "changed or recreated").await;
    assert_refused(&scene, changed, "changed or recreated").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A request bound in the pause whose pipeline's stage tools changed afterwards is refused
#[tokio::test]
async fn refuses_changed_stage_tools() {
    // a bound request, and a pipeline whose first stage now runs another tool
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let mut tools = scene.pipeline.stage_tools.clone();
    let first = scene.pipeline.order[0][0].clone();
    tools.insert(first, Uuid::new_v4());
    hset(
        &scene,
        &format!("pipeline_data:{}:{}", scene.group, scene.pipeline.name),
        "stage_tools",
        &serde_json::to_string(&tools).expect("tools serialize"),
    )
    .await;
    // refused for the changed tools
    run_ok(&scene, RunFlags::default()).await;
    assert_refused(&scene, id, "stage tools").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A request never bound is refused by a run outside the pause
#[tokio::test]
async fn refuses_an_unbound_row_outside_the_pause() {
    // an unbound in-window request
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.binding = None;
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // a hand run refuses it
    run_ok(&scene, RunFlags::default()).await;
    assert_refused(&scene, id, "bound").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A run without the in-pause flag never writes a binding
#[tokio::test]
async fn a_run_outside_the_pause_writes_no_binding() {
    // an unbound in-window request
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.binding = None;
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // the row keeps no binding
    run_ok(&scene, RunFlags::default()).await;
    let row = get(&scene, id).await;
    assert_eq!(row.request().expect("a request").binding, None, "{row:?}");
}

/// A request made by an account the operator makes (a hunt) is refused
#[tokio::test]
async fn refuses_a_hunt_reaction() {
    // the harness's admin is named like the operator's own account
    let scene = Box::pin(scene()).await;
    let request = request(&scene, ADMIN);
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // refused as a system account's request
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "system account").await;
    assert!(reactions(&scene).await.is_empty());
}

/// A creator who held no role in the group when the request was recorded is refused, even
/// when they hold one now
#[tokio::test]
async fn refuses_a_creator_who_was_not_a_member_at_the_cancel() {
    // the creator is a member now, but wasn't at the cancel
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.membership = CreatorMembership {
        held_role: false,
        role: "none".to_owned(),
    };
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // refused for the recorded membership
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "recorded").await;
    assert!(reactions(&scene).await.is_empty());
}

/// An admin with no role in the group, whose loaded `groups` list holds every group, is refused
#[tokio::test]
async fn refuses_an_admin_outside_the_group() {
    // an admin who never joined the group
    let scene = Box::pin(scene()).await;
    let admin = user(&scene, "titan", UserRole::Admin).await;
    let request = request(&scene, &admin.username);
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // refused for holding no role in the group
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "role").await;
    assert!(reactions(&scene).await.is_empty());
}

/// An analyst in the group's `analysts` set only is refused
#[tokio::test]
async fn refuses_an_analyst_outside_the_group() {
    // an analyst who holds no group role
    let scene = Box::pin(scene()).await;
    let analyst = user(&scene, "rhea", UserRole::Analyst).await;
    let request = request(&scene, &analyst.username);
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // refused for holding no role in the group
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "role").await;
    assert!(reactions(&scene).await.is_empty());
}

/// An admin who holds the `users` role in the group is resubmitted
#[tokio::test]
async fn resubmits_an_admin_with_a_user_role_in_the_group() {
    // an admin added to the group's users
    let scene = Box::pin(scene()).await;
    let admin = user(&scene, "dione", UserRole::Admin).await;
    add_user(&scene, &admin.username).await;
    let request = request(&scene, &admin.username);
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // resubmitted as the admin
    run_ok(&scene, in_pause()).await;
    let new = assert_resubmitted(&scene, id).await;
    let reaction = scene
        .admin
        .reactions
        .get(&scene.group, new)
        .await
        .expect("the reaction reads");
    assert_eq!(reaction.creator, admin.username);
}

/// A run without the flag binds nothing even while the history shows an open pause with no
/// `restore-paused` entry
#[tokio::test]
async fn a_run_without_the_flag_binds_nothing_while_the_history_shows_an_open_pause() {
    // the history holds the drain's paused counts and no restore-paused entry
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.binding = None;
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // nothing is bound and the row is refused as unbound
    run_ok(&scene, RunFlags::default()).await;
    let row = assert_refused(&scene, id, "bound").await;
    assert_eq!(row.request().expect("a request").binding, None);
    assert!(reactions(&scene).await.is_empty());
}

/// A flag naming another attempt is refused before anything is touched
#[tokio::test]
async fn the_flag_for_another_attempt_is_refused() {
    // an unbound request of the current attempt, and the flag of an earlier one
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.binding = None;
    let id = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    let flags = RunFlags {
        in_pause: Some(EARLIER.to_owned()),
        ..RunFlags::default()
    };
    // the run is refused and the row is untouched
    let err = run(&scene, flags).await.expect_err("the flag is refused");
    assert!(err.to_string().contains("--in-pause"), "{err}");
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Recorded, "{row:?}");
    assert_eq!(row.request().expect("a request").binding, None);
    assert!(reactions(&scene).await.is_empty());
}

/// A flagged run binds the in-window rows to the pipeline's stage tools, and no other row
#[tokio::test]
async fn a_flagged_run_binds_in_window_rows() {
    // two unbound in-window requests and an unbound one of an earlier attempt
    let scene = Box::pin(scene()).await;
    let mut request = request(&scene, &scene.creator.username);
    request.binding = None;
    let first = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    let second = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    let earlier = seed_row(&scene, &request, DrainStatus::Recorded, Some(EARLIER), None).await;
    // the in-window rows are bound and resubmitted, the other is stale and unbound
    run_ok(&scene, in_pause()).await;
    for id in [first, second] {
        let row = get(&scene, id).await;
        assert_eq!(
            row.request().expect("a request").binding,
            Some(binding(&scene.pipeline)),
            "{row:?}"
        );
        assert_resubmitted(&scene, id).await;
    }
    let row = get(&scene, earlier).await;
    assert_eq!(row.status, DrainStatus::Stale, "{row:?}");
    assert_eq!(row.request().expect("a request").binding, None);
}

/// A hand run after a restore, whose history ends the window, creates nothing by default
#[tokio::test]
async fn a_hand_run_after_a_restore_creates_nothing_by_default() {
    // a restore was recorded after the drain finished
    let mut history = window();
    history.push(entry(
        RESTORED_STEP,
        StepState::Done,
        at(ATTEMPT) + Duration::minutes(30),
    ));
    let scene = Box::pin(scene_with(V04, &history)).await;
    let id = seed_recorded(&scene).await;
    // the restored row is marked stale and nothing is created
    run_ok(&scene, RunFlags::default()).await;
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Stale, "{row:?}");
    assert!(row.result.is_some(), "{row:?}");
    assert!(reactions(&scene).await.is_empty());
    assert!(resubmit_entry(&scene).await.is_none());
}

/// `--include-stale` takes stale rows, still through every check
#[tokio::test]
async fn include_stale_still_applies_every_check() {
    // three stale rows: a sound one, a disabled creator's, and an unbound one
    let scene = Box::pin(scene()).await;
    let sound = request(&scene, &scene.creator.username);
    let sound = seed_row(&scene, &sound, DrainStatus::Stale, Some(EARLIER), None).await;
    let disabled = user(&scene, "phoebe", UserRole::User).await;
    add_user(&scene, &disabled.username).await;
    let disable = UserUpdate {
        password: None,
        email: None,
        role: Some(UserRole::Disabled),
        settings: None,
    };
    scene
        .admin
        .users
        .update(&disabled.username, disable)
        .await
        .expect("the user is disabled");
    let refused = request(&scene, &disabled.username);
    let refused = seed_row(&scene, &refused, DrainStatus::Stale, Some(EARLIER), None).await;
    let mut unbound = request(&scene, &scene.creator.username);
    unbound.binding = None;
    let unbound = seed_row(&scene, &unbound, DrainStatus::Stale, Some(EARLIER), None).await;
    // without the flag the stale rows stay stale
    run_ok(&scene, RunFlags::default()).await;
    for id in [sound, refused, unbound] {
        assert_eq!(get(&scene, id).await.status, DrainStatus::Stale);
    }
    // with it, only the sound row is resubmitted
    run_ok(&scene, include_stale()).await;
    let new = assert_resubmitted(&scene, sound).await;
    assert_refused(&scene, refused, "disabled").await;
    assert_refused(&scene, unbound, "bound").await;
    assert_eq!(reactions(&scene).await, BTreeSet::from([new.to_string()]));
}

/// A reservation whose reaction is gone is refused by a run without the flag, in the window or
/// out of it with `--include-stale`, and keeps its reserved id
#[tokio::test]
async fn a_missing_reservation_is_refused_without_the_flag() {
    // an in-window and an earlier attempt's reservation, neither reaction existing
    let scene = Box::pin(scene()).await;
    let request = request(&scene, &scene.creator.username);
    let (near, far) = (Uuid::new_v4(), Uuid::new_v4());
    let recent = seed_row(
        &scene,
        &request,
        DrainStatus::Resubmitting,
        Some(ATTEMPT),
        Some(near),
    )
    .await;
    let old = seed_row(
        &scene,
        &request,
        DrainStatus::Resubmitting,
        Some(EARLIER),
        Some(far),
    )
    .await;
    // a hand run refuses the in-window one, and one with --include-stale the other
    run_ok(&scene, RunFlags::default()).await;
    let row = assert_refused(&scene, recent, "reserved reaction not found").await;
    assert_eq!(row.entry, Some(near.to_string()));
    run_ok(&scene, include_stale()).await;
    let row = assert_refused(&scene, old, "reserved reaction not found").await;
    assert_eq!(row.entry, Some(far.to_string()));
    assert!(reactions(&scene).await.is_empty());
}

/// The operator's run recreates a missing in-window reservation under its reserved id
#[tokio::test]
async fn a_flagged_run_recreates_a_missing_reservation_with_its_id() {
    // an in-window reservation whose reaction was never created
    let scene = Box::pin(scene()).await;
    let request = request(&scene, &scene.creator.username);
    let reserved = Uuid::new_v4();
    let id = seed_row(
        &scene,
        &request,
        DrainStatus::Resubmitting,
        Some(ATTEMPT),
        Some(reserved),
    )
    .await;
    // created under that id
    run_ok(&scene, in_pause()).await;
    assert_eq!(assert_resubmitted(&scene, id).await, reserved);
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([reserved.to_string()])
    );
}

/// A reservation of an earlier attempt is never marked stale: it keeps its reserved id whether
/// its reaction exists or not
#[tokio::test]
async fn an_out_of_window_resubmitting_row_is_never_marked_stale() {
    // an earlier attempt's reservation whose reaction exists, and one whose reaction doesn't
    let scene = Box::pin(scene()).await;
    let request = request(&scene, &scene.creator.username);
    let held = seed_recorded(&scene).await;
    arm_resubmit_fault(
        &scene.world.ns,
        &held.to_string(),
        ResubmitFault::AfterCreate,
    );
    run(&scene, in_pause()).await.expect_err("the run fails");
    let existing: Uuid = get(&scene, held)
        .await
        .entry
        .expect("a reserved id")
        .parse()
        .expect("a uuid");
    cql(
        &scene.world,
        &format!(
            "UPDATE {}.schema_migrations SET attempt = '{EARLIER}' WHERE kind = 'drain_request' \
             AND id = '{held}'",
            scene.world.ns
        ),
    )
    .await;
    let missing = Uuid::new_v4();
    let gone = seed_row(
        &scene,
        &request,
        DrainStatus::Resubmitting,
        Some(EARLIER),
        Some(missing),
    )
    .await;
    // a hand run: neither is stale, and both keep their ids
    run_ok(&scene, RunFlags::default()).await;
    let row = get(&scene, held).await;
    assert_eq!(row.status, DrainStatus::Resubmitted, "{row:?}");
    assert_eq!(row.entry, Some(existing.to_string()));
    let row = get(&scene, gone).await;
    assert_ne!(row.status, DrainStatus::Stale, "{row:?}");
    assert_eq!(row.entry, Some(missing.to_string()));
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([existing.to_string()])
    );
}

/// A reserved id stays in `entry` through every status the row takes afterwards
#[tokio::test]
async fn a_reserved_id_survives_every_status_change() {
    // a run reserves an id and stops before the create
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    arm_resubmit_fault(
        &scene.world.ns,
        &id.to_string(),
        ResubmitFault::AfterReserve,
    );
    run(&scene, in_pause()).await.expect_err("the run fails");
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Resubmitting, "{row:?}");
    let reserved = row.entry.clone().expect("a reserved id");
    assert!(reactions(&scene).await.is_empty());
    // the operator's rerun creates it under that id
    run_ok(&scene, in_pause()).await;
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Resubmitted, "{row:?}");
    assert_eq!(row.entry.as_deref(), Some(reserved.as_str()));
    assert_eq!(row.result.as_deref(), Some(reserved.as_str()));
    // and a reservation that is refused keeps its id too
    let request = request(&scene, &scene.creator.username);
    let other = Uuid::new_v4();
    let refused = seed_row(
        &scene,
        &request,
        DrainStatus::Resubmitting,
        Some(ATTEMPT),
        Some(other),
    )
    .await;
    run_ok(&scene, RunFlags::default()).await;
    let row = assert_refused(&scene, refused, "reserved reaction not found").await;
    assert_eq!(row.entry, Some(other.to_string()));
}

/// Rows of an earlier attempt and rows with no attempt are marked stale and not resubmitted
#[tokio::test]
async fn rows_from_an_earlier_attempt_are_stale() {
    // an earlier attempt that stopped, then the current one
    let mut history = drain(EARLIER);
    history.push(entry(
        thorium::models::upgrades::UPGRADE_STOPPED_STEP,
        StepState::Done,
        at(EARLIER) + Duration::minutes(30),
    ));
    history.extend(window());
    let scene = Box::pin(scene_with(V04, &history)).await;
    let request = request(&scene, &scene.creator.username);
    let earlier = seed_row(&scene, &request, DrainStatus::Recorded, Some(EARLIER), None).await;
    let unattempted = seed_row(&scene, &request, DrainStatus::Recorded, None, None).await;
    let current = seed_recorded(&scene).await;
    // only the current attempt's row is resubmitted
    run_ok(&scene, in_pause()).await;
    for id in [earlier, unattempted] {
        let row = get(&scene, id).await;
        assert_eq!(row.status, DrainStatus::Stale, "{row:?}");
        assert!(row.result.is_some(), "{row:?}");
        assert_eq!(row.entry, None);
    }
    let new = assert_resubmitted(&scene, current).await;
    assert_eq!(reactions(&scene).await, BTreeSet::from([new.to_string()]));
}

/// A drain whose first run failed and whose retry finished is one attempt under the first run's
/// start: the operator's run resubmits every request either run recorded exactly once, a flag
/// naming the retry's start is refused, and a later `--include-stale` run takes only the stale
/// request of an earlier attempt
#[tokio::test]
async fn a_retried_drain_resubmits_both_runs_requests_once() {
    // the first run failed, the retry after a new approval finished, and so did the conversions
    let start = at(ATTEMPT);
    let mut first = entry("drain-reactions", StepState::Running, start);
    first.paused.insert("api".to_owned(), 1);
    let mut retry = entry(
        "drain-reactions",
        StepState::Running,
        start + Duration::minutes(15),
    );
    retry.paused.insert("api".to_owned(), 1);
    let history = vec![
        first,
        entry(
            "drain-reactions",
            StepState::Failed,
            start + Duration::minutes(5),
        ),
        retry.clone(),
        entry(
            "drain-reactions",
            StepState::Done,
            start + Duration::minutes(25),
        ),
        entry(
            "finalize-tool-only",
            StepState::Done,
            start + Duration::minutes(30),
        ),
    ];
    let scene = Box::pin(scene_with(V04, &history)).await;
    // a request each run recorded under the first run's start, and a stale one of an earlier
    // attempt
    let request = request(&scene, &scene.creator.username);
    let by_first = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    let by_retry = seed_row(&scene, &request, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    let earlier = seed_row(&scene, &request, DrainStatus::Stale, Some(EARLIER), None).await;
    // the retry's own start doesn't name the attempt
    let flags = RunFlags {
        in_pause: retry.at.clone(),
        ..RunFlags::default()
    };
    let err = run(&scene, flags).await.expect_err("the flag is refused");
    assert!(err.to_string().contains("--in-pause"), "{err}");
    assert!(reactions(&scene).await.is_empty());
    // the operator's run resubmits both runs' requests and leaves the stale one
    run_ok(&scene, in_pause()).await;
    let first_new = assert_resubmitted(&scene, by_first).await;
    let retry_new = assert_resubmitted(&scene, by_retry).await;
    assert_eq!(get(&scene, earlier).await.status, DrainStatus::Stale);
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([first_new.to_string(), retry_new.to_string()])
    );
    // a hand run with --include-stale takes the stale request only, once
    run_ok(&scene, include_stale()).await;
    run_ok(&scene, include_stale()).await;
    let earlier_new = assert_resubmitted(&scene, earlier).await;
    assert_eq!(assert_resubmitted(&scene, by_first).await, first_new);
    assert_eq!(assert_resubmitted(&scene, by_retry).await, retry_new);
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([
            first_new.to_string(),
            retry_new.to_string(),
            earlier_new.to_string()
        ])
    );
}

/// Two overlapping runs that both read a row before either reserved it, as a keyspace on tablets
/// allows since its guard is a read then a plain write, reserve the same id and make exactly one
/// reaction
#[tokio::test]
async fn overlapping_runs_make_exactly_one_reaction() {
    // the first run reserves the request's id and creates its reaction, then stops
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    arm_resubmit_fault(&scene.world.ns, &id.to_string(), ResubmitFault::AfterCreate);
    run(&scene, in_pause()).await.expect_err("the run fails");
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Resubmitting, "{row:?}");
    let reserved = reserved_id(&id.to_string(), Some(ATTEMPT));
    assert_eq!(row.entry, Some(reserved.to_string()));
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([reserved.to_string()])
    );
    // the second run read the row before that reservation, and its plain write lands over it
    cql(
        &scene.world,
        &format!(
            "UPDATE {}.schema_migrations SET status = 'recorded', entry = null WHERE kind = \
             'drain_request' AND id = '{id}'",
            scene.world.ns
        ),
    )
    .await;
    // it reserves the same id, finds the reaction there, and creates no second one
    run_ok(&scene, in_pause()).await;
    assert_eq!(assert_resubmitted(&scene, id).await, reserved);
    assert_eq!(
        reactions(&scene).await,
        BTreeSet::from([reserved.to_string()])
    );
}

/// A reservation written on a status that changed since it was read is not applied
#[tokio::test]
async fn a_reservation_is_not_applied_over_a_changed_status() {
    // a recorded row that another writer refuses
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let mut refused = get(&scene, id).await;
    refused.status = DrainStatus::Refused;
    refused.result = Some("refused by another writer".to_owned());
    assert!(
        scene
            .store
            .update_status_if(&refused, DrainStatus::Recorded)
            .await
            .expect("the update runs")
    );
    // a reservation that expects it recorded isn't applied
    let mut reserve = get(&scene, id).await;
    reserve.status = DrainStatus::Resubmitting;
    reserve.entry = Some(Uuid::new_v4().to_string());
    let applied = scene
        .store
        .update_status_if(&reserve, DrainStatus::Recorded)
        .await
        .expect("the update runs");
    assert!(!applied);
    let row = get(&scene, id).await;
    assert_eq!(row.status, DrainStatus::Refused, "{row:?}");
    assert_eq!(row.entry, None);
    assert_eq!(row.result.as_deref(), Some("refused by another writer"));
    // and the resubmission leaves the refused row alone
    run_ok(&scene, in_pause()).await;
    assert_eq!(get(&scene, id).await, row);
    assert!(reactions(&scene).await.is_empty());
}

/// A resubmitted request's args row is deleted; a refused request's args are kept
#[tokio::test]
async fn args_are_deleted_on_resubmission() {
    // one sound request and one refused for the creator's recorded membership
    let scene = Box::pin(scene()).await;
    let sound = seed_recorded(&scene).await;
    let mut lapsed = request(&scene, &scene.creator.username);
    lapsed.membership.held_role = false;
    let lapsed = seed_row(&scene, &lapsed, DrainStatus::Recorded, Some(ATTEMPT), None).await;
    // the resubmitted row's args are gone, the refused row's kept
    run_ok(&scene, in_pause()).await;
    assert_resubmitted(&scene, sound).await;
    assert_refused(&scene, lapsed, "recorded").await;
    let gone = scene
        .store
        .get_args(&sound.to_string())
        .await
        .expect("the args read");
    assert_eq!(gone, None);
    let kept = scene
        .store
        .get_args(&lapsed.to_string())
        .await
        .expect("the args read");
    assert_eq!(kept, Some(args_json(&scene.pipeline)));
}

/// A request whose args row is gone while its hash names args is refused
#[tokio::test]
async fn expired_args_are_refused() {
    // the args row expired
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    scene
        .store
        .delete_args(&id.to_string())
        .await
        .expect("the args delete");
    // refused for the missing args
    run_ok(&scene, in_pause()).await;
    assert_refused(&scene, id, "arguments expired or not restored").await;
    assert!(reactions(&scene).await.is_empty());
}

/// `migrate report` names the requests in its tables and its JSON without their args
#[tokio::test]
async fn the_report_prints_no_args() {
    // one resubmitted request and one refused
    let scene = Box::pin(scene()).await;
    let sound = seed_recorded(&scene).await;
    let expired = seed_recorded(&scene).await;
    scene
        .store
        .delete_args(&expired.to_string())
        .await
        .expect("the args delete");
    run_ok(&scene, in_pause()).await;
    // the tables and the JSON
    let harness = Harness::get_without_tablets().await;
    for json in [false, true] {
        let mut command = harness.thoradm();
        command.args(["migrate", "report", "--id", ID]);
        if json {
            command.arg("--json");
        }
        let output = World::run(&mut command);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains(&sound.to_string()), "{stdout}");
        assert!(stdout.contains(&expired.to_string()), "{stdout}");
        assert!(!stdout.contains(ARG_SECRET), "{stdout}");
        assert!(!stderr.contains(ARG_SECRET), "{stderr}");
    }
}

/// A buffer the captured log lines are written to
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    /// Append bytes to the buffer
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes to append
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // a poisoned lock still holds the earlier lines
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    /// Nothing is buffered beyond the vector
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// No log line of a run holds a request's args, while the resubmission is logged with the old
/// and new ids and the creator
#[tokio::test]
async fn no_log_line_holds_args() {
    // every event of this test's thread, at every level, goes to a buffer
    let scene = Box::pin(scene()).await;
    let id = seed_recorded(&scene).await;
    let expired = seed_recorded(&scene).await;
    scene
        .store
        .delete_args(&expired.to_string())
        .await
        .expect("the args delete");
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    run_ok(&scene, in_pause()).await;
    drop(guard);
    // the resubmission is logged, the args never are
    let new = assert_resubmitted(&scene, id).await;
    let lines = String::from_utf8_lossy(
        &captured
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();
    assert!(!lines.contains(ARG_SECRET), "{lines}");
    let logged = lines.lines().any(|line| {
        line.contains(&id.to_string())
            && line.contains(&new.to_string())
            && line.contains(&scene.creator.username)
    });
    assert!(logged, "{lines}");
}
