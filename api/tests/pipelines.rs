//! Tests the Pipelines routes in Thorium

use rand::{rng, seq::SliceRandom};
use thorium::models::{
    AgentEntityRequest, EntityUpdate, ImageBan, ImageBanKind, ImageBanUpdate, ImageScaler,
    ImageUpdate, ModelEndpointEntityRequest, NotificationLevel, NotificationParams,
    NotificationRequest, PipelineBan, PipelineBanKind, PipelineBanUpdate, PipelineRequest,
    PipelineUpdate, ReactionRequest, ToolSetEntityRequest, UserRole,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, contains, fail, is, is_in, unwrap_variant, vec_in_vec};
use uuid::Uuid;

/// Create a pipeline request whose one stage is backed by a tool id
///
/// # Arguments
///
/// * `group` - The group of the pipeline
/// * `stage` - The name of the pipeline's only stage
/// * `tool` - The Tool or Tool Instance id backing the stage
fn one_stage(group: &str, stage: &str, tool: Uuid) -> PipelineRequest {
    // a single stage pipeline with a random name
    PipelineRequest::new(
        group,
        generators::gen_string(20).to_lowercase(),
        serde_json::json!([[stage]]),
    )
    .stage_tool(stage, tool)
}

/// Pipeline stages are backed by tools in the pipeline's group, and the wrong kind of entity,
/// one outside the group, an invisible one, or a missing one is refused naming the stage
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn stage_tool_validation() -> Result<(), Error> {
    // a group with an owner who develops for K8s only
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let owner = generators::named_user_with_role(
        &admin,
        &generators::readable_name("callisto", &suffix),
        UserRole::Developer {
            k8s: true,
            bare_metal: false,
            windows: false,
            kvm: false,
            external: false,
        },
    )
    .await?;
    let roles = generators::GroupRoles::default().owner(&owner.username);
    let group = generators::god_group(&admin, "mars", &suffix, &roles).await?;
    let other =
        generators::god_group(&admin, "venus", &suffix, &generators::GroupRoles::default()).await?;
    // a Tool, an Image Tool Instance, and a Harness Tool Instance in the group are each accepted
    let (image_tool, image_instance) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (_, harness_instance) =
        generators::tool(&[&group], "1.0.0", &generators::gen_harness_spec(), &admin).await?;
    for (stage, id) in [
        ("tool-root", image_tool.id),
        ("image-instance", image_instance.id),
        ("harness-instance", harness_instance.id),
    ] {
        let req = one_stage(&group, stage, id);
        owner.client.pipelines.create(&req).await?;
        // the stage name names no image and the stage tool is stored
        let read = owner.client.pipelines.get(&group, &req.name).await?;
        is!(read.stage_tools.get(stage), Some(&id));
        fail!(admin.images.get(&group, stage).await, 404);
    }
    // a toolset, an agent, and a model entity in the group are the wrong kind
    let toolset = admin
        .entities
        .create(generators::gen_toolset(
            "hoard",
            &[&group],
            ToolSetEntityRequest::default(),
        ))
        .await?
        .id;
    let agent = admin
        .entities
        .create(generators::gen_agent(
            "scout",
            &[&group],
            AgentEntityRequest::new(generators::gen_agent_config("scout")),
        ))
        .await?
        .id;
    let endpoint = generators::model_endpoint(
        &[&group],
        ModelEndpointEntityRequest::new(generators::gen_anthropic_provider()),
        &admin,
    )
    .await?
    .id;
    // a tool instance in another group the owner can't see and one the owner can see
    let (_, hidden) =
        generators::tool(&[&other], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let visible_group = generators::god_group(
        &admin,
        "ceres",
        &suffix,
        &generators::GroupRoles::default().user(&owner.username),
    )
    .await?;
    let (_, outside) = generators::tool(
        &[&visible_group],
        "1.0.0",
        &generators::gen_image_spec(),
        &admin,
    )
    .await?;
    // wrong kinds and tools outside the group are bad requests, missing or hidden ids not found
    for (stage, id, code) in [
        ("toolset-stage", toolset, 400_u16),
        ("agent-stage", agent, 400),
        ("model-stage", endpoint, 400),
        ("outside-stage", outside.id, 400),
        ("hidden-stage", hidden.id, 404),
        ("missing-stage", Uuid::new_v4(), 404),
    ] {
        let req = one_stage(&group, stage, id);
        let resp = owner.client.pipelines.create(&req).await;
        fail!(resp, code, stage);
        // nothing was stored
        fail!(owner.client.pipelines.get(&group, &req.name).await, 404);
    }
    // a stage tool whose workload scaler the owner doesn't develop for is unauthorized
    let (_, bare_metal) = generators::tool(
        &[&group],
        "1.0.0",
        &generators::gen_image_spec_for(ImageScaler::BareMetal),
        &admin,
    )
    .await?;
    let resp = owner
        .client
        .pipelines
        .create(&one_stage(&group, "bare-stage", bare_metal.id))
        .await;
    fail!(resp, 401);
    // removing the tool of a stage that stays in the order is refused
    let kept = one_stage(&group, "kept", image_instance.id);
    owner.client.pipelines.create(&kept).await?;
    let update = PipelineUpdate::default().remove_stage_tool("kept");
    let resp = owner
        .client
        .pipelines
        .update(&group, &kept.name, &update)
        .await;
    fail!(resp, 400, "kept");
    // removing the pipeline's group from a tool a stage names is a conflict naming the pipeline
    let (tool_b, instance_b) = generators::tool(
        &[&group, &visible_group],
        "1.0.0",
        &generators::gen_image_spec(),
        &admin,
    )
    .await?;
    let referrer = one_stage(&group, "named", tool_b.id);
    owner.client.pipelines.create(&referrer).await?;
    let removal = admin
        .entities
        .update(tool_b.id, EntityUpdate::default().remove_group(&group))
        .await;
    fail!(removal, 409, referrer.name.as_str());
    // once the stage names another tool the removal succeeds
    let update = PipelineUpdate::default().add_stage_tool("named", image_instance.id);
    owner
        .client
        .pipelines
        .update(&group, &referrer.name, &update)
        .await?;
    admin
        .entities
        .update(tool_b.id, EntityUpdate::default().remove_group(&group))
        .await?;
    let read = admin.entities.get(instance_b.id).await?;
    is!(read.groups.contains(&group), false);
    Ok(())
}

/// Stage tools are checked on create and update, and a reaction runs each stage's tool instance
#[tokio::test]
async fn pipeline_stage_tools_create_update() -> Result<(), Error> {
    // a group and a developer who owns it
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let other = generators::groups(1, &admin).await?.remove(0).name;
    // a Tool followed to its latest instance and a pinned Tool Instance
    let (latest_tool, latest_instance) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (_, pinned) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (outside_tool, _) =
        generators::tool(&[&other], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    // stage names that aren't images are accepted when every stage has a tool
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&group, &name, serde_json::json!([["follow"], ["pinned"]]))
        .stage_tool("follow", latest_tool.id)
        .stage_tool("pinned", pinned.id);
    admin.pipelines.create(&req).await?;
    // a tool outside the pipeline's group and an unknown id are refused naming the stage
    let outside = PipelineRequest::new(&group, "outside", serde_json::json!([["far"]]))
        .stage_tool("far", outside_tool.id);
    fail!(admin.pipelines.create(&outside).await, 400, "far");
    let unknown = PipelineRequest::new(&group, "unknown", serde_json::json!([["gone"]]))
        .stage_tool("gone", Uuid::new_v4());
    fail!(admin.pipelines.create(&unknown).await, 404, "gone");
    // the same checks apply when an update adds stages
    let update = PipelineUpdate::default()
        .order(vec![vec!["follow".to_owned()], vec!["far".to_owned()]])
        .add_stage_tool("far", outside_tool.id);
    fail!(
        admin.pipelines.update(&group, &name, &update).await,
        400,
        "far"
    );
    let update = PipelineUpdate::default()
        .order(vec![vec!["follow".to_owned()], vec!["new".to_owned()]])
        .remove_stage_tool("pinned");
    fail!(
        admin.pipelines.update(&group, &name, &update).await,
        400,
        "new"
    );
    // a reaction runs the tool's latest instance and the pinned instance
    let pipe = admin.pipelines.get(&group, &name).await?;
    let created = admin
        .reactions
        .create(&ReactionRequest::new(&group, &name))
        .await?;
    let reaction = admin.reactions.get(&group, created.id).await?;
    is!(
        reaction.plan.tool_instance("follow"),
        Some(latest_instance.id)
    );
    is!(reaction.plan.tool_instance("pinned"), Some(pinned.id));
    is!(pipe.stage_tools.len(), 2);
    Ok(())
}

/// Pipelines that name a tool or its instances block removing their group from the tool
#[tokio::test]
async fn tool_group_removal_blocked_by_pipelines() -> Result<(), Error> {
    // a tool in two groups and a user who can only see one of them
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let viewer = generators::moon_user(&admin, "io", &suffix).await?;
    let a =
        generators::god_group(&admin, "juno", &suffix, &generators::GroupRoles::default()).await?;
    let roles = generators::GroupRoles::default().owner(&viewer.username);
    let b = generators::god_group(&admin, "minerva", &suffix, &roles).await?;
    let (tool, instance) =
        generators::tool(&[&a, &b], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    // a pipeline in b naming the tool blocks removing b
    let by_tool = one_stage(&b, "root", tool.id);
    admin.pipelines.create(&by_tool).await?;
    let removal = admin
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&b))
        .await;
    fail!(removal, 409, by_tool.name.as_str());
    // a pipeline in b naming one of the tool's instances blocks it the same way
    let by_instance = one_stage(&b, "pinned", instance.id);
    admin.pipelines.create(&by_instance).await?;
    let removal = admin
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&b))
        .await;
    fail!(removal, 409, by_instance.name.as_str());
    // a pipeline in a doesn't block removing b
    let in_a = one_stage(&a, "elsewhere", tool.id);
    admin.pipelines.create(&in_a).await?;
    // a hidden referrer in b is neither named nor counted for a caller who can't see it
    let hidden_group = generators::god_group(
        &admin,
        "vulcan",
        &suffix,
        &generators::GroupRoles::default(),
    )
    .await?;
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(&hidden_group))
        .await?;
    let hidden = one_stage(&hidden_group, "hidden", tool.id);
    admin.pipelines.create(&hidden).await?;
    let removal = admin
        .entities
        .update(
            tool.id,
            EntityUpdate::default()
                .remove_group(&b)
                .remove_group(&hidden_group),
        )
        .await;
    let err = removal.expect_err("the removal is blocked");
    let msg = err.msg().unwrap_or_default();
    is!(msg.contains(&by_tool.name), true);
    is!(msg.contains(&in_a.name), false);
    // once the blocking pipelines are deleted the removal succeeds
    admin.pipelines.delete(&b, &by_tool.name).await?;
    admin.pipelines.delete(&b, &by_instance.name).await?;
    admin
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&b))
        .await?;
    // every tool instance lost b
    let read = admin.entities.get(instance.id).await?;
    is!(read.groups.contains(&b), false);
    Ok(())
}

/// Every stage needs a stage tool on create and keeps it on update, and a pipeline written by
/// an earlier version can gain its stage tools
#[tokio::test]
async fn stage_tools_required() -> Result<(), Error> {
    // a group and an Image tool instance in it
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let instance = generators::tool_instance(&[&group], &admin).await?;
    // a create with a stage that has no stage tool is refused naming it
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&group, &name, serde_json::json!([["plant"], ["harvest"]]))
        .stage_tool("plant", instance);
    fail!(admin.pipelines.create(&req).await, 400, "harvest");
    // an update that drops a kept stage's tool is refused naming it
    let req = req.stage_tool("harvest", instance);
    admin.pipelines.create(&req).await?;
    let update = PipelineUpdate::default().remove_stage_tool("harvest");
    fail!(
        admin.pipelines.update(&group, &name, &update).await,
        400,
        "harvest"
    );
    // a pipeline stored without stage tools gains them through an update
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool.get().await.expect("a redis connection");
    let key = format!(
        "{}:pipeline_data:{group}:{name}",
        test_utilities::CONF.thorium.namespace
    );
    let _: () = redis::cmd("hdel")
        .arg(&key)
        .arg("stage_tools")
        .query_async(&mut *conn)
        .await
        .expect("drop the stage tools");
    let read = admin.pipelines.get(&group, &name).await?;
    is!(read.stage_tools.is_empty(), true);
    // a reaction on it is refused naming its unconverted stages
    let resp = admin
        .reactions
        .create(&ReactionRequest::new(&group, &name))
        .await;
    fail!(resp, 400, "plant");
    // adding the stage tools converts it
    let update = PipelineUpdate::default()
        .add_stage_tool("plant", instance)
        .add_stage_tool("harvest", instance);
    admin.pipelines.update(&group, &name, &update).await?;
    admin
        .reactions
        .create(&ReactionRequest::new(&group, &name))
        .await?;
    Ok(())
}

#[tokio::test]
async fn create() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    let resp = client.pipelines.create(&pipe_req).await?;
    is!(resp.status().as_u16(), 204);
    Ok(())
}

#[tokio::test]
async fn create_image_no_exist() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a pipeline with a stage that no tool backs
    let pipeline_name = "pipeline-no-exist-image";
    let pipe_req = PipelineRequest::new(
        &group,
        pipeline_name,
        serde_json::json!(vec![vec!["image-no-exist"]]),
    );
    // the stage without a tool is refused
    let resp = client.pipelines.create(&pipe_req).await;
    fail!(resp, 400, "image-no-exist");
    Ok(())
}

#[tokio::test]
async fn create_conflict() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request with no description
    let mut pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    pipe_req.description = None;
    // Create a test pipeline
    let resp = client.pipelines.create(&pipe_req).await?;
    is!(resp.status().as_u16(), 204);
    pipe_req = pipe_req.description("This description should not be set on the existing pipeline");
    let resp = client.pipelines.create(&pipe_req).await;
    // check for a 409 conflict
    fail!(resp, 409);
    // check that the description for the existing pipeline was not set
    let pipeline = client
        .pipelines
        .get(&pipe_req.group, &pipe_req.name)
        .await?;
    is!(pipeline.description, None::<String>);
    Ok(())
}

#[tokio::test]
async fn create_banned_image() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups and seed a stored image in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    test_utilities::faults::allow_image_writes(&group);
    // create an image
    let image_req = generators::gen_image(&group);
    client.images.create(&image_req).await?;
    // add bans to the underlying image
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
    // a stage named after the banned image runs its tool, not the image
    let pipeline_name = "pipeline-ban-new";
    let instance = generators::tool_instance(&[&group], &client).await?;
    let pipe_req = PipelineRequest::new(
        &group,
        pipeline_name,
        serde_json::json!(vec![vec![&image_req.name]]),
    )
    .stage_tool(&image_req.name, instance);
    // the image's bans don't apply to the stage
    client.pipelines.create(&pipe_req).await?;
    Ok(())
}

#[tokio::test]
async fn get() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create an pipeline and then get it
    let pipeline = generators::gen_pipe(&group, 20, false, &client).await?;
    let resp = client.pipelines.create(&pipeline).await?;
    is!(resp.status().as_u16(), 204);
    // get the pipeline and compare it
    let retrieved = client.pipelines.get(&group, &pipeline.name).await?;
    is!(retrieved, pipeline);
    Ok(())
}

#[tokio::test]
async fn update() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // create an update request with a shuffled order
    let mut order: Vec<Vec<String>> = serde_json::from_value(pipe_req.order)?;
    order[0].shuffle(&mut rng());
    let pipe_update = PipelineUpdate::default()
        .order(order)
        .sla(86401)
        .description("Updated description")
        .bans(
            PipelineBanUpdate::default()
                .add_ban(PipelineBan::new(PipelineBanKind::generic("Test ban 1!")))
                .add_ban(PipelineBan::new(PipelineBanKind::generic("Test ban 2!"))),
        );
    // update the pipeline
    client
        .pipelines
        .update(&group, &pipe_req.name, &pipe_update)
        .await?;
    // check if the pipeline matches the update
    let retrieved = client.pipelines.get(&group, &pipe_req.name).await?;
    is!(retrieved, pipe_update);
    // update with clear variables
    let pipe_update = PipelineUpdate::default().clear_description();
    client
        .pipelines
        .update(&group, &pipe_req.name, &pipe_update)
        .await?;
    // check if the pipeline matches the update
    let retrieved = client.pipelines.get(&group, &pipe_req.name).await?;
    is!(retrieved, pipe_update);
    Ok(())
}

#[tokio::test]
async fn delete() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // delete the pipeline
    client.pipelines.delete(&group, &pipe_req.name).await?;
    // make sure the pipeline was deleted
    let resp = client.pipelines.get(&group, &pipe_req.name).await;
    fail!(resp, 404);
    // TODO: test that all reactions were deleted?
    // TODO: test that all notifications were deleted; can't do that without specific
    // route to get notifications because we get a 404 when we try to get notifications
    // after deletion
    Ok(())
}

#[tokio::test]
async fn add_banned_image() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create the pipeline tests groups and seed a stored image in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    test_utilities::faults::allow_image_writes(&group);
    // create an image
    let image_req = generators::gen_image(&group);
    client.images.create(&image_req).await?;
    // add bans to the underlying image
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
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // add a stage named after the banned image to the pipeline's order without a tool
    let order: Vec<Vec<String>> = serde_json::from_value(pipe_req.order.clone())?;
    let mut new_order = order.clone();
    new_order[0].push(image_req.name.clone());
    let pipe_update = PipelineUpdate::default().order(new_order.clone());
    // a new stage without a tool is refused
    let resp = client
        .pipelines
        .update(&group, &pipe_req.name, &pipe_update)
        .await;
    fail!(resp, 400, image_req.name.as_str());
    // with a tool the stage runs the tool, so the image's bans don't apply
    let instance = generators::tool_instance(&[&group], &client).await?;
    let pipe_update = PipelineUpdate::default()
        .order(new_order)
        .add_stage_tool(&image_req.name, instance);
    client
        .pipelines
        .update(&group, &pipe_req.name, &pipe_update)
        .await?;
    // the same holds for a stage in its own step
    let mut new_order = order.clone();
    new_order.push(vec![image_req.name.clone()]);
    let pipe_update = PipelineUpdate::default().order(new_order);
    client
        .pipelines
        .update(&group, &pipe_req.name, &pipe_update)
        .await?;
    Ok(())
}

#[tokio::test]
async fn list() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random pipelines
    let pipelines = generators::pipelines(&group, 20, false, &client).await?;
    // get the names of all the pipelines we have created
    let names: Vec<String> = pipelines.iter().map(|pipe| pipe.name.clone()).collect();
    // list the reactions we just created
    let mut cursor = client.pipelines.list(&group);
    cursor.next().await?;
    // make sure all the images we tried to create are in our list
    for pipe in names {
        is_in!(cursor.names, pipe);
    }
    Ok(())
}

#[tokio::test]
async fn list_details() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // setup 20 random pipelines
    let pipelines = generators::pipelines(&group, 20, false, &client).await?;
    // list the pipelines we just created
    let mut cursor = client.pipelines.list(&group).details();
    cursor.next().await?;
    // make sure all the images we tried to create are in our list
    vec_in_vec!(&cursor.details, &pipelines);
    Ok(())
}

#[tokio::test]
async fn update_bans() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // get the pipeline
    let pipeline = client.pipelines.get(&group, &pipe_req.name).await?;
    // update the pipeline with bans
    let mut bans = vec![
        PipelineBan::new(PipelineBanKind::generic("Test ban 1!")),
        PipelineBan::new(PipelineBanKind::image_ban(&pipeline.order[0][0])),
    ];
    let update =
        PipelineUpdate::default().bans(PipelineBanUpdate::default().add_bans(bans.clone()));
    client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await?;
    // successfully remove a ban from the image
    let update = PipelineUpdate::default().bans(
        PipelineBanUpdate::default().remove_ban(
            bans.pop()
                .ok_or_else(|| Error::new("Popped empty bans vec"))?
                .id,
        ),
    );
    client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await?;
    // get the pipeline and make sure the ban was removed
    let updated = client.pipelines.get(&group, &pipe_req.name).await?;
    is!(updated, update);
    // remove the second ban
    let update = PipelineUpdate::default().bans(
        PipelineBanUpdate::default().remove_ban(
            bans.pop()
                .ok_or_else(|| Error::new("Popped empty bans vec"))?
                .id,
        ),
    );
    client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await?;
    // get the pipeline and make sure the ban was removed
    let updated = client.pipelines.get(&group, &pipeline.name).await?;
    is!(updated, update);
    Ok(())
}

#[tokio::test]
async fn update_bans_bad() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get user client
    let user_client = generators::client(&client).await?;
    // Create a group
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &user_client).await?;
    // Create a test pipeline
    user_client.pipelines.create(&pipe_req).await?;
    // get the pipeline
    let pipeline = user_client.pipelines.get(&group, &pipe_req.name).await?;
    // update the pipeline with bans
    let mut bans = vec![
        PipelineBan::new(PipelineBanKind::generic("Test ban 1!")),
        PipelineBan::new(PipelineBanKind::image_ban(&pipeline.order[0][0])),
    ];
    let update =
        PipelineUpdate::default().bans(PipelineBanUpdate::default().add_bans(bans.clone()));
    client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await?;
    // attempt to add a ban as a non admin
    let resp = user_client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await;
    fail!(resp, 401);
    // attempt to remove a ban that doesn't exist
    let update =
        PipelineUpdate::default().bans(PipelineBanUpdate::default().remove_ban(Uuid::new_v4()));
    let resp = client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await;
    fail!(resp, 404);
    // attempt to add a ban that already exists
    let update =
        PipelineUpdate::default().bans(PipelineBanUpdate::default().add_ban(bans.pop().unwrap()));
    let resp = client
        .pipelines
        .update(&group, &pipeline.name, &update)
        .await;
    fail!(resp, 400);
    Ok(())
}

#[tokio::test]
async fn ban_from_image() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get a user client
    let user_client = generators::client(&client).await?;
    // Create a group and seed a stored image in it
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    test_utilities::faults::allow_image_writes(&group);
    // create an image
    let image_req = generators::gen_image(&group);
    client.images.create(&image_req).await?;
    // create a pipeline with a stage named after that image
    let pipeline_name = "pipeline-with-ban";
    let instance = generators::tool_instance(&[&group], &client).await?;
    let pipe_req = PipelineRequest::new(
        &group,
        pipeline_name,
        serde_json::json!(vec![vec![&image_req.name]]),
    )
    .stage_tool(&image_req.name, instance);
    client.pipelines.create(&pipe_req).await?;
    // add bans to the underlying image
    let mut image_bans = vec![
        ImageBan::new(ImageBanKind::generic("Test ban 1!")),
        ImageBan::new(ImageBanKind::generic("Test ban 2!")),
    ];
    let update =
        ImageUpdate::default().bans(ImageBanUpdate::default().add_bans(image_bans.clone()));
    client
        .images
        .update(&group, &image_req.name, &update)
        .await?;
    // check that a ban was added to the pipeline
    let pipeline = client.pipelines.get(&group, pipeline_name).await?;
    is!(pipeline.bans.len(), 1);
    let ban = pipeline.bans.values().next().unwrap();
    let ban = unwrap_variant!(&ban.ban_kind, PipelineBanKind::BannedImage);
    is!(ban.image, image_req.name);
    // remove the first ban
    let update = ImageUpdate::default()
        .bans(ImageBanUpdate::default().remove_ban(image_bans.pop().unwrap().id));
    client
        .images
        .update(&group, &image_req.name, &update)
        .await?;
    // check that the ban is still there
    let pipeline = client.pipelines.get(&group, pipeline_name).await?;
    is!(pipeline.bans.len(), 1);
    let ban = pipeline.bans.values().next().unwrap();
    let ban = unwrap_variant!(&ban.ban_kind, PipelineBanKind::BannedImage);
    is!(ban.image, image_req.name);
    // remove the last ban
    let update = ImageUpdate::default()
        .bans(ImageBanUpdate::default().remove_ban(image_bans.pop().unwrap().id));
    client
        .images
        .update(&group, &image_req.name, &update)
        .await?;
    // check that the ban is removed
    let pipeline = client.pipelines.get(&group, pipeline_name).await?;
    is!(pipeline.bans.len(), 0);
    Ok(())
}

#[tokio::test]
async fn notifications_bans() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get user client
    let user_client = generators::client(&client).await?;
    // Create a group and seed a stored image in it
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    test_utilities::faults::allow_image_writes(&group);
    // create an image
    let image_req = generators::gen_image(&group);
    client.images.create(&image_req).await?;
    // create a pipeline with a stage named after that image
    let pipeline_name = "pipeline-with-ban2";
    let instance = generators::tool_instance(&[&group], &client).await?;
    let pipe_req = PipelineRequest::new(
        &group,
        pipeline_name,
        serde_json::json!(vec![vec![&image_req.name]]),
    )
    .stage_tool(&image_req.name, instance);
    client.pipelines.create(&pipe_req).await?;
    // update the pipeline with bans
    let generic_ban_msg = "Test ban 1!";
    let pipeline_generic_ban = PipelineBan::new(PipelineBanKind::generic(generic_ban_msg));
    let update = PipelineUpdate::default()
        .bans(PipelineBanUpdate::default().add_ban(pipeline_generic_ban.clone()));
    client
        .pipelines
        .update(&group, &pipe_req.name, &update)
        .await?;
    // add a ban to the pipeline by adding a ban to the image
    let image_ban = ImageBan::new(ImageBanKind::generic("Test ban 2!"));
    let update = ImageUpdate::default().bans(ImageBanUpdate::default().add_ban(image_ban.clone()));
    client
        .images
        .update(&group, &image_req.name, &update)
        .await?;
    // get the pipeline's bans
    let pipeline_bans = client.pipelines.get(&group, &pipe_req.name).await?.bans;
    // get the pipelines notifications and make sure that a notification was created for both bans
    let notifications_ban_ids = user_client
        .pipelines
        .get_notifications(&group, &pipe_req.name)
        .await?
        .into_iter()
        .map(|notification| notification.ban_id.unwrap())
        .collect::<Vec<Uuid>>();
    is!(notifications_ban_ids.len(), 2);
    let pipeline_image_ban = pipeline_bans
        .values()
        .find(|ban| matches!(&ban.ban_kind, PipelineBanKind::BannedImage(_)))
        .unwrap();
    contains!(notifications_ban_ids, &pipeline_image_ban.id);
    contains!(notifications_ban_ids, &pipeline_generic_ban.id);
    // remove the ban from the image
    let update = ImageUpdate::default().bans(ImageBanUpdate::default().remove_ban(image_ban.id));
    client
        .images
        .update(&group, &image_req.name, &update)
        .await?;
    // get the pipelines notifications and make sure that notification was automatically removed
    let notifications = user_client
        .pipelines
        .get_notifications(&group, &pipe_req.name)
        .await?;
    is!(notifications.len(), 1);
    is!(
        &pipeline_generic_ban.id,
        notifications[0].ban_id.as_ref().unwrap()
    );
    is!(notifications[0].msg, generic_ban_msg);
    Ok(())
}

#[tokio::test]
async fn create_notification() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // create an pipeline notification
    let req = NotificationRequest::new("Test warning message!", NotificationLevel::Warn);
    client
        .pipelines
        .create_notification(&group, &pipe_req.name, &req, &NotificationParams::default())
        .await?;
    // make sure the pipeline notification is there
    let notifications = client
        .pipelines
        .get_notifications(&group, &pipe_req.name)
        .await?;
    is!(notifications.len(), 1);
    is!(notifications[0], req);
    is!(notifications[0].key.group, group);
    is!(notifications[0].key.pipeline, pipe_req.name);
    Ok(())
}

#[tokio::test]
async fn create_notification_bad() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get user client
    let user_client = generators::client(&client).await?;
    // Create a group
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // fail to create a pipeline notification as a regular user
    let req = NotificationRequest::new("Test warning message!", NotificationLevel::Warn);
    let resp = user_client
        .pipelines
        .create_notification(&group, &pipe_req.name, &req, &NotificationParams::default())
        .await;
    fail!(resp, 401);
    // fail to create an pipeline notification for a pipeline that doesn't exist
    let resp = client
        .pipelines
        .create_notification(
            &group,
            "does-not-exist",
            &req,
            &NotificationParams::default(),
        )
        .await;
    fail!(resp, 404);
    Ok(())
}

#[tokio::test]
async fn delete_notification() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // generate a random pipeline request
    let pipe_req = generators::gen_pipe(&group, 20, false, &client).await?;
    // Create a test pipeline
    client.pipelines.create(&pipe_req).await?;
    // create a notification
    let req = NotificationRequest::new("Test warning message!", NotificationLevel::Warn);
    client
        .pipelines
        .create_notification(&group, &pipe_req.name, &req, &NotificationParams::default())
        .await?;
    // get the pipeline notification
    let notification = client
        .pipelines
        .get_notifications(&group, &pipe_req.name)
        .await?
        .remove(0);
    // delete the notification
    client
        .pipelines
        .delete_notification(&group, &pipe_req.name, &notification.id)
        .await?;
    // check that the notification was deleted
    let notifications = client
        .pipelines
        .get_notifications(&group, &pipe_req.name)
        .await?;
    is!(notifications.len(), 0);
    Ok(())
}

#[tokio::test]
async fn delete_notification_bad() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get user client
    let user_client = generators::client(&client).await?;
    // Create a group
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    // setup a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &user_client)
        .await?
        .remove(0);
    // create a notification
    let req = NotificationRequest::new("Test warning message!", NotificationLevel::Warn);
    client
        .pipelines
        .create_notification(&group, &pipe_req.name, &req, &NotificationParams::default())
        .await?;
    // get the pipeline's notifications
    let notification = user_client
        .pipelines
        .get_notifications(&group, &pipe_req.name)
        .await?
        .remove(0);
    // fail to delete an pipeline notification as a regular user
    let resp = user_client
        .pipelines
        .delete_notification(&group, &pipe_req.name, &notification.id)
        .await;
    fail!(resp, 401);
    // fail to delete an pipeline notification for an pipeline that doesn't exist
    let resp = client
        .pipelines
        .delete_notification(&group, "no-exists", &notification.id)
        .await;
    fail!(resp, 404);
    // fail to delete an pipeline notification that doesn't exist
    let resp = client
        .pipelines
        .delete_notification(&group, &pipe_req.name, &Uuid::new_v4())
        .await;
    fail!(resp, 404);
    Ok(())
}
