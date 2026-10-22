//! Tests the Images routes in Thorium

use data_encoding::BASE64;
use std::collections::{HashMap, HashSet};
use thorium::client::{ClientSettings, Users};
use thorium::models::{
    EntityMetadata, GenericJob, GenericJobArgs, GenericJobArgsUpdate, GenericJobOpts, GroupAllowed,
    GroupUpdate, GroupUsersUpdate, ImageBan, ImageBanKind, ImageBanUpdate, ImageScaler,
    ImageUpdate, Pipeline, PipelineBan, PipelineBanKind, PipelineBanUpdate, PipelineRequest,
    PipelineUpdate, PlannedStage, ReactionCursorOpts, ReactionListParams, ReactionPlan,
    ReactionRequest, ReactionSource, ReactionStatus, ReactionUpdate, Resources, RunProvenance,
    ScopedTokenRequest, StageLogLine, StageLogsAdd, UserCreate, UserRole, UserUpdate,
};
use thorium::test_utilities::{self, CONF, generators};
use thorium::{
    Error, Thorium, contains, contains_key, fail, is, is_empty, is_in, is_not_in, vec_in_vec,
};
use uuid::Uuid;

/// Get a connection to the test API's Redis database
async fn redis_conn() -> redis::aio::MultiplexedConnection {
    // reuse the harness's pool so the connection selects the configured database
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let pooled = pool.get().await.expect("a Redis connection");
    pooled.clone()
}

/// Build the key to a reaction's data hash
///
/// # Arguments
///
/// * `group` - The group of the reaction
/// * `id` - The id of the reaction
fn reaction_key(group: &str, id: Uuid) -> String {
    // reactions live in one hash per reaction
    format!(
        "{}:reaction_data:{group}:{id}",
        test_utilities::CONF.thorium.namespace
    )
}

/// Read every field of a Redis hash
///
/// # Arguments
///
/// * `key` - The key of the hash
async fn hash(key: &str) -> HashMap<String, String> {
    // read the whole hash
    redis::cmd("hgetall")
        .arg(key)
        .query_async(&mut redis_conn().await)
        .await
        .expect("a hash")
}

/// Create a pipeline whose stages each run a new Image tool in an environment
///
/// # Arguments
///
/// * `group` - The group of the pipeline
/// * `steps` - The number of stages, each in its own step
/// * `scaler` - The scaler whose environment the stages' tools run in
/// * `client` - The client to create the pipeline with
async fn steps_pipe(
    group: &str,
    steps: usize,
    scaler: ImageScaler,
    client: &Thorium,
) -> Result<Pipeline, Error> {
    // create a tool for each stage
    let mut stages = Vec::with_capacity(steps);
    for _ in 0..steps {
        stages.push(
            generators::stage_tool(group, &generators::gen_image_spec_for(scaler), client).await?,
        );
    }
    // put each stage in its own step
    let order = stages
        .iter()
        .map(|(name, _)| vec![name.clone()])
        .collect::<Vec<Vec<String>>>();
    let name = generators::gen_string(20).to_lowercase();
    let req = stages.into_iter().fold(
        PipelineRequest::new(group, &name, serde_json::json!(order)),
        |req, (stage, instance)| req.stage_tool(stage, instance),
    );
    client.pipelines.create(&req).await?;
    client.pipelines.get(group, &name).await
}

/// A reaction stores its source and plan, and a finished reaction stored without them reads with
/// a pipeline source and an empty plan
#[tokio::test]
async fn reaction_source_and_plan_in_redis() -> Result<(), Error> {
    // a two step pipeline and a reaction on it
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let pipe = steps_pipe(&group, 2, ImageScaler::K8s, &admin).await?;
    let created = admin
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    // the reaction reads back its pipeline source and plan with no run fields
    let reaction = admin.reactions.get(&group, created.id).await?;
    is!(
        matches!(
            &reaction.source,
            ReactionSource::Pipeline { name, .. } if *name == pipe.name
        ),
        true
    );
    let expected = pipe
        .order
        .iter()
        .map(|step| {
            step.iter()
                .map(|stage| PlannedStage {
                    name: stage.clone(),
                    tool_instance: generators::stage_instance(&pipe, stage),
                })
                .collect::<Vec<PlannedStage>>()
        })
        .collect::<Vec<Vec<PlannedStage>>>();
    let expected = ReactionPlan { stages: expected };
    is!(&reaction.plan, &expected);
    is!(reaction.run_token, None::<String>);
    is!(reaction.run, None::<Uuid>);
    // the hash holds the source and plan and no run fields
    let key = reaction_key(&group, created.id);
    let raw = hash(&key).await;
    is!(raw.contains_key("source"), true);
    is!(raw.contains_key("plan"), true);
    is!(raw.contains_key("run_token"), false);
    is!(raw.contains_key("run"), false);
    // a finished hash without the new fields reads with defaults and is left unchanged
    let _: () = redis::cmd("hdel")
        .arg(&key)
        .arg("source")
        .arg("plan")
        .query_async(&mut redis_conn().await)
        .await
        .expect("drop the new fields");
    let before = hash(&key).await;
    let read = admin.reactions.get(&group, created.id).await?;
    is!(
        &read.source,
        &ReactionSource::Pipeline {
            name: pipe.name.clone(),
            provenance: RunProvenance::default(),
        }
    );
    is!(&read.plan, &ReactionPlan::default());
    is!(read.run_token, None::<String>);
    is!(hash(&key).await, before);
    Ok(())
}

/// A reaction proceeds through its plan without reading its pipeline, and a reaction stored
/// without a plan still reads, lists, and deletes
#[tokio::test]
async fn reaction_runs_plan_without_pipeline() -> Result<(), Error> {
    // a two step pipeline and a reaction on it
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let pipe = steps_pipe(&group, 2, ImageScaler::K8s, &admin).await?;
    let created = admin
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    let first = &pipe.order[0][0];
    let second = &pipe.order[1][0];
    // a worker for each planned stage's tool instance
    let worker = generators::gen_string(24);
    generators::worker(
        "cluster0", "node0", &worker, &group, &pipe.name, first, &admin,
    )
    .await?;
    let second_worker = generators::gen_string(24);
    generators::worker(
        "cluster0",
        "node0",
        &second_worker,
        &group,
        &pipe.name,
        second,
        &admin,
    )
    .await?;
    // the pipeline's stages are replaced after the reaction is created
    let replacement = generators::tool_instance(&[&group], &admin).await?;
    let update = PipelineUpdate::default()
        .order(vec![vec!["swapped".to_owned()]])
        .add_stage_tool("swapped", replacement)
        .remove_stage_tool(first.as_str())
        .remove_stage_tool(second.as_str());
    admin.pipelines.update(&group, &pipe.name, &update).await?;
    // the first stage's job runs its planned tool instance
    let jobs = admin
        .jobs
        .claim_workload(
            &group,
            generators::stage_instance(&pipe, first),
            "cluster0",
            "node0",
            &worker,
            1,
        )
        .await?;
    is!(jobs.len(), 1);
    is!(&jobs[0].stage, first);
    admin
        .jobs
        .proceed(&jobs[0], &generators::stage_logs(), 1, &worker)
        .await?;
    // the second step's job runs the second planned stage, not the pipeline's new stage
    let reaction = admin.reactions.get(&group, created.id).await?;
    is!(reaction.current_stage, 1);
    let second_instance = generators::stage_instance(&pipe, second);
    let jobs = admin
        .jobs
        .claim_workload(
            &group,
            second_instance,
            "cluster0",
            "node0",
            &second_worker,
            1,
        )
        .await?;
    is!(jobs.len(), 1);
    is!(&jobs[0].stage, second);
    is!(jobs[0].tool_instance, second_instance);
    admin
        .jobs
        .proceed(&jobs[0], &generators::stage_logs(), 1, &second_worker)
        .await?;
    let reaction = admin.reactions.get(&group, created.id).await?;
    is!(reaction.status, ReactionStatus::Completed);
    // a finished reaction stored without a plan reads, lists, and deletes
    let key = reaction_key(&group, created.id);
    let _: () = redis::cmd("hdel")
        .arg(&key)
        .arg("source")
        .arg("plan")
        .query_async(&mut redis_conn().await)
        .await
        .expect("drop the plan");
    let read = admin.reactions.get(&group, created.id).await?;
    is!(read.plan.stages.is_empty(), true);
    let listed = admin
        .reactions
        .list(&group, &pipe.name)
        .details()
        .exec()
        .await?;
    is!(
        listed.details.iter().any(|item| item.id == created.id),
        true
    );
    admin.reactions.delete(&group, &created.id).await?;
    fail!(admin.reactions.get(&group, created.id).await, 404);
    generators::delete_worker(&worker, &admin).await?;
    generators::delete_worker(&second_worker, &admin).await?;
    Ok(())
}

/// A stage that follows a tool's latest instance is pinned in each reaction's plan
#[tokio::test]
async fn latest_tool_stage_pins_instance() -> Result<(), Error> {
    // a pipeline whose one stage follows a tool's latest instance
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (tool, first) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&group, &name, serde_json::json!([["scan"]]))
        .stage_tool("scan", tool.id);
    admin.pipelines.create(&req).await?;
    // a reaction pins the current instance
    let early = admin
        .reactions
        .create(&ReactionRequest::new(&group, &name))
        .await?;
    // publishing a new version doesn't change that reaction or its job
    admin
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_image_spec()),
        )
        .await?;
    let read = admin.reactions.get(&group, early.id).await?;
    is!(read.plan.tool_instance("scan"), Some(first.id));
    let job = admin.jobs.get(read.jobs[0]).await?;
    is!(job.tool_instance, first.id);
    // a later reaction pins the new instance
    let latest = admin.entities.get(tool.id).await?;
    let EntityMetadata::Tool(meta) = &latest.metadata else {
        return Err(Error::new("not a tool".to_owned()));
    };
    let late = admin
        .reactions
        .create(&ReactionRequest::new(&group, &name))
        .await?;
    let read = admin.reactions.get(&group, late.id).await?;
    is!(read.plan.tool_instance("scan"), meta.latest);
    assert_ne!(meta.latest, Some(first.id));
    Ok(())
}

/// Pipeline reactions keep the group and developer checks for argument overrides
#[tokio::test]
async fn pipeline_reaction_checks() -> Result<(), Error> {
    // a non-developer editor, a K8s developer, and a monitor of one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let editor = generators::moon_user(&admin, "europa", &suffix).await?;
    let developer = generators::named_user_with_role(
        &admin,
        &generators::readable_name("amalthea", &suffix),
        UserRole::Developer {
            k8s: true,
            bare_metal: false,
            windows: false,
            kvm: false,
            external: false,
        },
    )
    .await?;
    let monitor = generators::moon_user(&admin, "thebe", &suffix).await?;
    let roles = generators::GroupRoles::default()
        .user(&editor.username)
        .user(&developer.username)
        .monitor(&monitor.username);
    let group = generators::god_group(&admin, "saturn", &suffix, &roles).await?;
    // a pipeline whose stage names aren't images and whose stages run K8s tools
    let pipe = steps_pipe(&group, 1, ImageScaler::K8s, &admin).await?;
    let stage = pipe.order[0][0].clone();
    let plain = ReactionRequest::new(&group, &pipe.name);
    let overridden = ReactionRequest::new(&group, &pipe.name).args(
        &stage,
        GenericJobArgs::default().opts(GenericJobOpts::new(false, true, None)),
    );
    // the editor creates a plain reaction but can't override args
    editor.client.reactions.create(&plain).await?;
    fail!(editor.client.reactions.create(&overridden).await, 401);
    // the K8s developer can override args on the stage's K8s tool
    developer.client.reactions.create(&overridden).await?;
    // a monitor can't create reactions
    fail!(monitor.client.reactions.create(&plain).await, 401);
    // a group that disallows reactions refuses them
    let closed = generators::named_group_with_roles(
        &generators::readable_name("janus", &suffix),
        &generators::GroupRoles::default().user(&editor.username),
        GroupAllowed::default().disable_reactions(),
        &admin,
    )
    .await?;
    let closed_pipe = steps_pipe(&closed, 1, ImageScaler::K8s, &admin).await?;
    let resp = editor
        .client
        .reactions
        .create(&ReactionRequest::new(&closed, &closed_pipe.name))
        .await;
    fail!(resp, 401);
    Ok(())
}

/// A bulk create checks each pipeline's tool instances in that pipeline's group, even when an
/// earlier pipeline in another group already read the same tool instance
#[tokio::test]
async fn bulk_create_checks_tool_instances_per_group() -> Result<(), Error> {
    // two groups with a one stage pipeline each, on a tool instance of its own group
    let admin = test_utilities::admin_client().await?;
    let mut groups = generators::groups(2, &admin).await?;
    let (first, second) = (groups.remove(0).name, groups.remove(0).name);
    let mut pipes = Vec::with_capacity(2);
    for group in [&first, &second] {
        let (_, instance) =
            generators::stage_tool(group, &generators::gen_image_spec(), &admin).await?;
        let name = generators::gen_string(20).to_lowercase();
        let req = PipelineRequest::new(group, &name, serde_json::json!([["stage"]]))
            .stage_tool("stage", instance);
        admin.pipelines.create(&req).await?;
        pipes.push((name, instance));
    }
    let (first_pipe, first_instance) = pipes.remove(0);
    let (second_pipe, _) = pipes.remove(0);
    // the second pipeline's stage is pointed at the first group's tool instance in Redis,
    // a state the pipeline routes refuse to write
    let key = format!(
        "{}:pipeline_data:{second}:{second_pipe}",
        test_utilities::CONF.thorium.namespace
    );
    let stage_tools = serde_json::json!({ "stage": first_instance }).to_string();
    let _: () = redis::cmd("hset")
        .arg(&key)
        .arg("stage_tools")
        .arg(&stage_tools)
        .query_async(&mut redis_conn().await)
        .await
        .expect("point the stage at the other group's tool instance");
    // a single create of the second pipeline can't see that tool instance
    let resp = admin
        .reactions
        .create(&ReactionRequest::new(&second, &second_pipe))
        .await;
    fail!(resp, 404);
    // a bulk create that reads the tool instance for the first group first is refused too
    for reqs in [
        vec![
            ReactionRequest::new(&first, &first_pipe),
            ReactionRequest::new(&second, &second_pipe),
        ],
        vec![
            ReactionRequest::new(&second, &second_pipe),
            ReactionRequest::new(&first, &first_pipe),
        ],
    ] {
        let resp = admin.reactions.create_bulk(&reqs).await;
        fail!(resp, 404);
    }
    // no reaction of the second pipeline was created
    let listed = admin
        .reactions
        .list(&second, &second_pipe)
        .details()
        .exec()
        .await?;
    is!(listed.details.is_empty(), true);
    Ok(())
}

/// A bulk create reads each stage's tool instance once however many reactions it creates
#[tokio::test]
async fn bulk_create_reads_each_tool_instance_once() -> Result<(), Error> {
    // a three stage pipeline
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let pipe = steps_pipe(&group, 3, ImageScaler::K8s, &admin).await?;
    let instances = pipe
        .order
        .iter()
        .flatten()
        .map(|stage| generators::stage_instance(&pipe, stage))
        .collect::<Vec<Uuid>>();
    // count the tool instance reads of a bulk create of 10 and of 100 reactions
    let mut deltas = Vec::new();
    for count in [10, 100] {
        let before = instances
            .iter()
            .map(|id| test_utilities::faults::entity_reads(*id))
            .collect::<Vec<u64>>();
        let reqs = (0..count)
            .map(|_| ReactionRequest::new(&group, &pipe.name))
            .collect::<Vec<ReactionRequest>>();
        let resp = admin.reactions.create_bulk(&reqs).await?;
        is!(resp.created.len(), count);
        let delta = instances
            .iter()
            .zip(before)
            .map(|(id, before)| test_utilities::faults::entity_reads(*id) - before)
            .collect::<Vec<u64>>();
        // each stage's tool instance is read at most once per request
        assert!(delta.iter().all(|reads| *reads <= 1));
        deltas.push(delta);
        // every created reaction plans the same three tool instances
        for id in resp.created {
            let reaction = admin.reactions.get(&group, id).await?;
            is!(reaction.plan.tool_instances().len(), 3);
            for instance in &instances {
                is!(reaction.plan.tool_instances().contains(instance), true);
            }
        }
    }
    // both sizes read the same tool instances the same number of times
    is!(&deltas[0], &deltas[1]);
    Ok(())
}

/// Only admins with their full token read a run's token, and reactions without a token scoped to
/// their run are not found
#[tokio::test]
async fn run_token_route() -> Result<(), Error> {
    // a user's pipeline reaction
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let user = generators::moon_user(&admin, "leda", &suffix).await?;
    let roles = generators::GroupRoles::default().owner(&user.username);
    let group = generators::god_group(&admin, "uranus", &suffix, &roles).await?;
    let pipe = steps_pipe(&group, 1, ImageScaler::K8s, &admin).await?;
    let created = user
        .client
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    // a pipeline reaction has no run token
    fail!(admin.reactions.run_token(&group, created.id).await, 404);
    // a plain token the user owns is named on the reaction
    let token_name = format!("run-{}", created.id);
    user.client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(&token_name).group(&group))
        .await?;
    let _: () = redis::cmd("hset")
        .arg(reaction_key(&group, created.id))
        .arg("run_token")
        .arg(&token_name)
        .query_async(&mut redis_conn().await)
        .await
        .expect("name the run token");
    // a token without the reaction's run scope isn't served
    fail!(admin.reactions.run_token(&group, created.id).await, 404);
    // the owner and an admin's scoped token are refused
    fail!(
        user.client.reactions.run_token(&group, created.id).await,
        401
    );
    let scoped_admin = generators::scoped_client(&admin).await?;
    fail!(
        scoped_admin.reactions.run_token(&group, created.id).await,
        401
    );
    // once the token is deleted the reaction has no run token
    user.client.users.delete_scoped_token(&token_name).await?;
    fail!(admin.reactions.run_token(&group, created.id).await, 404);
    Ok(())
}

/// A job takes its scaler and generator flag from its tool instance and carries it to the claim
#[tokio::test]
async fn job_fields_from_tool_instance() -> Result<(), Error> {
    // a group and a node to register workers on
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let start = chrono::Utc::now() - chrono::Duration::days(1);
    let end = start + chrono::Duration::days(365 * 100);
    // a reaction on a K8s, a BareMetal, and an External tool each gets that scaler
    for scaler in [
        ImageScaler::K8s,
        ImageScaler::BareMetal,
        ImageScaler::External,
    ] {
        let pipe = steps_pipe(&group, 1, scaler, &admin).await?;
        let created = admin
            .reactions
            .create(&ReactionRequest::new(&group, &pipe.name))
            .await?;
        let reaction = admin.reactions.get(&group, created.id).await?;
        let job = admin.jobs.get(reaction.jobs[0]).await?;
        is!(job.scaler, scaler);
        is!(job.generator, false);
        // its deadline is in that scaler's deadline stream
        let deadlines = admin.jobs.deadlines(scaler, &start, &end, 100_000).await?;
        is!(
            deadlines.iter().any(|deadline| deadline.job_id == job.id),
            true
        );
    }
    // a generator tool's job is a generator
    let generator = generators::gen_generator_pipe(&group, &admin).await?;
    let created = admin
        .reactions
        .create(&ReactionRequest::new(&group, &generator.name))
        .await?;
    let reaction = admin.reactions.get(&group, created.id).await?;
    let job = admin.jobs.get(reaction.jobs[0]).await?;
    is!(job.generator, true);
    // the claimed job carries its tool instance and no run
    let worker = generators::gen_string(24);
    generators::worker(
        "cluster0",
        "node0",
        &worker,
        &group,
        &generator.name,
        &job.stage,
        &admin,
    )
    .await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, job.tool_instance, "cluster0", "node0", &worker, 1)
        .await?;
    is!(claimed.len(), 1);
    is!(claimed[0].tool_instance, job.tool_instance);
    is!(claimed[0].run, None::<Uuid>);
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

#[tokio::test]
async fn create() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let resp = client.reactions.create(&react_req).await?;
    // get the created reaction
    let created = client.reactions.get(&group, resp.id).await?;
    // make sure our reaction request matches what was created
    is!(created, react_req);
    Ok(())
}

#[tokio::test]
async fn create_bulk() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create a random reactions in bulk based on our pipeline request
    let (_, resp) = generators::reactions(&group, 20, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    Ok(())
}

/// Build a user update that only sets a role
///
/// # Arguments
///
/// * `role` - The role to set in this update
fn role_update(role: UserRole) -> UserUpdate {
    UserUpdate {
        password: None,
        email: None,
        role: Some(role),
        settings: None,
    }
}

#[tokio::test]
async fn create_bulk_by_user_disabled() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a user that will be disabled and one that stays enabled
    let disabled_client = generators::client(&admin).await?;
    let enabled_client = generators::client(&admin).await?;
    // get both users info
    let disabled_info = disabled_client.users.info().await?;
    let enabled_info = enabled_client.users.info().await?;
    // create a group and pipeline owned by each user
    let disabled_group = generators::groups(1, &disabled_client)
        .await?
        .remove(0)
        .name;
    let enabled_group = generators::groups(1, &enabled_client).await?.remove(0).name;
    let disabled_pipe_req = generators::pipelines(&disabled_group, 1, false, &disabled_client)
        .await?
        .remove(0);
    let enabled_pipe_req = generators::pipelines(&enabled_group, 1, false, &enabled_client)
        .await?
        .remove(0);
    // get the created pipelines
    let disabled_pipe = disabled_client
        .pipelines
        .get(&disabled_group, &disabled_pipe_req.name)
        .await?;
    let enabled_pipe = enabled_client
        .pipelines
        .get(&enabled_group, &enabled_pipe_req.name)
        .await?;
    // build a reaction request for each user
    let disabled_req = generators::gen_reaction(&disabled_group, &disabled_pipe, None);
    let enabled_req = generators::gen_reaction(&enabled_group, &enabled_pipe, None);
    // disable one of our users as an admin
    admin
        .users
        .update(&disabled_info.username, role_update(UserRole::Disabled))
        .await?;
    // bulk create reactions for both users as the admin
    let mut reqs = HashMap::default();
    reqs.insert(disabled_info.username.clone(), vec![disabled_req]);
    reqs.insert(enabled_info.username.clone(), vec![enabled_req]);
    let creates = admin.reactions.create_bulk_by_user(&reqs).await?;
    // make sure no reactions were created for our disabled user
    let disabled_resp = &creates[&disabled_info.username];
    is_empty!(disabled_resp.created);
    // make sure our disabled users request got an indexed error entry
    is!(disabled_resp.errors.len(), 1);
    contains_key!(disabled_resp.errors, &0);
    // make sure the error says this user was disabled
    is!(disabled_resp.errors[&0].contains("has been disabled"), true);
    // make sure our enabled users reaction was still created without errors
    let enabled_resp = &creates[&enabled_info.username];
    is_empty!(enabled_resp.errors);
    is!(enabled_resp.created.len(), 1);
    Ok(())
}

/// A bulk create by user reports one user's refused bulk request as per-request errors for that
/// user and still creates the other users' reactions exactly once
#[tokio::test]
async fn create_bulk_by_user_one_refused() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a group for both users to create reactions in
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add a member whose requests are allowed and one whose requests will be refused
    let allowed = group_member(&admin, &group, UserRole::User).await?;
    let refused = group_member(&admin, &group, UserRole::User).await?;
    // get both users names
    let allowed_name = allowed.users.info().await?.username;
    let refused_name = refused.users.info().await?.username;
    // create a pipeline in this group
    let pipe_req = generators::pipelines(&group, 1, false, &admin)
        .await?
        .remove(0);
    // get the created pipeline
    let pipe = admin.pipelines.get(&group, &pipe_req.name).await?;
    // upload a sample as the refused user
    let sub = refused.files.create(generators::gen_sample(&group)).await?;
    // delete that sample again so the refused user can no longer read it
    let opts = thorium::models::FileDeleteOpts::default().groups(vec![group.clone()]);
    let deleted = refused.files.delete(&sub.sha256, &sub.id, &opts).await?;
    is!(deleted.status().is_success(), true);
    // build a plain request for the allowed user
    let mut reqs = HashMap::default();
    reqs.insert(
        allowed_name.clone(),
        vec![ReactionRequest::new(&group, &pipe.name)],
    );
    // build a plain request and one naming the deleted sample for the refused user
    reqs.insert(
        refused_name.clone(),
        vec![
            ReactionRequest::new(&group, &pipe.name),
            ReactionRequest::new(&group, &pipe.name).sample(&sub.sha256),
        ],
    );
    // bulk create reactions for both users as the admin
    let creates = admin.reactions.create_bulk_by_user(&reqs).await?;
    // make sure every request of the refused user got an indexed error entry
    let refused_resp = &creates[&refused_name];
    is_empty!(refused_resp.created);
    is!(refused_resp.errors.len(), 2);
    contains_key!(refused_resp.errors, &0);
    contains_key!(refused_resp.errors, &1);
    // make sure the allowed user's reaction was still created without errors
    let allowed_resp = &creates[&allowed_name];
    is_empty!(allowed_resp.errors);
    is!(allowed_resp.created.len(), 1);
    // list the reactions in this pipeline
    let mut cursor = admin.reactions.list(&group, &pipe.name);
    cursor.next().await?;
    // make sure only the allowed user's reaction exists
    is!(cursor.names.len(), 1);
    is_in!(cursor.names, allowed_resp.created[0].to_string());
    Ok(())
}

/// Create a user with a system role and add them to a group as a direct member with the user role
///
/// # Arguments
///
/// * `admin` - The admin client to create the user and update the group with
/// * `group` - The group to add the user to
/// * `role` - The system role to give the user
async fn group_member(admin: &Thorium, group: &str, role: UserRole) -> Result<Thorium, Error> {
    // add the user as a direct member and keep only their client
    let (client, _, _) = group_member_ext(admin, group, role, false).await?;
    Ok(client)
}

/// Create a user with a system role and add them to a group as a direct member or as a monitor
///
/// Returns the user's client, username, and password.
///
/// # Arguments
///
/// * `admin` - The admin client to create the user and update the group with
/// * `group` - The group to add the user to
/// * `role` - The system role to give the user
/// * `monitor` - Whether to add the user as a monitor (a viewer) instead of a direct member
async fn group_member_ext(
    admin: &Thorium,
    group: &str,
    role: UserRole,
    monitor: bool,
) -> Result<(Thorium, String, String), Error> {
    // generate a username for this user
    let username = generators::gen_string(24);
    // generate a password for this user
    let password = generators::gen_string(64);
    // build a user request that skips email verification and has the requested system role
    let create = UserCreate::new(&username, &password, "fake@fake.gov")
        .skip_verification()
        .role(role);
    // create the user
    Users::create(
        &admin.host,
        create,
        Some(&CONF.thorium.secret_key),
        &ClientSettings::default(),
    )
    .await?;
    // build the list of users to add to the group
    let users = GroupUsersUpdate::default().direct_add(&username);
    // build a group update adding the user as a monitor or as a direct member with the user role
    let update = if monitor {
        GroupUpdate::default().monitors(users)
    } else {
        GroupUpdate::default().users(users)
    };
    // add the user to the group
    admin.groups.update(group, &update).await?;
    // build a client for this user
    let client = Thorium::build(&admin.host)
        .basic_auth(username.clone(), password.clone())
        .build()
        .await?;
    Ok((client, username, password))
}

/// A bulk create checks the samples of every request and creates nothing when the user can't
/// see one of them
#[tokio::test]
async fn create_bulk_checks_samples() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a group for the member to create reactions in
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // create a group the member isn't in
    let hidden_group = generators::groups(1, &admin).await?.remove(0).name;
    // add a member without a developer role to the first group
    let user = group_member(&admin, &group, UserRole::User).await?;
    // create a pipeline in the member's group
    let pipe_req = generators::pipelines(&group, 1, false, &admin)
        .await?
        .remove(0);
    // get the created pipeline
    let pipe = admin.pipelines.get(&group, &pipe_req.name).await?;
    // upload a sample the member can see
    let visible = admin
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    // upload a sample only in the group the member isn't in
    let hidden = admin
        .files
        .create(generators::gen_sample(&hidden_group))
        .await?
        .sha256;
    // build a request naming no samples
    let plain = ReactionRequest::new(&group, &pipe.name);
    // build a request naming the hidden sample
    let hidden_req = ReactionRequest::new(&group, &pipe.name).sample(&hidden);
    // a single create naming the hidden sample is refused
    fail!(user.reactions.create(&hidden_req).await, 401);
    // bulk create a reaction naming only the hidden sample
    let resp = user
        .reactions
        .create_bulk(std::slice::from_ref(&hidden_req))
        .await;
    // make sure that bulk create was refused
    fail!(resp, 401);
    // bulk create reactions naming the hidden sample after a plain request
    let resp = user
        .reactions
        .create_bulk(&[plain.clone(), hidden_req.clone()])
        .await;
    // make sure that bulk create was refused too
    fail!(resp, 401);
    // list the reactions in this pipeline
    let mut cursor = admin.reactions.list(&group, &pipe.name);
    cursor.next().await?;
    // make sure the refused bulk creates made no reactions
    is_empty!(cursor.names);
    // build a request naming the sample the member can see
    let visible_req = ReactionRequest::new(&group, &pipe.name).sample(&visible);
    // bulk create reactions naming only samples the member can see
    let resp = user.reactions.create_bulk(&[plain, visible_req]).await?;
    // make sure both reactions were created without errors
    is!(resp.created.len(), 2);
    is_empty!(resp.errors);
    Ok(())
}

/// A bulk create checks the argument overrides of every request, not just the first request
/// for each pipeline, and creates nothing when one of them isn't allowed
#[tokio::test]
async fn create_bulk_checks_overrides_on_every_request() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a group to create reactions in
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add a member without a developer role
    let user = group_member(&admin, &group, UserRole::User).await?;
    // add a member with the developer role for k8s images only
    let developer = group_member(
        &admin,
        &group,
        UserRole::Developer {
            k8s: true,
            bare_metal: false,
            windows: false,
            kvm: false,
            external: false,
        },
    )
    .await?;
    // create two pipelines in this group
    let pipe_reqs = generators::pipelines(&group, 2, false, &admin).await?;
    // get the first created pipeline
    let first = admin.pipelines.get(&group, &pipe_reqs[0].name).await?;
    // get the second created pipeline
    let second = admin.pipelines.get(&group, &pipe_reqs[1].name).await?;
    // build the command to replace an image's command with
    let cmd = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "echo overridden".to_owned(),
    ];
    // build options overriding an image's command, positionals, and kwargs
    let opts = GenericJobOpts::new(true, true, Some(cmd));
    // build a plain request for the first pipeline
    let plain = ReactionRequest::new(&group, &first.name);
    // build a request overriding args for the first pipeline's first image
    let overridden = ReactionRequest::new(&group, &first.name).args(
        &first.order[0][0],
        GenericJobArgs::default().opts(opts.clone()),
    );
    // build a request overriding args for the second pipeline's first image
    let second_overridden = ReactionRequest::new(&group, &second.name)
        .args(&second.order[0][0], GenericJobArgs::default().opts(opts));
    // a single create with overrides is refused
    fail!(user.reactions.create(&overridden).await, 401);
    // bulk create with an overriding request after a plain one for the same pipeline
    let resp = user
        .reactions
        .create_bulk(&[plain.clone(), overridden.clone()])
        .await;
    // make sure that bulk create was refused
    fail!(resp, 401);
    // bulk create with the overriding request first
    let resp = user
        .reactions
        .create_bulk(&[overridden.clone(), plain.clone()])
        .await;
    // make sure that bulk create was refused
    fail!(resp, 401);
    // bulk create overriding args in a different pipeline than the plain request's
    let resp = user
        .reactions
        .create_bulk(&[plain.clone(), second_overridden])
        .await;
    // make sure that bulk create was refused
    fail!(resp, 401);
    // check both pipelines for reactions
    for pipe in [&first, &second] {
        // list the reactions in this pipeline
        let mut cursor = admin.reactions.list(&group, &pipe.name);
        cursor.next().await?;
        // make sure the refused creates made no reactions in this pipeline
        is_empty!(cursor.names);
    }
    // bulk create only plain requests as the member without a developer role
    let resp = user
        .reactions
        .create_bulk(&[plain.clone(), plain.clone()])
        .await?;
    // make sure both reactions were created without errors
    is!(resp.created.len(), 2);
    is_empty!(resp.errors);
    // bulk create a plain and an overriding request as the developer
    let resp = developer
        .reactions
        .create_bulk(&[plain, overridden])
        .await?;
    // make sure the developer's overrides were allowed and both reactions created
    is!(resp.created.len(), 2);
    is_empty!(resp.errors);
    Ok(())
}

#[tokio::test]
async fn create_sub_reaction() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let resp = client.reactions.create(&react_req).await?;
    // turn our old reaction request into a sub reaction and resubmit it
    let react_req = react_req.parent(resp.id);
    client.reactions.create(&react_req).await?;
    // make sure that our sub reaction counter incremented
    let reaction = client.reactions.get(&group, resp.id).await?;
    is!(reaction.sub_reactions, 1);
    Ok(())
}

#[tokio::test]
async fn create_banned() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // add a ban to the pipeline
    let pipe_update = PipelineUpdate::default().bans(
        PipelineBanUpdate::default()
            .add_ban(PipelineBan::new(PipelineBanKind::generic("Generic ban!"))),
    );
    client
        .pipelines
        .update(&group, &pipe_req.name, &pipe_update)
        .await?;
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let resp = client.reactions.create(&react_req).await;
    // make sure the reaction fails to spawn because of the ban
    fail!(resp, 400, "ban");
    // create a new pipeline whose stage is named after a stored image that gets banned
    test_utilities::faults::allow_image_writes(&group);
    let image_req = generators::gen_image(&group);
    client.images.create(&image_req).await?;
    let pipeline_name = "pipeline-with-ban-react";
    let instance = generators::tool_instance(&[&group], &client).await?;
    let pipe_req = PipelineRequest::new(
        &group,
        pipeline_name,
        serde_json::json!(vec![vec![&image_req.name]]),
    )
    .stage_tool(&image_req.name, instance);
    client.pipelines.create(&pipe_req).await?;
    let image_bans = vec![
        ImageBan::new(ImageBanKind::generic("Test ban 1!")),
        ImageBan::new(ImageBanKind::generic("Test ban 2!")),
    ];
    let update =
        ImageUpdate::default().bans(ImageBanUpdate::default().add_bans(image_bans.clone()));
    client
        .images
        .update(&group, &image_req.name, &update)
        .await?;
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let resp = client.reactions.create(&react_req).await;
    // make sure the reaction fails to spawn because of the ban
    fail!(resp, 400, "ban");
    Ok(())
}

#[tokio::test]
async fn list() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, resp) = generators::reactions(&group, 20, None, &client).await?;
    // list the reactions we just created
    let mut cursor = client.reactions.list(&group, &reactions[0].pipeline);
    // get the next page of results
    cursor.next().await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(cursor.names, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, _) = generators::reactions(&group, 20, None, &client).await?;
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list(&group, &reactions[0].pipeline)
        .details();
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    vec_in_vec!(&cursor.details, &reactions);
    Ok(())
}

#[tokio::test]
async fn list_status() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, resp) = generators::reactions(&group, 20, None, &client).await?;
    // list the reactions we just created
    let mut cursor =
        client
            .reactions
            .list_status(&group, &reactions[0].pipeline, &ReactionStatus::Created);
    cursor.next().await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(cursor.names, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_status_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, _) = generators::reactions(&group, 20, None, &client).await?;
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list_status(&group, &reactions[0].pipeline, &ReactionStatus::Created)
        .details();
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    vec_in_vec!(&cursor.details, &reactions);
    Ok(())
}

#[tokio::test]
async fn list_tag() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (_, resp) = generators::reactions(&group, 20, Some("test"), &client).await?;
    // list the reactions we just created
    let mut cursor = client.reactions.list_tag(&group, "test");
    cursor.next().await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(cursor.names, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_tag_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, _) = generators::reactions(&group, 20, Some("test_dets"), &client).await?;
    // list the reactions we just created
    let mut cursor = client.reactions.list_tag(&group, "test_dets").details();
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    vec_in_vec!(&cursor.details, &reactions);
    Ok(())
}

#[tokio::test]
async fn list_group_set() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (_, resp) = generators::reactions(&group, 20, Some("test"), &client).await?;
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list_group(&group, &ReactionStatus::Created);
    cursor.next().await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(cursor.names, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_group_set_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, _) = generators::reactions(&group, 20, Some("test_dets"), &client).await?;
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list_group(&group, &ReactionStatus::Created)
        .details();
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    vec_in_vec!(&cursor.details, &reactions);
    Ok(())
}

#[tokio::test]
async fn list_sub() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 1 random parent reactions
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // setup 20 random sub reactions
    let (_, sub_resp, _) = generators::sub_reactions(&group, 20, &resp.created[0], &client).await?;
    // list the reactions we just created
    let mut cursor = client.reactions.list_sub(&group, &resp.created[0]);
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    for created in &sub_resp {
        is_in!(cursor.names, created.id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_sub_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 1 random parent reactions
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // setup 20 random sub reactions
    let (sub_reactions, _, _) =
        generators::sub_reactions(&group, 20, &resp.created[0], &client).await?;
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list_sub(&group, &resp.created[0])
        .details();
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    vec_in_vec!(&cursor.details, &sub_reactions);
    Ok(())
}

#[tokio::test]
async fn list_sub_status() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 1 random parent reactions
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // setup 20 random sub reactions
    let (_, sub_resp, _) = generators::sub_reactions(&group, 20, &resp.created[0], &client).await?;
    // list the reactions we just created
    let mut cursor =
        client
            .reactions
            .list_sub_status(&group, &resp.created[0], &ReactionStatus::Created);
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    for created in &sub_resp {
        is_in!(cursor.names, created.id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_sub_status_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 1 random parent reactions
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    // setup 20 random sub reactions
    let (sub_reactions, _, _) =
        generators::sub_reactions(&group, 20, &resp.created[0], &client).await?;
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list_sub_status(&group, &resp.created[0], &ReactionStatus::Created)
        .details();
    cursor.next().await?;
    // make sure all the reactions we tried to create are in our list
    vec_in_vec!(&cursor.details, &sub_reactions);
    Ok(())
}

#[tokio::test]
async fn list_new() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, resp) = generators::reactions(&group, 20, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // build the options for listing reactions in this group
    let opts = ReactionCursorOpts::default().group(&group);
    // list the reactions we just created
    let cursor = client
        .reactions
        .list_new(&reactions[0].pipeline, &opts)
        .await?;
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(cursor.data, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_new_multi_group() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create two groups to list reactions across
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // use the same pipeline name in both groups
    let pipeline_name = "shared-mg-pipe";
    // track the reactions we create in both groups
    let mut created = Vec::with_capacity(20);
    // create the same pipeline and 10 reactions in each group
    for group in &groups {
        // create a random tool for this groups pipeline
        let (stage, instance) = generators::stage_tool(
            group,
            &generators::gen_image_spec_for(ImageScaler::K8s),
            &client,
        )
        .await?;
        // create a pipeline with the shared name in this group
        let pipe_req =
            PipelineRequest::new(group, pipeline_name, serde_json::json!(vec![vec![&stage]]))
                .stage_tool(&stage, instance);
        client.pipelines.create(&pipe_req).await?;
        // get the pipeline we just created
        let pipe = client.pipelines.get(group, pipeline_name).await?;
        // create 10 random reactions for this groups pipeline
        let react_reqs: Vec<ReactionRequest> = (0..10)
            .map(|_| generators::gen_reaction(group, &pipe, None))
            .collect();
        let resp = client.reactions.create_bulk(&react_reqs).await?;
        // make sure no errors were returned
        is_empty!(resp.errors);
        // track the reactions we created in this group
        created.extend(resp.created);
    }
    // build the options for listing reactions across both groups
    let opts = ReactionCursorOpts::default().groups(groups);
    // list the reactions we just created across both groups
    let cursor = client.reactions.list_new(pipeline_name, &opts).await?;
    // make sure all the reactions we tried to create are in our list
    for id in &created {
        is_in!(cursor.data, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_new_paged() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, resp) = generators::reactions(&group, 20, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // build the options for listing reactions in small pages
    let opts = ReactionCursorOpts::default().group(&group).page_size(5);
    // list the first page of reactions we just created
    let mut cursor = client
        .reactions
        .list_new(&reactions[0].pipeline, &opts)
        .await?;
    // track all the reaction ids we gather across pages
    let mut gathered = cursor.data.clone();
    // walk the remaining pages of this cursor
    while !cursor.exhausted() {
        // get the next page of reactions
        cursor.refill().await?;
        // add this pages reactions to our gathered list
        gathered.extend(cursor.data.iter().cloned());
    }
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(gathered, id.to_string());
    }
    // make sure no duplicate reactions were returned across pages
    let unique = gathered.iter().collect::<HashSet<_>>();
    is!(gathered.len(), unique.len());
    Ok(())
}

#[tokio::test]
async fn list_status_new() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (reactions, resp) = generators::reactions(&group, 20, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // build the options for listing reactions in this group
    let opts = ReactionCursorOpts::default().group(&group);
    // list the created reactions we just created
    let cursor = client
        .reactions
        .list_status_new(&reactions[0].pipeline, &ReactionStatus::Created, &opts)
        .await?;
    // make sure all the reactions we tried to create are in our list
    for id in &resp.created {
        is_in!(cursor.data, id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_tag_new_multi_group() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create two groups to list reactions across
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // track the reactions we create in both groups
    let mut created = Vec::with_capacity(20);
    // create 10 tagged reactions in each group
    for group in &groups {
        // setup 10 random reactions with a shared tag
        let (_, resp) = generators::reactions(group, 10, Some("mg_tag"), &client).await?;
        // make sure no errors were returned
        is_empty!(resp.errors);
        // track the reactions we created in this group
        created.extend(resp.created);
    }
    // build the options for listing reactions across both groups in small pages
    let opts = ReactionCursorOpts::default().groups(groups).page_size(5);
    // list the first page of reactions with this tag
    let mut cursor = client.reactions.list_tag_new("mg_tag", &opts).await?;
    // track all the reaction ids we gather across pages
    let mut gathered = cursor.data.clone();
    // walk the remaining pages of this cursor
    while !cursor.exhausted() {
        // get the next page of reactions
        cursor.refill().await?;
        // add this pages reactions to our gathered list
        gathered.extend(cursor.data.iter().cloned());
    }
    // make sure all the reactions we tried to create are in our list
    for id in &created {
        is_in!(gathered, id.to_string());
    }
    // make sure no duplicate reactions were returned across pages
    let unique = gathered.iter().collect::<HashSet<_>>();
    is!(gathered.len(), unique.len());
    Ok(())
}

#[tokio::test]
async fn list_sub_new() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 1 random parent reaction
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // setup 20 random sub reactions
    let (_, sub_resp, _) = generators::sub_reactions(&group, 20, &resp.created[0], &client).await?;
    // build the options for listing sub reactions in this group
    let opts = ReactionCursorOpts::default().group(&group);
    // list the sub reactions we just created
    let cursor = client
        .reactions
        .list_sub_new(&resp.created[0], &opts)
        .await?;
    // make sure all the sub reactions we tried to create are in our list
    for created in &sub_resp {
        is_in!(cursor.data, created.id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn list_sub_status_new() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 1 random parent reaction
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    // make sure no errors were returned
    is_empty!(resp.errors);
    // setup 20 random sub reactions
    let (_, sub_resp, _) = generators::sub_reactions(&group, 20, &resp.created[0], &client).await?;
    // build the options for listing sub reactions in this group
    let opts = ReactionCursorOpts::default().group(&group);
    // list the created sub reactions we just created
    let cursor = client
        .reactions
        .list_sub_status_new(&resp.created[0], &ReactionStatus::Created, &opts)
        .await?;
    // make sure all the sub reactions we tried to create are in our list
    for created in &sub_resp {
        is_in!(cursor.data, created.id.to_string());
    }
    Ok(())
}

#[tokio::test]
async fn claim() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0", "node0", "claim", &group, &pipe.name, stage, &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, id.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // complete this job
        client.jobs.proceed(&jobs[0], &logs, 2, "claim").await?;
        // delete our worker
        generators::delete_worker("claim", &client).await?;
    }
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, id.id.to_string());
    Ok(())
}

#[tokio::test]
async fn expires() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None).tag("TestTag");
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0", "node0", "expires", &group, &pipe.name, stage, &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "expires",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, id.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // complete this job
        client.jobs.proceed(&jobs[0], &logs, 2, "expires").await?;
        // delete our worker
        generators::delete_worker("expires", &client).await?;
    }
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, id.id.to_string());
    // list the reactions we created our in our TestTag list
    let mut reactions = client.reactions.list_tag(&group, "TestTag");
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, id.id.to_string());
    // wait 10 seconds and then clean up old reactions
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    client.system.cleanup().await?;
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions = client
        .reactions
        .list_status(&group, &pipe.name, &ReactionStatus::Completed)
        .exec()
        .await?;
    reactions.next().await?;
    // make sure our reaction list is empty
    is!(reactions.names.is_empty(), true);
    // list the reactions with our test tag
    let mut reactions = client.reactions.list_tag(&group, "TestTag");
    reactions.next().await?;
    // make sure our reaction list is empty
    is!(reactions.names.is_empty(), true);
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn claim_generator() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::gen_generator_pipe(&group, &client).await?;
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // get a flattened list of stages
    let stages: Vec<String> = pipe.order.clone().into_iter().flatten().collect();
    // register a worker for the second stage's tool instance
    generators::worker(
        "cluster0",
        "node0",
        "claim_generator_next",
        &group,
        &pipe.name,
        &stages[1],
        &client,
    )
    .await?;
    // attempt to claim and proceed with a generator 3 times
    for _ in 0..3 {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_generator",
            &group,
            &pipe.name,
            &stages[0],
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, &stages[0]),
                "cluster0",
                "node0",
                "claim_generator",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, id.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // sleep this generator
        client.jobs.sleep(&jobs[0].id, "checkpoint-1").await?;
        // complete this job
        client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_generator")
            .await?;
        // try to claim the next stage of the reaction and make sure we don't get a job
        let next_stage = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, &stages[1]),
                "cluster0",
                "node0",
                "claim_generator_next",
                1,
            )
            .await?;
        // delete our worker
        generators::delete_worker("claim_generator", &client).await?;
        // make sure we failed to claim a job due to the previous stage failing
        is!(next_stage.len(), 0);
    }
    // register our test worker
    generators::worker(
        "cluster0",
        "node0",
        "claim_generator",
        &group,
        &pipe.name,
        &stages[0],
        &client,
    )
    .await?;
    // try to claim a generator job for the last time
    let jobs = client
        .jobs
        .claim_workload(
            &req.group,
            generators::stage_instance(&pipe, &stages[0]),
            "cluster0",
            "node0",
            "claim_generator",
            1,
        )
        .await?;
    // get our reactions data
    let react = client.reactions.get(&req.group, id.id).await?;
    // make sure this job matches our reaction
    is!(&jobs, react);
    // build random stage logs
    let logs = generators::stage_logs();
    // complete this job
    client
        .jobs
        .proceed(&jobs[0], &logs, 2, "claim_generator")
        .await?;
    // claim the final stage of this reaction and complete it
    let jobs = client
        .jobs
        .claim_workload(
            &req.group,
            generators::stage_instance(&pipe, &stages[1]),
            "cluster0",
            "node0",
            "claim_generator_next",
            1,
        )
        .await?;
    // get our reactions data
    let react = client.reactions.get(&req.group, id.id).await?;
    // make sure this job matches our reaction
    is!(&jobs, react);
    // build random stage logs
    let logs = generators::stage_logs();
    // complete this job
    client
        .jobs
        .proceed(&jobs[0], &logs, 2, "claim_generator_next")
        .await?;
    // delete our workers
    generators::delete_worker("claim_generator", &client).await?;
    generators::delete_worker("claim_generator_next", &client).await?;
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, id.id.to_string());
    Ok(())
}

#[tokio::test]
async fn claim_generator_fail() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::gen_generator_pipe(&group, &client).await?;
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;

    // get a flattened list of stages
    let stages: Vec<String> = pipe.order.clone().into_iter().flatten().collect();
    // attempt to claim and proceed with a generator 3 times
    for _ in 0..3 {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_generator_fail",
            &group,
            &pipe.name,
            &stages[0],
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, &stages[0]),
                "cluster0",
                "node0",
                "claim_generator_fail",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, id.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // sleep this generator
        client.jobs.sleep(&jobs[0].id, "checkpoint-1").await?;
        // complete this job
        client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_generator_fail")
            .await?;
        // delete our worker
        generators::delete_worker("claim_generator_fail", &client).await?;
    }
    // register our test worker
    generators::worker(
        "cluster0",
        "node0",
        "claim_generator_fail",
        &group,
        &pipe.name,
        &stages[0],
        &client,
    )
    .await?;
    // try to claim a generator job for the last time
    let jobs = client
        .jobs
        .claim_workload(
            &req.group,
            generators::stage_instance(&pipe, &stages[0]),
            "cluster0",
            "node0",
            "claim_generator_fail",
            1,
        )
        .await?;
    // get our reactions data
    let react = client.reactions.get(&req.group, id.id).await?;
    // make sure this job matches our reaction
    is!(&jobs, react);
    // build random stage logs
    let logs = generators::stage_logs();
    // fail this job
    client
        .jobs
        .error(&jobs[0].id, &logs, "claim_generator_fail")
        .await?;
    // try claim the final stage of this reaction and make sure it doesn't exist
    generators::worker(
        "cluster0",
        "node0",
        "claim_generator_fail_next",
        &group,
        &pipe.name,
        &stages[1],
        &client,
    )
    .await?;
    let jobs = client
        .jobs
        .claim_workload(
            &req.group,
            generators::stage_instance(&pipe, &stages[1]),
            "cluster0",
            "node0",
            "claim_generator_fail_next",
            1,
        )
        .await?;
    // delete our workers
    generators::delete_worker("claim_generator_fail", &client).await?;
    generators::delete_worker("claim_generator_fail_next", &client).await?;
    // make sure we failed to claim a job due to the previous stage failing
    is!(jobs.len(), 0);
    Ok(())
}

#[tokio::test]
async fn claim_sub_reacts_status() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let resp = client.reactions.create(&req).await?;
    let (_, sub_resp, sub_pipe) = generators::sub_reactions(&group, 1, &resp.id, &client).await?;
    // get the id of the sub reaction we created
    let ids: Vec<Uuid> = sub_resp.into_iter().map(|created| created.id).collect();
    let id = ids[0].to_string();
    // list the reactions we just created
    let mut cursor = client
        .reactions
        .list_sub_status(&group, &resp.id, &ReactionStatus::Created);
    cursor.next().await?;
    // make sure our sub reaction is in the created status list
    is_in!(cursor.names, id);
    // complete all stages of this sub reaction
    for stage in sub_pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_sub_reacts_status",
            &group,
            &sub_pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job for this sub reaction
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&sub_pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_status",
                1,
            )
            .await?;
        // build random stage logs
        let logs = generators::stage_logs();
        // complete this sub reactions job
        let status_resp = client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_sub_reacts_status")
            .await?;
        // delete our worker
        generators::delete_worker("claim_sub_reacts_status", &client).await?;
        // make sure our sub reactions status is in the correct list
        let status = ReactionStatus::from(status_resp.status);
        // list the reactions we just created
        let mut cursor = client.reactions.list_sub_status(&group, &resp.id, &status);
        cursor.next().await?;
        // make sure our sub reaction is in the correct status list
        is_in!(cursor.names, id);
    }
    Ok(())
}

#[tokio::test]
async fn claim_sub_reacts() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::gen_generator_pipe(&group, &client).await?;
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let resp = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_sub_reacts",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts",
                3,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, resp.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // create 3 random sub reactions based on a random pipeline
        let (sub_reacts, _, sub_pipe) =
            generators::sub_reactions(&group, 3, &resp.id, &client).await?;
        // build random stage logs
        let logs = generators::stage_logs();
        // complete this job
        client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_sub_reacts")
            .await?;
        // try to claim a job that shouldn't exist
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts",
                1,
            )
            .await?;
        // delete our worker
        generators::delete_worker("claim_sub_reacts", &client).await?;
        // make sure we failed to claim a job due to the sub reactions not being complete yet
        is!(jobs.len(), 0);
        // complete all sub reactions
        for _ in sub_reacts {
            // complete all stages of each sub reaction
            for stage in sub_pipe.order.iter().flatten() {
                // register our test worker
                generators::worker(
                    "cluster0",
                    "node0",
                    "claim_sub_reacts_sub",
                    &group,
                    &sub_pipe.name,
                    stage,
                    &client,
                )
                .await?;
                // try to claim a job for this sub reaction
                let jobs = client
                    .jobs
                    .claim_workload(
                        &req.group,
                        generators::stage_instance(&sub_pipe, stage),
                        "cluster0",
                        "node0",
                        "claim_sub_reacts_sub",
                        1,
                    )
                    .await?;
                // build random stage logs
                let logs = generators::stage_logs();
                // complete this sub reactions job
                client
                    .jobs
                    .proceed(&jobs[0], &logs, 2, "claim_sub_reacts_sub")
                    .await?;
                // delete our worker
                generators::delete_worker("claim_sub_reacts_sub", &client).await?;
            }
        }
    }
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, resp.id.to_string());
    Ok(())
}

#[tokio::test]
async fn claim_sub_reacts_fail() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let resp = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_sub_reacts_fail",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_fail",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, resp.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // create 3 random sub reactions based on a random pipeline
        let (sub_reacts, _, sub_pipe) =
            generators::sub_reactions(&group, 3, &resp.id, &client).await?;
        // build random stage logs
        let logs = generators::stage_logs();
        // complete this job
        client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_sub_reacts_fail")
            .await?;
        // try to claim a job that shouldn't exist yet
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_fail",
                1,
            )
            .await?;
        // delete our worker
        generators::delete_worker("claim_sub_reacts_fail", &client).await?;
        // make sure we failed to claim a job due to the sub reactions not being failed yet
        is!(jobs.len(), 0);
        // fail all sub reactions
        for _ in sub_reacts {
            // fail the first stage of all sub reactions
            for stage in sub_pipe.order.iter().flatten().take(1) {
                // register our test worker
                generators::worker(
                    "cluster0",
                    "node0",
                    "claim_sub_reacts_fail_sub",
                    &group,
                    &sub_pipe.name,
                    stage,
                    &client,
                )
                .await?;
                // try to claim a job for this sub reaction
                let jobs = client
                    .jobs
                    .claim_workload(
                        &req.group,
                        generators::stage_instance(&sub_pipe, stage),
                        "cluster0",
                        "node0",
                        "claim_sub_reacts_fail_sub",
                        1,
                    )
                    .await?;
                // build random stage logs
                let logs = generators::stage_logs();
                // complete this sub reactions job
                client
                    .jobs
                    .error(&jobs[0].id, &logs, "claim_sub_reacts_fail_sub")
                    .await?;
                // delete our worker
                generators::delete_worker("claim_sub_reacts_fail_sub", &client).await?;
            }
        }
    }
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, resp.id.to_string());
    Ok(())
}

#[tokio::test]
async fn claim_sub_reacts_race() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let resp = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_sub_reacts_race",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_race",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, resp.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // create 3 random sub reactions based on a random pipeline
        let (sub_reacts, _, sub_pipe) =
            generators::sub_reactions(&group, 3, &resp.id, &client).await?;
        // build random stage logs
        let logs = generators::stage_logs();
        // complete all sub reactions
        for _ in sub_reacts {
            // complete all stages of each sub reaction
            for stage in sub_pipe.order.iter().flatten() {
                // register our test worker
                generators::worker(
                    "cluster0",
                    "node0",
                    "claim_sub_reacts_race_sub",
                    &group,
                    &sub_pipe.name,
                    stage,
                    &client,
                )
                .await?;
                // try to claim a job for this sub reaction
                let jobs = client
                    .jobs
                    .claim_workload(
                        &req.group,
                        generators::stage_instance(&sub_pipe, stage),
                        "cluster0",
                        "node0",
                        "claim_sub_reacts_race_sub",
                        1,
                    )
                    .await?;
                // build random stage logs
                let logs = generators::stage_logs();
                // complete this sub reactions job
                client
                    .jobs
                    .proceed(&jobs[0], &logs, 2, "claim_sub_reacts_race_sub")
                    .await?;
                // delete our worker
                generators::delete_worker("claim_sub_reacts_race_sub", &client).await?;
            }
        }
        // try to claim a job that shouldn't exist yet
        let check_jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_race",
                1,
            )
            .await?;
        // make sure we failed to claim a job due to the sub reactions not being complete yet
        is!(check_jobs.len(), 0);
        // complete this job
        client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_sub_reacts_race")
            .await?;
        // delete our worker
        generators::delete_worker("claim_sub_reacts_race", &client).await?;
    }
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, resp.id.to_string());
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn claim_sub_reacts_race_fail() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let resp = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_sub_reacts_race_fail",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_race_fail",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&req.group, resp.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // create 3 random sub reactions based on a random pipeline
        let (sub_reacts, _, sub_pipe) =
            generators::sub_reactions(&group, 3, &resp.id, &client).await?;
        // build random stage logs
        let logs = generators::stage_logs();
        // complete all sub reactions
        for _ in sub_reacts {
            // complete all stages of each sub reaction
            for stage in sub_pipe.order.iter().flatten().take(1) {
                // register our test worker
                generators::worker(
                    "cluster0",
                    "node0",
                    "claim_sub_reacts_race_fail_sub",
                    &group,
                    &sub_pipe.name,
                    stage,
                    &client,
                )
                .await?;
                // try to claim a job for this sub reaction
                let jobs = client
                    .jobs
                    .claim_workload(
                        &req.group,
                        generators::stage_instance(&sub_pipe, stage),
                        "cluster0",
                        "node0",
                        "claim_sub_reacts_race_fail_sub",
                        1,
                    )
                    .await?;
                // build random stage logs
                let logs = generators::stage_logs();
                // complete this sub reactions job
                client
                    .jobs
                    .error(&jobs[0].id, &logs, "claim_sub_reacts_race_fail_sub")
                    .await?;
                // delete our worker
                generators::delete_worker("claim_sub_reacts_race_fail_sub", &client).await?;
            }
        }
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "claim_sub_reacts_race_fail_empty",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job that shouldn't exist yet
        let check_jobs = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "claim_sub_reacts_race_fail_empty",
                1,
            )
            .await?;
        // delete our worker
        generators::delete_worker("claim_sub_reacts_race_fail_empty", &client).await?;
        // make sure we failed to claim a job due to the sub reactions not being complete yet
        is!(check_jobs.len(), 0);
        // complete this job
        client
            .jobs
            .proceed(&jobs[0], &logs, 2, "claim_sub_reacts_race_fail_empty")
            .await?;
        // delete our worker
        generators::delete_worker("claim_sub_reacts_race_fail", &client).await?;
    }
    // list the reactions for our pipeline that now has a status of completed
    let mut reactions =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    reactions.next().await?;
    // make sure our reaction id is in the list
    is_in!(reactions.names, resp.id.to_string());
    Ok(())
}

#[tokio::test]
async fn update() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let create = client.reactions.create(&react_req).await?;
    // build an update for this reaction
    let update = ReactionUpdate::default().tag("wooooooot").arg(
        &pipe.order[0][0],
        GenericJobArgsUpdate::default()
            .positionals(vec!["1", "2", "3"])
            .kwarg("--new", vec!["ImANewArg"]),
    );
    // update this reaction
    let reaction = client.reactions.update(&group, &create.id, &update).await?;
    // make sure our reaction was applied
    is!(reaction, update);
    Ok(())
}

#[tokio::test]
async fn delete() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, Some("CornTag"));
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // list the reactions for our pipeline
    let mut reactions = client.reactions.list(&group, &pipe.name);
    reactions.next().await?;
    // make sure our reaction id is in this list
    is_in!(reactions.names, id.id.to_string());
    // list the reactions for this group
    let mut reactions = client
        .reactions
        .list_group(&group, &ReactionStatus::Created);
    reactions.next().await?;
    // make sure our reaction id is in this list
    is_in!(reactions.names, id.id.to_string());
    // list the reactions for our pipeline that now has a status of created
    let mut reactions = client
        .reactions
        .list_status(&group, &pipe.name, &ReactionStatus::Created);
    reactions.next().await?;
    // make sure our reaction id is in this list
    is_in!(reactions.names, id.id.to_string());
    // list the reactions with a specific tag
    let mut reactions = client.reactions.list_tag(&group, "CornTag");
    reactions.next().await?;
    // make sure our reaction id is in this list
    is_in!(reactions.names, id.id.to_string());
    // delete this reaction
    client.reactions.delete(&group, &id.id).await?;
    // list the reactions for our pipeline
    let mut reactions = client.reactions.list(&group, &pipe.name);
    reactions.next().await?;
    // make sure our reaction id is not in this list
    is_not_in!(reactions.names, id.id.to_string());
    // list the reactions for our pipeline
    let mut reactions = client
        .reactions
        .list_group(&group, &ReactionStatus::Created);
    reactions.next().await?;
    // make sure our reaction id is not in this list
    is_not_in!(reactions.names, id.id.to_string());
    // list the reactions for our pipeline that now has a status of created
    let mut reactions = client
        .reactions
        .list_status(&group, &pipe.name, &ReactionStatus::Created);
    reactions.next().await?;
    // make sure our reaction id is not in this list
    is_not_in!(reactions.names, id.id.to_string());
    // list the reactions with a specific tag
    let mut reactions = client.reactions.list_tag(&group, "CornTag");
    reactions.next().await?;
    // make sure our reaction id is not in this list
    is_not_in!(reactions.names, id.id.to_string());
    Ok(())
}

#[tokio::test]
async fn ephemeral() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req =
        generators::gen_reaction(&group, &pipe, None).buffer("test-file", "I am a test file");
    let resp = client.reactions.create(&react_req).await?;
    // download the ephemeral file and make sure its correct
    let download = client
        .reactions
        .download_ephemeral(&group, &resp.id, "test-file")
        .await?;
    is!(download, "I am a test file");
    // get the created reaction
    let created = client.reactions.get(&group, resp.id).await?;
    // make sure our reaction request matches what was created
    is!(created, react_req);
    // complete this reaction to make sure purging epehemeral files works
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0",
            "node0",
            "ephemeral",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job
        let jobs = client
            .jobs
            .claim_workload(
                &group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "ephemeral",
                1,
            )
            .await?;
        // get our reactions data
        let react = client.reactions.get(&group, created.id).await?;
        // make sure this job matches our reaction
        is!(&jobs, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // complete this job
        client.jobs.proceed(&jobs[0], &logs, 2, "ephemeral").await?;
        // delete our worker
        generators::delete_worker("ephemeral", &client).await?;
    }
    Ok(())
}

#[tokio::test]
async fn ephemeral_fail() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let react_req =
        generators::gen_reaction(&group, &pipe, None).buffer("test-file", "I am a test file");
    let resp = client.reactions.create(&react_req).await?;
    // get the created reaction
    let created = client.reactions.get(&group, resp.id).await?;
    // make sure our reaction request matches what was created
    is!(created, react_req);
    // get the next stage so we can claim the job and then fail it
    let stage = pipe.order.iter().flatten().next().unwrap();
    // register our test worker
    generators::worker(
        "cluster0",
        "node0",
        "ephemeral_fail",
        &group,
        &pipe.name,
        stage,
        &client,
    )
    .await?;
    // try to claim a job for this sub reaction
    let jobs = client
        .jobs
        .claim_workload(
            &group,
            generators::stage_instance(&pipe, stage),
            "cluster0",
            "node0",
            "ephemeral_fail",
            1,
        )
        .await?;
    // build random stage logs
    let logs = generators::stage_logs();
    // complete this sub reactions job
    client
        .jobs
        .error(&jobs[0].id, &logs, "ephemeral_fail")
        .await?;
    // delete our worker
    generators::delete_worker("ephemeral_fail", &client).await?;
    Ok(())
}

#[tokio::test]
async fn parent_ephemeral() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random parent reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None)
        .buffer("parent-file", "I am a parent test file");
    let resp = client.reactions.create(&react_req).await?;
    // Create a random child reaction based on our pipeline request
    let react_req = generators::gen_reaction(&group, &pipe, None)
        .parent(resp.id)
        .buffer("child-file", "I am a child test file");
    let resp = client.reactions.create(&react_req).await?;
    // download the child ephemeral file and make sure its correct
    let download = client
        .reactions
        .download_ephemeral(&group, &resp.id, "child-file")
        .await?;
    is!(download, "I am a child test file");
    // get the created reaction
    let created = client.reactions.get(&group, resp.id).await?;
    // make sure our reaction request matches what was created
    is!(created, react_req);
    // make sure our parent ephemeral file was propogated
    is!(
        created.parent_ephemeral.get("parent-file"),
        created.parent.as_ref()
    );
    // download the child ephemeral file and make sure its correct
    let download = client
        .reactions
        .download_ephemeral(&group, &created.parent.unwrap(), "parent-file")
        .await?;
    is!(download, "I am a parent test file");
    Ok(())
}

/// Updating a reaction's ephemeral file overwrites the stored object, so a download after the
/// update returns the new content rather than the stale original
///
/// `upload_base64` PUTs the file even when the key already exists, so updating a reaction's
/// ephemeral file under the same name replaces its old content.
#[tokio::test]
async fn updating_ephemeral_file_overwrites_stale_content() -> Result<(), Error> {
    // the original bytes uploaded when the reaction is created
    const ORIGINAL: &str = "I am the original config";
    // the corrected bytes the update tries to replace them with, under the same file name
    const UPDATED: &str = "I am the corrected config after a retry";
    // get an admin client to drive the reaction routes
    let client = test_utilities::admin_client().await?;
    // register a test node so the reaction has somewhere to schedule
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group to hold this reaction
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline to react against
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // fetch the full pipeline so we can build a reaction for it
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // create a reaction carrying an ephemeral file with the original content
    let react_req = generators::gen_reaction(&group, &pipe, None).buffer("config.json", ORIGINAL);
    let resp = client.reactions.create(&react_req).await?;
    // sanity check the file downloads as the original content before the update
    let before = client
        .reactions
        .download_ephemeral(&group, &resp.id, "config.json")
        .await?;
    is!(before, ORIGINAL);
    // build an update that supplies new content under the same ephemeral file name; ReactionUpdate
    // has no ephemeral() builder so populate the map directly, base64 encoding the bytes the route
    // expects
    let mut update = ReactionUpdate::default();
    update
        .ephemeral
        .insert("config.json".to_owned(), BASE64.encode(UPDATED.as_bytes()));
    // apply the update; the api reports success either way
    client.reactions.update(&group, &resp.id, &update).await?;
    // download the file again; it has the corrected content, not the stale original
    let after = client
        .reactions
        .download_ephemeral(&group, &resp.id, "config.json")
        .await?;
    is!(after, UPDATED);
    Ok(())
}

/// Ephemeral files added by a reaction update are recorded on the reaction
///
/// `build` always sets the ephemeral list when the reaction is created, so
/// `db::reactions::update` overwrites that list rather than only setting it when missing. This
/// keeps a new file from being uploaded to S3 while the reaction (and so its jobs) never lists it.
#[tokio::test]
async fn updating_reaction_records_new_ephemeral_files() -> Result<(), Error> {
    // get an admin client to drive the reaction routes
    let client = test_utilities::admin_client().await?;
    // register a test node so the reaction has somewhere to schedule
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group to hold this reaction
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline to react against
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // fetch the full pipeline so we can build a reaction for it
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // create a reaction carrying one ephemeral file
    let react_req =
        generators::gen_reaction(&group, &pipe, None).buffer("original.json", "original");
    let resp = client.reactions.create(&react_req).await?;
    // add a second ephemeral file in an update
    let mut update = ReactionUpdate::default();
    update
        .ephemeral
        .insert("added.json".to_owned(), BASE64.encode("added".as_bytes()));
    client.reactions.update(&group, &resp.id, &update).await?;
    // the reaction must list both its original and its newly added ephemeral file
    let react = client.reactions.get(&group, resp.id).await?;
    is_in!(react.ephemeral, "original.json".to_owned());
    is_in!(react.ephemeral, "added.json".to_owned());
    // and the added file is downloadable
    let added = client
        .reactions
        .download_ephemeral(&group, &resp.id, "added.json")
        .await?;
    is!(added, "added");
    Ok(())
}

/// Send a raw reaction handle command since the client has no method for this route
///
/// A rejected command returns an error carrying the status and response body, so a caller
/// fails here instead of at a later assertion that assumed the command was applied.
///
/// # Arguments
///
/// * `client` - The admin client whose host to send this command to
/// * `group` - The group the reaction is in
/// * `id` - The reaction to handle a command for
/// * `cmd` - The command to send (`proceed` or `fail`)
async fn handle_reaction(
    client: &Thorium,
    group: &str,
    id: &Uuid,
    cmd: &str,
) -> Result<reqwest::StatusCode, Error> {
    // build the url for this reactions handle route
    let url = format!("{}/api/reactions/handle/{group}/{id}/{cmd}", client.host);
    // send this command as the test admin using basic auth
    let resp = reqwest::Client::new()
        .post(&url)
        .basic_auth("thorium", Some("password"))
        .send()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // get the status of this command
    let status = resp.status();
    // surface the status and body of any rejected command
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::new(format!(
            "handle {cmd} for reaction {id} returned {status}: {body}"
        )));
    }
    Ok(status)
}

/// Register a worker for and claim the job of every image in a single stage pipeline
///
/// # Arguments
///
/// * `client` - The admin client to use
/// * `group` - The group the pipeline is in
/// * `pipe` - The single stage pipeline to claim jobs for
/// * `prefix` - The prefix to use for each of our worker names
async fn claim_stage(
    client: &Thorium,
    group: &str,
    pipe: &Pipeline,
    prefix: &str,
) -> Result<Vec<GenericJob>, Error> {
    // claim the job for each image in our only stage
    let mut jobs = Vec::with_capacity(pipe.order[0].len());
    for (i, image) in pipe.order[0].iter().enumerate() {
        // register a worker for this image
        let worker = format!("{prefix}_{i}");
        generators::worker(
            "cluster0", "node0", &worker, group, &pipe.name, image, client,
        )
        .await?;
        // claim this images job
        let mut claimed = client
            .jobs
            .claim_workload(
                group,
                generators::stage_instance(pipe, image),
                "cluster0",
                "node0",
                &worker,
                1,
            )
            .await?;
        jobs.push(claimed.remove(0));
    }
    Ok(jobs)
}

/// Delete the workers registered by [`claim_stage`]
///
/// # Arguments
///
/// * `client` - The admin client to use
/// * `pipe` - The single stage pipeline jobs were claimed for
/// * `prefix` - The prefix used for each of our worker names
async fn delete_stage_workers(
    client: &Thorium,
    pipe: &Pipeline,
    prefix: &str,
) -> Result<(), Error> {
    for i in 0..pipe.order[0].len() {
        generators::delete_worker(&format!("{prefix}_{i}"), client).await?;
    }
    Ok(())
}

/// A reaction the user failed (canceled) while its jobs were running stays Failed
///
/// `proceed` refuses Failed reactions as well as Completed ones, so when the canceled reaction's
/// still running jobs finish, the reaction is not resurrected and flipped to Completed.
#[tokio::test]
async fn failed_reaction_not_resurrected_by_job_completion() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group to test in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline with a single stage of three images
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // create a reaction and claim all of its jobs so they are running
    let req = generators::gen_reaction(&group, &pipe, None);
    let resp = client.reactions.create(&req).await?;
    let jobs = claim_stage(&client, &group, &pipe, "c5_01").await?;
    // the user fails (cancels) the reaction while its jobs are running
    is!(
        handle_reaction(&client, &group, &resp.id, "fail").await?,
        reqwest::StatusCode::ACCEPTED
    );
    // the running jobs then finish; ignore their results since they may be rejected
    let logs = generators::stage_logs();
    for (i, job) in jobs.iter().enumerate() {
        let _ = client
            .jobs
            .proceed(job, &logs, 1, &format!("c5_01_{i}"))
            .await;
    }
    // the canceled reaction must still be failed
    let react = client.reactions.get(&group, resp.id).await?;
    is!(react.status, ReactionStatus::Failed);
    // and it must not be listed as completed
    let mut completed =
        client
            .reactions
            .list_status(&group, &pipe.name, &ReactionStatus::Completed);
    completed.next().await?;
    is_not_in!(completed.names, resp.id.to_string());
    delete_stage_workers(&client, &pipe, "c5_01").await?;
    Ok(())
}

/// A failed sub reaction only counts once toward its parent's completed sub reactions
///
/// Failing the sub reaction increments its parent's `completed_sub_reactions`. A Failed reaction
/// is never proceeded, so the sub reaction's running jobs finishing afterwards doesn't resurrect
/// and complete it and increment the count again.
#[tokio::test]
async fn failed_sub_reaction_counts_once_in_parent() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group to test in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a parent reaction and a single sub reaction
    let (_, parent) = generators::reactions(&group, 1, None, &client).await?;
    let parent = parent.created[0];
    let (_, subs, sub_pipe) = generators::sub_reactions(&group, 1, &parent, &client).await?;
    // claim all of the sub reaction's jobs so they are running
    let jobs = claim_stage(&client, &group, &sub_pipe, "c5_01_sub").await?;
    // the user fails the sub reaction while its jobs are running
    handle_reaction(&client, &group, &subs[0].id, "fail").await?;
    // the running jobs then finish; ignore their results since they may be rejected
    let logs = generators::stage_logs();
    for (i, job) in jobs.iter().enumerate() {
        let _ = client
            .jobs
            .proceed(job, &logs, 1, &format!("c5_01_sub_{i}"))
            .await;
    }
    // the parent must count its single sub reaction exactly once
    let parent = client.reactions.get(&group, parent).await?;
    is!(parent.completed_sub_reactions, 1);
    delete_stage_workers(&client, &sub_pipe, "c5_01_sub").await?;
    Ok(())
}

/// Paging through a group's reaction list advances past the first page
///
/// `list_group_set` uses the cursor as the rank to start the next page at, so each page picks up
/// where the last one left off and the listing reaches every created reaction without repeats.
#[tokio::test]
async fn list_group_set_paged() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random reactions
    let (_, resp) = generators::reactions(&group, 20, None, &client).await?;
    is_empty!(resp.errors);
    // list the reactions we just created in pages of 5
    let mut cursor = client
        .reactions
        .list_group(&group, &ReactionStatus::Created)
        .page_size(5);
    // gather every page, capping the pages so a cursor that never advances cannot loop forever
    let mut gathered = Vec::new();
    for _ in 0..10 {
        cursor.next().await?;
        gathered.extend(cursor.names.iter().cloned());
        if cursor.exhausted {
            break;
        }
    }
    // every reaction we created must be listed
    for id in &resp.created {
        is_in!(gathered, id.to_string());
    }
    // and no reaction may be listed twice
    let unique = gathered.iter().collect::<HashSet<_>>();
    is!(gathered.len(), unique.len());
    Ok(())
}

/// Failing a reaction that is still queued removes it from the Created group list
///
/// `fail` moves a reaction out of the group set of the status it actually had, so a reaction
/// failed before any of its jobs were claimed leaves the Created group list and is listed as
/// Failed.
#[tokio::test]
async fn failing_queued_reaction_leaves_created_list() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a reaction whose jobs are never claimed
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    let id = resp.created[0];
    // fail the still queued reaction
    handle_reaction(&client, &group, &id, "fail").await?;
    // it must no longer be listed as Created
    let mut created = client
        .reactions
        .list_group(&group, &ReactionStatus::Created)
        .page_size(1000);
    created.next().await?;
    is_not_in!(created.names, id.to_string());
    // and it is listed as Failed
    let mut failed = client
        .reactions
        .list_group(&group, &ReactionStatus::Failed)
        .page_size(1000);
    failed.next().await?;
    is_in!(failed.names, id.to_string());
    Ok(())
}

/// Failing a queued sub reaction removes it from its parent's Created sub list
///
/// The sub reaction is moved out of its parent's sub status set for the status it actually had,
/// so a sub reaction failed while still Created leaves its parent's Created sub status list.
#[tokio::test]
async fn failing_queued_sub_reaction_leaves_parent_created_list() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a parent reaction with a single queued sub reaction
    let (_, parent) = generators::reactions(&group, 1, None, &client).await?;
    let parent = parent.created[0];
    let (_, subs, _) = generators::sub_reactions(&group, 1, &parent, &client).await?;
    // fail the still queued sub reaction
    handle_reaction(&client, &group, &subs[0].id, "fail").await?;
    // it must no longer be listed as a Created sub reaction
    let mut created = client
        .reactions
        .list_sub_status(&group, &parent, &ReactionStatus::Created);
    created.next().await?;
    is_not_in!(created.names, subs[0].id.to_string());
    Ok(())
}

/// Completing a queued reaction by proceeding it removes it from the Created group list
///
/// `complete` moves a reaction out of the group set of the status it actually had, so a single
/// stage reaction proceeded by its owner before any job was claimed is Completed and no longer
/// listed as Created.
#[tokio::test]
async fn proceeding_queued_reaction_leaves_created_list() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a single stage reaction whose jobs are never claimed
    let (_, resp) = generators::reactions(&group, 1, None, &client).await?;
    let id = resp.created[0];
    // proceed the still queued reaction past its only stage, completing it
    handle_reaction(&client, &group, &id, "proceed").await?;
    // it must no longer be listed as Created
    let mut created = client
        .reactions
        .list_group(&group, &ReactionStatus::Created)
        .page_size(1000);
    created.next().await?;
    is_not_in!(created.names, id.to_string());
    Ok(())
}

/// Concurrent proceeds of the same reaction don't both advance from the same stage
///
/// `proceed` only saves the advanced `current_stage` if the reaction is still on the stage it
/// proceeded from and hasn't been completed or failed, so two concurrent proceeds can't both
/// advance from stage 0 to stage 1 and both create (duplicate) the jobs for stage 1.
///
/// The race is probabilistic, so this races many reactions and checks that none of them has more
/// than one stage 1 job.
#[tokio::test]
#[ignore = "probabilistic race between concurrent reaction proceeds"]
async fn concurrent_user_proceed_does_not_duplicate_stage_jobs() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build a three stage pipeline with a single image per stage
    let images: Vec<String> = generators::images(&group, 3, false, &client)
        .await?
        .into_iter()
        .map(|image| image.name)
        .collect();
    let order = serde_json::json!([[&images[0]], [&images[1]], [&images[2]]]);
    let pipe_req = PipelineRequest::new(&group, generators::gen_string(25), order);
    client.pipelines.create(&pipe_req).await?;
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // create many reactions to widen the race window
    let mut ids = HashSet::new();
    for _ in 0..10 {
        let req = generators::gen_reaction(&group, &pipe, None);
        ids.insert(client.reactions.create(&req).await?.id);
    }
    // proceed each reaction twice concurrently
    for id in &ids {
        let _ = tokio::join!(
            handle_reaction(&client, &group, id, "proceed"),
            handle_reaction(&client, &group, id, "proceed")
        );
    }
    // claim every stage 1 job
    generators::worker(
        "cluster0", "node0", "c5_04", &group, &pipe.name, &images[1], &client,
    )
    .await?;
    let jobs = client
        .jobs
        .claim_workload(
            &group,
            generators::stage_instance(&pipe, &images[1]),
            "cluster0",
            "node0",
            "c5_04",
            100,
        )
        .await?;
    // no reaction may have more than one stage 1 job
    let mut per_reaction: HashMap<Uuid, usize> = HashMap::new();
    for job in &jobs {
        *per_reaction.entry(job.reaction).or_default() += 1;
    }
    for (id, count) in per_reaction {
        assert!(
            count <= 1,
            "reaction {id} had {count} stage 1 jobs; concurrent proceeds duplicated a stage"
        );
    }
    generators::delete_worker("c5_04", &client).await?;
    Ok(())
}

/// A stage-log page that crosses the boundary between two storage buckets of lines returns
/// every requested line, in order
#[tokio::test]
async fn stage_logs_across_buckets() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a reaction to save stage logs for
    let group = generators::groups(1, &client).await?.remove(0).name;
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let id = client.reactions.create(&react_req).await?.id;
    // save lines at positions 2475 through 2524, which straddle the 2500 line boundary
    let logs = StageLogsAdd {
        index: 0,
        logs: (2475..2525)
            .map(|index| StageLogLine {
                index,
                line: format!("line{index}"),
            })
            .collect(),
        return_code: None,
    };
    client
        .reactions
        .add_stage_logs(&group, &id, "harvest", &logs)
        .await?;
    // read pages that start before the boundary and end after it
    for (cursor, limit) in [(2475, 50), (2490, 20)] {
        let params = ReactionListParams { cursor, limit };
        let read = client
            .reactions
            .logs(&group, &id, "harvest", &params)
            .await?;
        let expected = (cursor..cursor + limit)
            .map(|index| format!("line{index}"))
            .collect::<Vec<String>>();
        is!(
            read.logs,
            expected,
            format!("cursor={cursor}&limit={limit}")
        );
    }
    Ok(())
}

/// The largest stage-log page read from a cursor off a bucket boundary spans more buckets than
/// one query may name, and still returns every requested line in order
#[tokio::test]
async fn stage_logs_largest_page_unaligned() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a reaction to save stage logs for
    let group = generators::groups(1, &client).await?.remove(0).name;
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    let react_req = generators::gen_reaction(&group, &pipe, None);
    let id = client.reactions.create(&react_req).await?.id;
    // save lines before the window, in its first and last buckets of 2500 lines, and in the
    // bucket after its last
    let positions: [usize; 7] = [0, 1, 2, 2499, 247_501, 250_000, 252_500];
    let logs = StageLogsAdd {
        index: 0,
        logs: positions
            .iter()
            .map(|index| StageLogLine {
                index: *index as u64,
                line: format!("line{index}"),
            })
            .collect(),
        return_code: None,
    };
    client
        .reactions
        .add_stage_logs(&group, &id, "harvest", &logs)
        .await?;
    // read the largest pages from cursors off a bucket boundary
    for (cursor, limit) in [(1, 250_000), (2499, 247_502)] {
        let params = ReactionListParams { cursor, limit };
        let read = client
            .reactions
            .logs(&group, &id, "harvest", &params)
            .await?;
        let expected = positions
            .iter()
            .filter(|index| (cursor..252_500).contains(*index))
            .map(|index| format!("line{index}"))
            .collect::<Vec<String>>();
        is!(
            read.logs,
            expected,
            format!("cursor={cursor}&limit={limit}")
        );
    }
    Ok(())
}

/// The developer role for k8s images only
fn k8s_developer() -> UserRole {
    UserRole::Developer {
        k8s: true,
        bare_metal: false,
        windows: false,
        kvm: false,
        external: false,
    }
}

/// Creates a two stage pipeline in a group whose stages each run a new K8s Image tool
///
/// # Arguments
///
/// * `admin` - The admin client to create the tools and pipeline with
/// * `group` - The group to create the pipeline in
async fn two_stage_pipeline(admin: &Thorium, group: &str) -> Result<Pipeline, Error> {
    // each stage runs its own K8s Image tool instance in its own step
    steps_pipe(group, 2, ImageScaler::K8s, admin).await
}

/// Register a worker that claims a user's jobs of the tool instance a pipeline stage runs
///
/// # Arguments
///
/// * `admin` - The admin client to register the worker with
/// * `user` - The client of the user whose jobs the worker claims
/// * `node` - The node the worker is on
/// * `name` - The name of the worker
/// * `pipe` - The pipeline whose stage the worker runs
/// * `stage` - The stage whose tool instance the worker claims jobs for
async fn creator_worker(
    admin: &Thorium,
    user: &Thorium,
    node: &str,
    name: &str,
    pipe: &Pipeline,
    stage: &str,
) -> Result<(), Error> {
    // jobs are queued per creator, so the worker runs as the user whose jobs it claims
    let username = user.users.info().await?.username;
    let list = thorium::models::WorkerRegistrationList::default().add(
        "cluster0",
        node,
        name,
        username,
        &pipe.group,
        generators::stage_instance(pipe, stage),
        None,
        Resources::default(),
        thorium::models::Pools::Deadline,
    );
    admin
        .system
        .register_workers(ImageScaler::K8s, &list)
        .await?;
    Ok(())
}

/// Builds options that replace an image's command, positionals, and kwargs
///
/// # Arguments
///
/// * `marker` - The text the replacement command echoes
fn override_opts(marker: &str) -> GenericJobOpts {
    // build the command to replace an image's command with
    let cmd = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!("echo {marker}"),
    ];
    GenericJobOpts::new(true, true, Some(cmd))
}

/// Builds an update that sets the options of one stage of a reaction
///
/// # Arguments
///
/// * `stage` - The stage (image) to set options for
/// * `opts` - The options to set
fn opts_update(stage: &str, opts: GenericJobOpts) -> ReactionUpdate {
    ReactionUpdate::default().arg(stage, GenericJobArgsUpdate::default().opts(opts))
}

/// Overriding a stage's command needs the Developer role for the scaler of the tool instance the
/// stage runs, on a create, a bulk create, a bulk create for another user, and an update
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn overrides_need_the_stage_tool_instance_scaler() -> Result<(), Error> {
    // a plain user, a K8s developer, and a BareMetal developer in one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let plain = generators::moon_user(&admin, "carme", &suffix).await?;
    let k8s = generators::named_user_with_role(
        &admin,
        &generators::readable_name("ananke", &suffix),
        k8s_developer(),
    )
    .await?;
    let bare_metal = generators::named_user_with_role(
        &admin,
        &generators::readable_name("sinope", &suffix),
        UserRole::Developer {
            k8s: false,
            bare_metal: true,
            windows: false,
            kvm: false,
            external: false,
        },
    )
    .await?;
    let roles = generators::GroupRoles::default()
        .user(&plain.username)
        .user(&k8s.username)
        .user(&bare_metal.username);
    let group = generators::god_group(&admin, "vulcan", &suffix, &roles).await?;
    // a pipeline whose one stage runs a BareMetal tool instance
    let pipe = steps_pipe(&group, 1, ImageScaler::BareMetal, &admin).await?;
    let stage = pipe.order[0][0].clone();
    let opts = override_opts("BARE-METAL");
    let overridden = ReactionRequest::new(&group, &pipe.name)
        .args(&stage, GenericJobArgs::default().opts(opts.clone()));
    // the K8s developer's create and bulk create with the override are refused
    fail!(k8s.client.reactions.create(&overridden).await, 401);
    let resp = k8s
        .client
        .reactions
        .create_bulk(std::slice::from_ref(&overridden))
        .await;
    fail!(resp, 401);
    // an admin's bulk create for the K8s developer reports their request as refused
    let reqs = HashMap::from([(k8s.username.clone(), vec![overridden.clone()])]);
    let by_user = admin.reactions.create_bulk_by_user(&reqs).await?;
    is_empty!(by_user[&k8s.username].created);
    contains_key!(by_user[&k8s.username].errors, &0);
    // the K8s developer can't add the override to the plain user's reaction
    let id = plain
        .client
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    let update = opts_update(&stage, opts.clone());
    fail!(k8s.client.reactions.update(&group, &id, &update).await, 401);
    // nothing the K8s developer tried was created or stored
    let mut cursor = admin.reactions.list(&group, &pipe.name);
    cursor.next().await?;
    is!(cursor.names, vec![id.to_string()]);
    let stored = admin.reactions.get(&group, id).await?;
    is!(
        stored.args.get(&stage).map(|args| args.opts.clone()),
        None::<GenericJobOpts>
    );
    // the BareMetal developer overrides on a create, a bulk create, and an update
    let created = bare_metal.client.reactions.create(&overridden).await?;
    let stored = admin.reactions.get(&group, created.id).await?;
    is!(stored.args[&stage].opts, opts);
    let resp = bare_metal
        .client
        .reactions
        .create_bulk(std::slice::from_ref(&overridden))
        .await?;
    is!(resp.created.len(), 1);
    let updated = bare_metal
        .client
        .reactions
        .update(&group, &id, &update)
        .await?;
    is!(updated.args[&stage].opts, opts);
    // an admin's bulk create for the BareMetal developer creates their request
    let reqs = HashMap::from([(bare_metal.username.clone(), vec![overridden])]);
    let by_user = admin.reactions.create_bulk_by_user(&reqs).await?;
    is_empty!(by_user[&bare_metal.username].errors);
    is!(by_user[&bare_metal.username].created.len(), 1);
    Ok(())
}

/// A group member without the Developer role can't override another member's reaction's
/// image command through an update, just as they can't on their own create
#[tokio::test]
async fn update_overrides_refused_for_member() -> Result<(), Error> {
    // get admin client (the group owner)
    let admin = test_utilities::admin_client().await?;
    // register a node for workers to claim jobs on
    let node = format!("override-node-{}", generators::gen_string(6));
    generators::node("cluster0", &node, Resources::default(), &admin).await?;
    // create the group
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add the creator and another member as plain users without a developer role
    let creator = group_member(&admin, &group, UserRole::User).await?;
    let member = group_member(&admin, &group, UserRole::User).await?;
    // create a two stage pipeline
    let pipe = two_stage_pipeline(&admin, &group).await?;
    let stage_two = pipe.order[1][0].clone();
    // the creator creates a reaction with no overrides
    let id = creator
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    // build options overriding the second stage's command
    let opts = override_opts("INJECTED");
    // the other member's own create with that override is refused
    let own = ReactionRequest::new(&group, &pipe.name)
        .args(&stage_two, GenericJobArgs::default().opts(opts.clone()));
    fail!(member.reactions.create(&own).await, 401);
    // the other member's update of the creator's reaction with the same override is refused
    let update = opts_update(&stage_two, opts);
    fail!(member.reactions.update(&group, &id, &update).await, 401);
    // make sure the stored reaction has no override for the second stage
    let stored = admin.reactions.get(&group, id).await?;
    is!(
        stored.args.get(&stage_two).map(|args| args.opts.clone()),
        None::<GenericJobOpts>
    );
    // run the first stage as an agent would so the second stage's job is created, with a
    // worker that claims the creator's jobs
    let worker_one = format!("override-w1-{}", generators::gen_string(6));
    creator_worker(
        &admin,
        &creator,
        &node,
        &worker_one,
        &pipe,
        &pipe.order[0][0],
    )
    .await?;
    // jobs are queued per creator, so claim them with the creator's credentials
    let tool_one = generators::stage_instance(&pipe, &pipe.order[0][0]);
    let jobs = creator
        .jobs
        .claim_workload(&group, tool_one, "cluster0", &node, &worker_one, 1)
        .await?;
    creator
        .jobs
        .proceed(&jobs[0], &generators::stage_logs(), 2, &worker_one)
        .await?;
    generators::delete_worker(&worker_one, &admin).await?;
    // claim the second stage's job with another worker for the creator's jobs
    let worker_two = format!("override-w2-{}", generators::gen_string(6));
    creator_worker(&admin, &creator, &node, &worker_two, &pipe, &stage_two).await?;
    let tool_two = generators::stage_instance(&pipe, &stage_two);
    let jobs = creator
        .jobs
        .claim_workload(&group, tool_two, "cluster0", &node, &worker_two, 1)
        .await?;
    generators::delete_worker(&worker_two, &admin).await?;
    // make sure the second stage's job runs the image's own command
    is!(jobs.len(), 1);
    is!(jobs[0].args.opts.override_cmd, None::<Vec<String>>);
    Ok(())
}

/// The creator of a reaction can't override its image command through an update without
/// the Developer role
#[tokio::test]
async fn update_overrides_refused_for_creator() -> Result<(), Error> {
    // get admin client (the group owner)
    let admin = test_utilities::admin_client().await?;
    // create the group
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add the creator as a plain user without a developer role
    let creator = group_member(&admin, &group, UserRole::User).await?;
    // create a two stage pipeline
    let pipe = two_stage_pipeline(&admin, &group).await?;
    let stage_two = pipe.order[1][0].clone();
    // the creator creates a reaction with no overrides
    let id = creator
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    // the creator's update overriding only the command, only the positionals, or only the
    // kwargs is refused
    let cmd_only = GenericJobOpts::new(false, false, Some(vec!["/bin/true".to_owned()]));
    for opts in [
        cmd_only,
        GenericJobOpts::new(true, false, None),
        GenericJobOpts::new(false, true, None),
    ] {
        let update = opts_update(&stage_two, opts);
        fail!(creator.reactions.update(&group, &id, &update).await, 401);
    }
    // the creator's update overriding the command, positionals, and kwargs is refused
    let update = opts_update(&stage_two, override_opts("INJECTED"));
    fail!(creator.reactions.update(&group, &id, &update).await, 401);
    // make sure the stored reaction has no override for the second stage
    let stored = admin.reactions.get(&group, id).await?;
    is!(
        stored.args.get(&stage_two).map(|args| args.opts.clone()),
        None::<GenericJobOpts>
    );
    Ok(())
}

/// A creator with the Developer role for the image's scaler, another member with that role,
/// and an admin can all override a reaction's image command through an update
#[tokio::test]
async fn update_overrides_allowed_for_developer_and_admin() -> Result<(), Error> {
    // get admin client (the group owner)
    let admin = test_utilities::admin_client().await?;
    // create the group
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add a plain user and two k8s developers
    let user = group_member(&admin, &group, UserRole::User).await?;
    let developer = group_member(&admin, &group, k8s_developer()).await?;
    let other_developer = group_member(&admin, &group, k8s_developer()).await?;
    // create a two stage pipeline
    let pipe = two_stage_pipeline(&admin, &group).await?;
    let stage_two = pipe.order[1][0].clone();
    // the developer creates a reaction with no overrides
    let dev_id = developer
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    // the developer overrides their own reaction's second stage
    let opts = override_opts("DEVELOPER");
    let updated = developer
        .reactions
        .update(&group, &dev_id, &opts_update(&stage_two, opts.clone()))
        .await?;
    is!(updated.args[&stage_two].opts, opts);
    // another developer overrides the same stage with a different command
    let opts = override_opts("OTHER-DEVELOPER");
    let updated = other_developer
        .reactions
        .update(&group, &dev_id, &opts_update(&stage_two, opts.clone()))
        .await?;
    is!(updated.args[&stage_two].opts, opts);
    // the plain user creates a reaction with no overrides
    let user_id = user
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    // the admin overrides the plain user's reaction's second stage
    let opts = override_opts("ADMIN");
    let updated = admin
        .reactions
        .update(&group, &user_id, &opts_update(&stage_two, opts.clone()))
        .await?;
    is!(updated.args[&stage_two].opts, opts);
    // make sure both overrides were stored
    let stored = admin.reactions.get(&group, dev_id).await?;
    is!(
        stored.args[&stage_two].opts,
        override_opts("OTHER-DEVELOPER")
    );
    let stored = admin.reactions.get(&group, user_id).await?;
    is!(stored.args[&stage_two].opts, opts);
    Ok(())
}

/// Builds an update that replaces or adds one ephemeral file
///
/// # Arguments
///
/// * `name` - The name of the ephemeral file
/// * `content` - The bytes to write to the file
fn ephemeral_update(name: &str, content: &str) -> ReactionUpdate {
    // ReactionUpdate has no ephemeral builder so populate its map directly with base64 bytes
    let mut update = ReactionUpdate::default();
    update
        .ephemeral
        .insert(name.to_owned(), BASE64.encode(content.as_bytes()));
    update
}

/// Replacing an existing ephemeral file of a reaction with an override stage needs the Developer
/// role for that stage's scaler, so an editor can't swap the script an override runs, while adding
/// a new file and replacing a file of a reaction with no override stay open to editors
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn update_ephemeral_replacement_needs_developer_with_override() -> Result<(), Error> {
    // the script a developer's override runs, and the script an editor tries to swap in
    const ORIGINAL: &str = "echo original";
    const INJECTED: &str = "echo injected";
    // get admin client (the group owner)
    let admin = test_utilities::admin_client().await?;
    // create the group
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add a plain editor and two k8s developers
    let editor = group_member(&admin, &group, UserRole::User).await?;
    let developer = group_member(&admin, &group, k8s_developer()).await?;
    let other_developer = group_member(&admin, &group, k8s_developer()).await?;
    // create a two stage pipeline
    let pipe = two_stage_pipeline(&admin, &group).await?;
    let stage_two = pipe.order[1][0].clone();
    // the developer creates a reaction whose second stage runs the ephemeral script
    let opts = GenericJobOpts::new(
        true,
        true,
        Some(vec![
            "/bin/sh".to_owned(),
            "/tmp/thorium/ephemeral/s.sh".to_owned(),
        ]),
    );
    let req = ReactionRequest::new(&group, &pipe.name)
        .args(&stage_two, GenericJobArgs::default().opts(opts))
        .buffer("s.sh", ORIGINAL);
    let id = developer.reactions.create(&req).await?.id;
    // the editor can't replace the script and the stored script is unchanged
    let swap = ephemeral_update("s.sh", INJECTED);
    fail!(editor.reactions.update(&group, &id, &swap).await, 401);
    let stored = admin
        .reactions
        .download_ephemeral(&group, &id, "s.sh")
        .await?;
    is!(stored, ORIGINAL);
    // the editor can still add a new file to the reaction
    let added = editor
        .reactions
        .update(&group, &id, &ephemeral_update("extra.txt", "extra"))
        .await?;
    contains!(added.ephemeral, &"extra.txt".to_owned());
    // another developer can replace the script
    other_developer
        .reactions
        .update(&group, &id, &ephemeral_update("s.sh", "echo second"))
        .await?;
    let stored = admin
        .reactions
        .download_ephemeral(&group, &id, "s.sh")
        .await?;
    is!(stored, "echo second");
    // the admin can replace the script
    admin
        .reactions
        .update(&group, &id, &ephemeral_update("s.sh", "echo admin"))
        .await?;
    let stored = admin
        .reactions
        .download_ephemeral(&group, &id, "s.sh")
        .await?;
    is!(stored, "echo admin");
    // a reaction with no override stage keeps its files open to any editor
    let plain = ReactionRequest::new(&group, &pipe.name).buffer("data.txt", "one");
    let plain_id = developer.reactions.create(&plain).await?.id;
    editor
        .reactions
        .update(&group, &plain_id, &ephemeral_update("data.txt", "two"))
        .await?;
    let stored = admin
        .reactions
        .download_ephemeral(&group, &plain_id, "data.txt")
        .await?;
    is!(stored, "two");
    Ok(())
}

/// A group member without the Developer role can still update another member's reaction
/// when the update doesn't change any stage's options
#[tokio::test]
async fn update_without_opts_change_by_editor() -> Result<(), Error> {
    // get admin client (the group owner)
    let admin = test_utilities::admin_client().await?;
    // create the group
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add a k8s developer as the creator and a plain user as the editor
    let developer = group_member(&admin, &group, k8s_developer()).await?;
    let editor = group_member(&admin, &group, UserRole::User).await?;
    // create a two stage pipeline
    let pipe = two_stage_pipeline(&admin, &group).await?;
    let stage_two = pipe.order[1][0].clone();
    // the developer creates a reaction overriding the second stage
    let opts = override_opts("DEVELOPER");
    let req = ReactionRequest::new(&group, &pipe.name)
        .args(&stage_two, GenericJobArgs::default().opts(opts.clone()));
    let id = developer.reactions.create(&req).await?.id;
    // the editor updates tags, positionals, kwargs, and switches without setting options
    let update = ReactionUpdate::default().tag("edited").arg(
        &stage_two,
        GenericJobArgsUpdate::default()
            .positionals(vec!["1", "2"])
            .kwarg("--new", vec!["arg"])
            .switch("--fast"),
    );
    let updated = editor.reactions.update(&group, &id, &update).await?;
    is!(updated, update);
    // make sure the developer's override is untouched
    is!(updated.args[&stage_two].opts, opts);
    // the editor sends the stage's current options unchanged alongside a new tag
    let update = opts_update(&stage_two, opts.clone()).tag("same-opts");
    let updated = editor.reactions.update(&group, &id, &update).await?;
    is!(updated.args[&stage_two].opts, opts);
    is_in!(updated.tags, "same-opts".to_owned());
    // the editor's update changing the override is refused
    let update = opts_update(&stage_two, override_opts("EDITOR"));
    fail!(editor.reactions.update(&group, &id, &update).await, 401);
    // make sure the stored reaction keeps the developer's override
    let stored = admin.reactions.get(&group, id).await?;
    is!(stored.args[&stage_two].opts, opts);
    Ok(())
}

/// Sends a proceed or fail command for a reaction with a user's basic credentials
///
/// Returns the response's status code, or 0 when the request couldn't be sent.
///
/// # Arguments
///
/// * `host` - The Thorium API host
/// * `username` - The user sending the command
/// * `password` - The user's password
/// * `group` - The reaction's group
/// * `id` - The reaction's id
/// * `cmd` - `proceed` or `fail`
async fn handle_status(
    host: &str,
    username: &str,
    password: &str,
    group: &str,
    id: &Uuid,
    cmd: &str,
) -> u16 {
    // build the handle url
    let url = format!("{host}/api/reactions/handle/{group}/{id}/{cmd}");
    // send the command with the user's basic credentials
    reqwest::Client::new()
        .post(&url)
        .basic_auth(username, Some(password))
        .send()
        .await
        .map_or(0, |resp| resp.status().as_u16())
}

/// A group viewer (monitor) can see a reaction but is refused an update, proceed, and fail
#[tokio::test]
async fn update_refused_for_viewer() -> Result<(), Error> {
    // get admin client (the group owner)
    let admin = test_utilities::admin_client().await?;
    // create the group
    let group = generators::groups(1, &admin).await?.remove(0).name;
    // add the creator as a plain user and the viewer as a monitor
    let creator = group_member(&admin, &group, UserRole::User).await?;
    let (viewer, name, password) = group_member_ext(&admin, &group, UserRole::User, true).await?;
    // create a two stage pipeline
    let pipe = two_stage_pipeline(&admin, &group).await?;
    let stage_two = pipe.order[1][0].clone();
    // the creator creates a reaction with no overrides
    let id = creator
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    // the viewer can see the reaction
    viewer.reactions.get(&group, id).await?;
    // the viewer's update overriding the second stage's command is refused
    let opts = GenericJobOpts::new(false, false, Some(vec!["/bin/true".to_owned()]));
    let update = opts_update(&stage_two, opts);
    fail!(viewer.reactions.update(&group, &id, &update).await, 401);
    // the viewer's update with no options is refused
    let update = ReactionUpdate::default().tag("viewer");
    fail!(viewer.reactions.update(&group, &id, &update).await, 401);
    // the viewer's proceed and fail are refused
    is!(
        handle_status(&admin.host, &name, &password, &group, &id, "proceed").await,
        401
    );
    is!(
        handle_status(&admin.host, &name, &password, &group, &id, "fail").await,
        401
    );
    // make sure the reaction is unchanged
    let stored = admin.reactions.get(&group, id).await?;
    is!(stored.status, ReactionStatus::Created);
    is!(stored.current_stage, 0);
    is!(
        stored.args.get(&stage_two).map(|args| args.opts.clone()),
        None::<GenericJobOpts>
    );
    is_not_in!(stored.tags, "viewer".to_owned());
    Ok(())
}
