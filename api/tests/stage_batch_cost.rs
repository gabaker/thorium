//! Tests that resolving a pipeline's stage tools reads them in one batch
//!
//! Creating or changing a pipeline and creating a reaction on it resolve every stage to a tool
//! instance. These tests count the queries a request sends to the entity table and check that
//! the count doesn't grow with the number of stages or reactions. The count covers every request
//! the in-process test API serves, so this is its own test binary and its tests run one at a time.

use thorium::models::{
    GroupUpdate, GroupUsersUpdate, ImageScaler, PipelineRequest, PipelineUpdate, ReactionRequest,
    UserRole,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Runs the tests in this binary one at a time, since the query count is process wide
static SERIAL: Mutex<()> = Mutex::const_new(());

/// A group with the tools its pipelines' stages run
struct Setup {
    /// The admin client
    admin: Thorium,
    /// The group the pipelines and tools are in
    group: String,
}

impl Setup {
    /// Create a group
    async fn new() -> Result<Self, Error> {
        // create a group for the pipelines
        let admin = test_utilities::admin_client().await?;
        let group = generators::groups(1, &admin).await?.remove(0).name;
        Ok(Setup { admin, group })
    }

    /// Create image tools in the group
    ///
    /// # Arguments
    ///
    /// * `count` - The number of tools to create
    ///
    /// # Returns
    ///
    /// The stage names, the ids of the tools' first instances, and the ids of the tools
    async fn tools(&self, count: usize) -> Result<(Vec<String>, Vec<Uuid>, Vec<Uuid>), Error> {
        // create each tool with one instance and name a stage after it
        let spec = generators::gen_image_spec_for(ImageScaler::K8s);
        let mut stages = Vec::with_capacity(count);
        let mut instances = Vec::with_capacity(count);
        let mut tools = Vec::with_capacity(count);
        for _ in 0..count {
            let (tool, instance) =
                generators::tool(&[&self.group], "1.0.0", &spec, &self.admin).await?;
            stages.push(tool.name.clone());
            instances.push(instance.id);
            tools.push(tool.id);
        }
        Ok((stages, instances, tools))
    }

    /// Build a request for a pipeline that runs each stage in its own step
    ///
    /// # Arguments
    ///
    /// * `stages` - The stage names in order
    /// * `backing` - The id backing each stage
    fn request(&self, stages: &[String], backing: &[Uuid]) -> PipelineRequest {
        // name the pipeline and run each stage in its own step
        let name = generators::gen_string(20).to_lowercase();
        let order = serde_json::json!(stages.iter().map(|stage| vec![stage]).collect::<Vec<_>>());
        // back every stage with its tool
        stages.iter().zip(backing).fold(
            PipelineRequest::new(&self.group, &name, order),
            |req, (stage, id)| req.stage_tool(stage, *id),
        )
    }

    /// Create a pipeline whose stages run tools and return its name
    ///
    /// # Arguments
    ///
    /// * `count` - The number of stages
    /// * `use_tools` - Back the stages with tools instead of their first instances
    async fn pipeline(&self, count: usize, use_tools: bool) -> Result<String, Error> {
        // create the tools and the pipeline over them
        let (stages, instances, tools) = self.tools(count).await?;
        let backing = if use_tools { tools } else { instances };
        let req = self.request(&stages, &backing);
        self.admin.pipelines.create(&req).await?;
        Ok(req.name)
    }
}

/// Count the queries sent to the entity table while a request runs
macro_rules! queries {
    ($request:expr) => {{
        // read the count around the request
        let before = test_utilities::faults::entity_queries();
        let result = $request;
        (result, test_utilities::faults::entity_queries() - before)
    }};
}

/// Creating a reaction reads its stages in one query however many stages the pipeline has
#[tokio::test]
async fn reaction_create_reads_stages_in_one_batch() -> Result<(), Error> {
    // the queries of a reaction create don't grow with the stages
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    for count in [1, 3, 8] {
        // create a pipeline of this many stages and a reaction request for it
        let pipeline = setup.pipeline(count, false).await?;
        let req = ReactionRequest::new(&setup.group, &pipeline);
        // count the queries of the create
        let (created, queries) = queries!(setup.admin.reactions.create(&req).await);
        created?;
        assert_eq!(queries, 1, "{count} stages");
    }
    Ok(())
}

/// Stages backed by tools take one query for the tools and one for their latest instances
#[tokio::test]
async fn reaction_create_reads_tools_and_latest_instances_in_two_batches() -> Result<(), Error> {
    // stages backed by tools need one batch for the tools and one for their latest instances
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    for count in [1, 3, 8] {
        // create a pipeline of this many tool backed stages and a reaction request for it
        let pipeline = setup.pipeline(count, true).await?;
        let req = ReactionRequest::new(&setup.group, &pipeline);
        // count the queries of the create
        let (created, queries) = queries!(setup.admin.reactions.create(&req).await);
        created?;
        assert_eq!(queries, 2, "{count} stages");
    }
    Ok(())
}

/// A bulk create reads the pipeline's stages once however many reactions it creates
#[tokio::test]
async fn bulk_reaction_create_reads_stages_once_per_request() -> Result<(), Error> {
    // the queries of a bulk create don't grow with the reactions
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // create a pipeline of three stages
    let pipeline = setup.pipeline(3, false).await?;
    for count in [1, 10, 60] {
        // build this many reactions on it and count the queries of creating them
        let reqs = (0..count)
            .map(|_| ReactionRequest::new(&setup.group, &pipeline))
            .collect::<Vec<ReactionRequest>>();
        let (created, queries) = queries!(setup.admin.reactions.create_bulk(&reqs).await);
        assert_eq!(created?.created.len(), count);
        assert_eq!(queries, 1, "{count} reactions");
    }
    Ok(())
}

/// A bulk create over pipelines that share tools reads each tool once
#[tokio::test]
async fn bulk_reaction_create_reads_shared_stages_once() -> Result<(), Error> {
    // two pipelines that share their tools read each tool once for the whole request
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // create two pipelines over the same three tools
    let (stages, instances, _) = setup.tools(3).await?;
    let first = setup.request(&stages, &instances);
    let second = setup.request(&stages, &instances);
    setup.admin.pipelines.create(&first).await?;
    setup.admin.pipelines.create(&second).await?;
    // create reactions on both pipelines in one request and count its queries
    let reqs = vec![
        ReactionRequest::new(&setup.group, &first.name),
        ReactionRequest::new(&setup.group, &second.name),
        ReactionRequest::new(&setup.group, &first.name),
    ];
    let (created, queries) = queries!(setup.admin.reactions.create_bulk(&reqs).await);
    assert_eq!(created?.created.len(), 3);
    assert_eq!(queries, 1);
    Ok(())
}

/// Creating a pipeline reads its stage tools in one query, for an admin and for a member
#[tokio::test]
async fn pipeline_create_reads_stages_in_one_batch() -> Result<(), Error> {
    // the queries of a pipeline create don't grow with the stages, for an admin and for a member
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // make a developer who owns the group
    let member = generators::user_with_role(
        &setup.admin,
        UserRole::Developer {
            k8s: true,
            bare_metal: false,
            windows: false,
            external: false,
            kvm: false,
        },
    )
    .await?;
    let update =
        GroupUpdate::default().owners(GroupUsersUpdate::default().direct_add(&member.username));
    setup.admin.groups.update(&setup.group, &update).await?;
    for count in [1, 3, 8] {
        // create this many tools and count the queries of creating a pipeline over them
        let (stages, instances, _) = setup.tools(count).await?;
        let req = setup.request(&stages, &instances);
        let (created, queries) = queries!(setup.admin.pipelines.create(&req).await);
        created?;
        assert_eq!(queries, 1, "{count} stages, admin");
        // do the same as the member
        let req = setup.request(&stages, &instances);
        let (created, queries) = queries!(member.client.pipelines.create(&req).await);
        created?;
        assert_eq!(queries, 1, "{count} stages, member");
    }
    Ok(())
}

/// Changing a pipeline's stage tool rechecks every stage in one query
#[tokio::test]
async fn pipeline_update_reads_stages_in_one_batch() -> Result<(), Error> {
    // a stage change checks every stage of the pipeline in one batch
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    for count in [1, 3, 8] {
        // create a pipeline of this many stages
        let pipeline = setup.pipeline(count, false).await?;
        // swap the first stage's tool for a new one and count the queries of the update
        let pipe = setup.admin.pipelines.get(&setup.group, &pipeline).await?;
        let stage = pipe.order[0][0].clone();
        let (_, replacement, _) = setup.tools(1).await?;
        let update = PipelineUpdate::default().add_stage_tool(&stage, replacement[0]);
        let (updated, queries) = queries!(
            setup
                .admin
                .pipelines
                .update(&setup.group, &pipeline, &update)
                .await
        );
        updated?;
        assert_eq!(queries, 1, "{count} stages");
    }
    Ok(())
}

/// An update that leaves the stages alone still rechecks them in one query
#[tokio::test]
async fn pipeline_update_without_stage_change_reads_stages_in_one_batch() -> Result<(), Error> {
    // every update rechecks the pipeline's stage tools, so a description change reads them once
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // create a pipeline and change only its description
    let pipeline = setup.pipeline(3, false).await?;
    let update = PipelineUpdate {
        description: Some("changed".to_owned()),
        ..PipelineUpdate::default()
    };
    let (updated, queries) = queries!(
        setup
            .admin
            .pipelines
            .update(&setup.group, &pipeline, &update)
            .await
    );
    updated?;
    assert_eq!(queries, 1);
    Ok(())
}

/// A missing tool, the wrong kind of entity, and a tool in another group are each refused
/// naming the stage
#[tokio::test]
async fn stage_errors_name_the_stage() -> Result<(), Error> {
    // a missing tool, the wrong kind of entity, and a tool in another group are each refused
    // naming the stage, whether or not the other stages are fine
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // create three tools to back the stages
    let (stages, instances, _) = setup.tools(3).await?;
    // a missing tool
    let missing = Uuid::new_v4();
    let mut backing = instances.clone();
    backing[1] = missing;
    let resp = setup
        .admin
        .pipelines
        .create(&setup.request(&stages, &backing))
        .await;
    fail!(resp, 404);
    let msg = resp.err().and_then(|err| err.msg()).unwrap_or_default();
    assert!(
        msg.contains(&format!("stage `{}`", stages[1])) && msg.contains(&missing.to_string()),
        "{msg}"
    );
    // an entity that is not a tool
    let (_, other) = generators::entity(
        generators::gen_entity(&setup.group, generators::gen_other_meta()),
        &setup.admin,
    )
    .await?;
    let mut backing = instances.clone();
    backing[2] = other.id;
    let resp = setup
        .admin
        .pipelines
        .create(&setup.request(&stages, &backing))
        .await;
    fail!(resp, 400);
    let msg = resp.err().and_then(|err| err.msg()).unwrap_or_default();
    assert!(
        msg.contains(&format!("stage `{}` must be backed by a Tool", stages[2])),
        "{msg}"
    );
    // a tool in another group
    let elsewhere = generators::groups(1, &setup.admin).await?.remove(0).name;
    let spec = generators::gen_image_spec_for(ImageScaler::K8s);
    let (_, foreign) = generators::tool(&[&elsewhere], "1.0.0", &spec, &setup.admin).await?;
    let mut backing = instances.clone();
    backing[0] = foreign.id;
    let resp = setup
        .admin
        .pipelines
        .create(&setup.request(&stages, &backing))
        .await;
    fail!(resp, 400);
    let msg = resp.err().and_then(|err| err.msg()).unwrap_or_default();
    assert!(
        msg.contains(&format!("stage `{}`", stages[0]))
            && msg.contains("is not in the pipeline's group"),
        "{msg}"
    );
    Ok(())
}

/// Stages that share a tool each resolve to it and the plan keeps the pipeline's order
#[tokio::test]
async fn shared_and_repeated_stage_tools_resolve_in_order() -> Result<(), Error> {
    // stages that share a tool resolve to it each, and the plan keeps the pipeline's order
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // back three stages with two tools, the first tool twice
    let (stages, instances, _) = setup.tools(2).await?;
    let backing = vec![instances[0], instances[1], instances[0]];
    let mut names = stages.clone();
    names.push(format!("{}-again", stages[0]));
    let req = setup.request(&names, &backing);
    setup.admin.pipelines.create(&req).await?;
    // create a reaction on it and count the queries
    let (created, queries) = queries!(
        setup
            .admin
            .reactions
            .create(&ReactionRequest::new(&setup.group, &req.name))
            .await
    );
    let created = created?;
    assert_eq!(queries, 1);
    // read the reaction's plan back in stage order
    let reaction = setup.admin.reactions.get(&setup.group, created.id).await?;
    let planned = reaction
        .plan
        .stages
        .iter()
        .flatten()
        .map(|stage| (stage.name.clone(), stage.tool_instance))
        .collect::<Vec<_>>();
    assert_eq!(planned, names.into_iter().zip(backing).collect::<Vec<_>>());
    Ok(())
}

/// A tool whose only instance was deleted is refused naming the stage
#[tokio::test]
async fn tool_without_a_runnable_instance_is_refused_naming_the_stage() -> Result<(), Error> {
    // a tool whose only instance was deleted has nothing to run, whichever stages sit around it
    // run alone so no other request lands in the count
    let _guard = SERIAL.lock().await;
    // create the group the tools and pipelines live in
    let setup = Setup::new().await?;
    // create three tools and delete the middle tool's only instance
    let (stages, instances, tools) = setup.tools(3).await?;
    setup.admin.entities.delete(instances[1]).await?;
    // back the middle stage with the tool and the others with their instances
    let backing = vec![instances[0], tools[1], instances[2]];
    let resp = setup
        .admin
        .pipelines
        .create(&setup.request(&stages, &backing))
        .await;
    let msg = resp
        .err()
        .and_then(|err| err.msg())
        .expect("a tool with no instance backed a stage");
    assert!(
        msg.contains(&format!(
            "stage `{}`: tool `{}` has no version to run",
            stages[1], stages[1]
        )),
        "{msg}"
    );
    Ok(())
}
