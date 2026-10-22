//! Integration tests for deleting what other entities explicitly reference
//!
//! A tool, tool instance, toolset, agent, or workflow that an agent calls, a toolset includes, a
//! workflow orchestrates, a harness provides, a pipeline stage runs, or a tool spec names can't be
//! deleted: the delete is a `409` naming the referrers the caller can see. Membership that only
//! follows a toolset's tag selector never blocks a delete, and deleting the referrers first lets
//! the delete through, in any order.

use std::time::Duration;
use thorium::models::{
    AgentEntityRequest, AgenticToolBacking, AssociationKind, ContainerToolBacking,
    EntityMetadataRequest, EntityRequest, GroupUpdate, GroupUsersUpdate, PipelineRequest,
    ScopedTokenRequest, ToolEntityRequest, ToolSetEntityRequest, ToolSetSelector, ToolSpec,
    WorkflowCatalogEntityRequest,
};
use thorium::test_utilities::generators::{GroupRoles, god_group, moon_user};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

mod common;

use common::{agentic_tool, harness_tool};

/// Get a refused request's status code and message
///
/// # Arguments
///
/// * `result` - The refused request
fn refusal<T>(result: Result<T, Error>) -> (u16, String) {
    // the status and message of the error
    let Err(err) = result else {
        panic!("expected a refusal");
    };
    let status = err.status().map_or(0, |status| status.as_u16());
    let msg = err.msg().unwrap_or_default();
    // unwrap the message from the API's error body when it's in one
    let msg = serde_json::from_str::<serde_json::Value>(&msg)
        .ok()
        .and_then(|body| body["error"].as_str().map(ToOwned::to_owned))
        .unwrap_or(msg);
    (status, msg)
}

/// Check that a delete is refused with a `409` that names every given referrer
///
/// # Arguments
///
/// * `client` - The client deleting the entity
/// * `id` - The entity to delete
/// * `names` - The names the refusal must contain
async fn refused_naming(client: &Thorium, id: Uuid, names: &[&str]) -> Result<String, Error> {
    // the delete is a conflict
    let (status, msg) = refusal(client.entities.delete(id).await);
    assert_eq!(status, 409, "{msg}");
    // the message names each referrer
    for name in names {
        assert!(msg.contains(name), "`{name}` is missing from: {msg}");
    }
    // the entity is still there
    client.entities.get(id).await?;
    Ok(msg)
}

/// Create an agent and get its id
///
/// # Arguments
///
/// * `client` - The client to create the agent with
/// * `name` - The agent's name, which its delegation function is named after
/// * `groups` - The groups to create the agent in
/// * `req` - Adds the agent's edges to its request
async fn agent(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: impl FnOnce(AgentEntityRequest) -> AgentEntityRequest,
) -> Result<Uuid, Error> {
    // build a config named after the agent and add its edges
    let base = AgentEntityRequest::new(generators::gen_agent_config(&name.replace('-', "_")));
    Ok(client
        .entities
        .create(generators::gen_agent(name, groups, req(base)))
        .await?
        .id)
}

/// Create a toolset and get its id
///
/// # Arguments
///
/// * `client` - The client to create the toolset with
/// * `name` - The toolset's name
/// * `groups` - The groups to create the toolset in
/// * `req` - The toolset's settings and included tools
async fn toolset(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: ToolSetEntityRequest,
) -> Result<Uuid, Error> {
    // create the toolset
    Ok(client
        .entities
        .create(generators::gen_toolset(name, groups, req))
        .await?
        .id)
}

/// Create a workflow catalog and get its id
///
/// # Arguments
///
/// * `client` - The client to create the workflow with
/// * `name` - The workflow's name
/// * `groups` - The groups to create the workflow in
/// * `req` - The workflow's agents
async fn workflow(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: WorkflowCatalogEntityRequest,
) -> Result<Uuid, Error> {
    // create the workflow
    Ok(client
        .entities
        .create(generators::gen_workflow(name, groups, req))
        .await?
        .id)
}

/// A toolset an agent calls can't be deleted until the agent is, and then it can
#[tokio::test]
async fn toolset_called_by_agent() -> Result<(), Error> {
    // a toolset in one group that an agent calls
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (tool_root, _) = agentic_tool(&[&a], &admin).await?;
    let kit = toolset(
        &admin,
        "kit",
        &[&a],
        ToolSetEntityRequest::default().tool(tool_root.id),
    )
    .await?;
    let caller = agent(&admin, "dg-caller", &[&a], |req| req.tool_set(kit)).await?;
    // the delete is refused naming the agent, and nothing was removed
    refused_naming(&admin, kit, &["dg-caller"]).await?;
    let edges = admin.entities.associations(kit).await?;
    assert!(
        edges
            .iter()
            .any(|edge| edge.kind == AssociationKind::Includes)
    );
    // the agent still calls the toolset
    let stored = admin.entities.get(caller).await?;
    assert_eq!(stored.name, "dg-caller");
    // once the agent is deleted the toolset is
    admin.entities.delete(caller).await?;
    admin.entities.delete(kit).await?;
    let (status, _) = refusal(admin.entities.get(kit).await);
    assert_eq!(status, 404);
    // the tool it included is untouched
    admin.entities.get(tool_root.id).await?;
    Ok(())
}

/// A toolset nothing calls, with members from a tag selector only, deletes freely, and so do
/// the tools the selector picks up
#[tokio::test]
async fn selector_membership_never_blocks() -> Result<(), Error> {
    // a toolset whose only members come from a selector, and a tool carrying the tag
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let value = format!("dg-{suffix}");
    let selected = admin
        .entities
        .create(
            generators::gen_tool(&[&a], "1.0.0", &generators::gen_agentic_spec())
                .tag("suite", &value),
        )
        .await?;
    let kit = toolset(
        &admin,
        "picked",
        &[&a],
        ToolSetEntityRequest::default().selector(ToolSetSelector::new("suite", &value)),
    )
    .await?;
    // the selected tool deletes, because nothing names it explicitly
    admin
        .entities
        .delete(selected.tool_instance.expect("a first instance"))
        .await?;
    admin.entities.delete(selected.id).await?;
    // and so does the toolset
    admin.entities.delete(kit).await?;
    let (status, _) = refusal(admin.entities.get(kit).await);
    assert_eq!(status, 404);
    Ok(())
}

/// A tool or tool instance a toolset includes explicitly can't be deleted, but one it only
/// selects by tag can
#[tokio::test]
async fn tool_included_by_toolset() -> Result<(), Error> {
    // a toolset that includes one tool at latest and another pinned to its instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (latest_root, latest) = agentic_tool(&[&a], &admin).await?;
    let (_, pinned) = agentic_tool(&[&a], &admin).await?;
    let kit = toolset(
        &admin,
        "dg-kit",
        &[&a],
        ToolSetEntityRequest::default()
            .tool(latest_root.id)
            .tool(pinned.id),
    )
    .await?;
    // the pinned instance can't be deleted while the toolset pins it
    refused_naming(&admin, pinned.id, &["dg-kit"]).await?;
    // the latest tool's only instance goes, but its root is still included
    admin.entities.delete(latest.id).await?;
    refused_naming(&admin, latest_root.id, &["dg-kit"]).await?;
    // once the toolset is gone, both go
    admin.entities.delete(kit).await?;
    admin.entities.delete(pinned.id).await?;
    admin.entities.delete(latest_root.id).await?;
    Ok(())
}

/// A tool, tool instance, or harness an agent calls or runs in can't be deleted
#[tokio::test]
async fn tool_called_by_agent() -> Result<(), Error> {
    // an agent that calls one tool at latest, another pinned, and runs in a pinned harness
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (latest_root, latest) = agentic_tool(&[&a], &admin).await?;
    let (_, pinned) = agentic_tool(&[&a], &admin).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let caller = agent(&admin, "dg-runner", &[&a], |req| {
        req.tool(latest_root.id).tool(pinned.id).harness(harness.id)
    })
    .await?;
    // every referenced piece is refused naming the agent
    refused_naming(&admin, pinned.id, &["dg-runner"]).await?;
    refused_naming(&admin, harness.id, &["dg-runner"]).await?;
    admin.entities.delete(latest.id).await?;
    refused_naming(&admin, latest_root.id, &["dg-runner"]).await?;
    // after the agent goes they all delete
    admin.entities.delete(caller).await?;
    admin.entities.delete(pinned.id).await?;
    admin.entities.delete(harness.id).await?;
    admin.entities.delete(latest_root.id).await?;
    Ok(())
}

/// A native tool instance a harness instance provides can't be deleted until the harness
/// instance is
#[tokio::test]
async fn native_tool_provided_by_harness() -> Result<(), Error> {
    // a native tool and a harness whose first instance provides it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (native_root, native) =
        generators::tool(&[&a], "1.0.0", &generators::gen_native_spec(), &admin).await?;
    let meta =
        ToolEntityRequest::new("1.0.0", &generators::gen_harness_spec())?.provides(vec![native.id]);
    let req = EntityRequest::new(
        format!("harness{}", generators::gen_string(10).to_lowercase()),
        EntityMetadataRequest::Tool(meta),
        vec![a.clone()],
    );
    let harness = admin.entities.create(req).await?;
    let harness_instance = harness.tool_instance.expect("a first instance");
    // the provided instance is refused, naming the harness instance
    refused_naming(&admin, native.id, &["ToolInstance `harness"]).await?;
    // deleting the harness instance and then the native instance works
    admin.entities.delete(harness_instance).await?;
    admin.entities.delete(native.id).await?;
    admin.entities.delete(native_root.id).await?;
    Ok(())
}

/// An agent a workflow orchestrates or another agent delegates to can't be deleted, in any
/// order of deleting the referrers
#[tokio::test]
async fn agent_referenced_by_workflow_and_agent() -> Result<(), Error> {
    // a workflow orchestrating an agent that a parent also delegates to
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let child = agent(&admin, "dg-child", &[&a], |req| req).await?;
    let parent = agent(&admin, "dg-parent", &[&a], |req| req.sub_agent(child)).await?;
    let lead = workflow(
        &admin,
        "dg-lead",
        &[&a],
        WorkflowCatalogEntityRequest::default()
            .agent(parent)
            .agent(child)
            .entry_agent(parent),
    )
    .await?;
    // the child is named by both referrers, the parent by the workflow
    refused_naming(&admin, child, &["dg-parent", "dg-lead"]).await?;
    refused_naming(&admin, parent, &["dg-lead"]).await?;
    // deleting the parent first leaves the workflow, which still refuses both
    admin.entities.delete(lead).await?;
    refused_naming(&admin, child, &["dg-parent"]).await?;
    admin.entities.delete(parent).await?;
    admin.entities.delete(child).await?;
    Ok(())
}

/// A workflow nothing references deletes, and so does a workflow instance's root once its
/// agents are only referenced by it
#[tokio::test]
async fn workflow_deletes_first() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "dg-solo", &[&a], |req| req).await?;
    let lead = workflow(
        &admin,
        "dg-flow",
        &[&a],
        WorkflowCatalogEntityRequest::default()
            .agent(solo)
            .entry_agent(solo),
    )
    .await?;
    // the agent waits for the workflow
    refused_naming(&admin, solo, &["dg-flow"]).await?;
    // the workflow itself has no referrers
    admin.entities.delete(lead).await?;
    admin.entities.delete(solo).await?;
    Ok(())
}

/// A tool or tool instance a pipeline stage runs can't be deleted until the pipeline is
#[tokio::test]
async fn tool_run_by_pipeline_stage() -> Result<(), Error> {
    // a pipeline with one stage following a tool and one pinned to an instance
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (follow_root, follow) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (_, pinned) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&group, &name, serde_json::json!([["follow"], ["pinned"]]))
        .stage_tool("follow", follow_root.id)
        .stage_tool("pinned", pinned.id);
    admin.pipelines.create(&req).await?;
    // the pinned instance and the followed root are refused naming the pipeline
    refused_naming(&admin, pinned.id, &[&name]).await?;
    admin.entities.delete(follow.id).await?;
    refused_naming(&admin, follow_root.id, &[&name]).await?;
    // once the pipeline is deleted both go
    admin.pipelines.delete(&group, &name).await?;
    admin.entities.delete(pinned.id).await?;
    admin.entities.delete(follow_root.id).await?;
    Ok(())
}

/// An Image tool instance a container backed agentic tool pins can't be deleted until that
/// tool instance is
#[tokio::test]
async fn tool_pinned_by_container_backing() -> Result<(), Error> {
    // an Image tool and an agentic tool backed by a container that pins its instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (base_root, base) =
        generators::tool(&[&a], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let ToolSpec::Agentic(mut agentic) = generators::gen_agentic_spec() else {
        unreachable!("an agentic spec is always an agentic tool spec")
    };
    agentic.backing = AgenticToolBacking::Container(ContainerToolBacking {
        tool_instance: base.id,
        ..ContainerToolBacking::default()
    });
    let (backed_root, backed) =
        generators::tool(&[&a], "1.0.0", &ToolSpec::Agentic(agentic), &admin).await?;
    // the pinned instance is refused naming the backed tool's instance
    refused_naming(&admin, base.id, &[&backed.name]).await?;
    // once the backed tool is gone the pinned instance and its tool delete
    admin.entities.delete(backed.id).await?;
    admin.entities.delete(backed_root.id).await?;
    admin.entities.delete(base.id).await?;
    admin.entities.delete(base_root.id).await?;
    Ok(())
}

/// A tool named in another tool instance's dependency settings can't be deleted until that
/// instance is
#[tokio::test]
async fn tool_named_by_dependency_settings() -> Result<(), Error> {
    // an Image tool and a second Image tool whose results depend on the first
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (base_root, base) =
        generators::tool(&[&a], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let mut spec = generators::gen_image_spec();
    if let ToolSpec::Image(workload) = &mut spec {
        workload.data.dependencies.results.tools = vec![base_root.id];
    }
    let (dependent_root, dependent) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    // the root is refused once its instance goes, because the dependent's spec names it
    admin.entities.delete(base.id).await?;
    refused_naming(&admin, base_root.id, &[&dependent.name]).await?;
    // after the dependent instance goes everything deletes
    admin.entities.delete(dependent.id).await?;
    admin.entities.delete(base_root.id).await?;
    admin.entities.delete(dependent_root.id).await?;
    Ok(())
}

/// The refusal names a bounded list of referrers and counts the rest
#[tokio::test]
async fn refusal_is_bounded() -> Result<(), Error> {
    // a toolset that twelve agents call
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (tool_root, _) = agentic_tool(&[&a], &admin).await?;
    let kit = toolset(
        &admin,
        "busy",
        &[&a],
        ToolSetEntityRequest::default().tool(tool_root.id),
    )
    .await?;
    let mut callers = Vec::new();
    for index in 0..12 {
        let name = format!("dg-caller-{index:02}");
        callers.push(agent(&admin, &name, &[&a], |req| req.tool_set(kit)).await?);
    }
    // ten referrers are named and the other two are counted
    let msg = refused_naming(&admin, kit, &[]).await?;
    let named = msg.matches("dg-caller-").count();
    assert_eq!(named, 10, "{msg}");
    assert!(msg.contains("2 more"), "{msg}");
    // deleting all of the agents lets the toolset go
    for caller in callers {
        admin.entities.delete(caller).await?;
    }
    admin.entities.delete(kit).await?;
    Ok(())
}

/// A caller who can delete an entity but can't see a referrer's group learns only a count
#[tokio::test]
async fn hidden_referrers_are_counted() -> Result<(), Error> {
    // a submitter in two groups who is later removed from one of them
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let owner = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&owner.username);
    let seen = god_group(&admin, "minerva", &suffix, &roles).await?;
    let unseen = god_group(&admin, "juno", &suffix, &roles).await?;
    let (tool_root, _) = agentic_tool(&[&seen, &unseen], &admin).await?;
    let kit = toolset(
        &owner.client,
        "shared-kit",
        &[&seen, &unseen],
        ToolSetEntityRequest::default().tool(tool_root.id),
    )
    .await?;
    // one agent calls it in each group
    agent(&admin, "dg-visible", &[&seen], |req| req.tool_set(kit)).await?;
    let hidden = agent(&admin, "dg-secret", &[&unseen], |req| req.tool_set(kit)).await?;
    admin
        .groups
        .update(
            &unseen,
            &GroupUpdate::default()
                .users(GroupUsersUpdate::default().direct_remove(&owner.username)),
        )
        .await?;
    // the submitter's refusal names the visible agent and only counts the other
    let msg = refused_naming(&owner.client, kit, &["dg-visible", "1 you can't see"]).await?;
    assert!(!msg.contains("dg-secret"), "{msg}");
    // with only the hidden agent left, the refusal names nothing
    let agents = admin.entities.get(kit).await?;
    assert_eq!(agents.groups.len(), 2);
    admin
        .entities
        .delete(admin_agent_id(&admin, &seen, "dg-visible").await?)
        .await?;
    let msg = refused_naming(&owner.client, kit, &["1 you can't see"]).await?;
    assert!(!msg.contains("dg-secret"), "{msg}");
    // once it is deleted the submitter deletes the toolset
    admin.entities.delete(hidden).await?;
    owner.client.entities.delete(kit).await?;
    Ok(())
}

/// Find an agent's id by its name in a group
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `group` - The group the agent is in
/// * `name` - The agent's name
async fn admin_agent_id(admin: &Thorium, group: &str, name: &str) -> Result<Uuid, Error> {
    // list the group's agents and find the one with this name
    let opts = thorium::models::EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .kind(thorium::models::EntityKinds::Agent)
        .limit(100);
    let lines = admin.entities.list(&opts).await?.data;
    lines
        .into_iter()
        .find(|line| line.name == name)
        .map(|line| line.id)
        .ok_or_else(|| Error::new(format!("no agent `{name}` in `{group}`")))
}

/// A scoped token deletes unreferenced entities as before and gets the same conflict as a full
/// token on a referenced one, and a scope that doesn't hold every group is still refused first
#[tokio::test]
async fn scoped_token_deletes() -> Result<(), Error> {
    // an editor with a token scoped to one of two groups
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let editor = moon_user(&admin, "ganymede", &suffix).await?;
    let roles = GroupRoles::default().user(&editor.username);
    let in_scope = god_group(&admin, "minerva", &suffix, &roles).await?;
    let other = god_group(&admin, "juno", &suffix, &roles).await?;
    let scoped = editor
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new("delete-guard").group(&in_scope))
        .await?;
    let client = Thorium::build(&admin.host)
        .token(&scoped.token)
        .build()
        .await?;
    // a referenced toolset is a conflict and an unreferenced one deletes
    let (tool_root, _) = agentic_tool(&[&in_scope], &admin).await?;
    let used = toolset(
        &editor.client,
        "used",
        &[&in_scope],
        ToolSetEntityRequest::default().tool(tool_root.id),
    )
    .await?;
    let caller = agent(&editor.client, "dg-scoped", &[&in_scope], |req| {
        req.tool_set(used)
    })
    .await?;
    refused_naming(&client, used, &["dg-scoped"]).await?;
    editor.client.entities.delete(caller).await?;
    client.entities.delete(used).await?;
    // a toolset in a group outside the scope is refused with a 401, not a conflict
    let wide = toolset(
        &editor.client,
        "wide",
        &[&in_scope, &other],
        ToolSetEntityRequest::default(),
    )
    .await?;
    agent(&editor.client, "dg-wide", &[&in_scope], |req| {
        req.tool_set(wide)
    })
    .await?;
    let (status, _) = refusal(client.entities.delete(wide).await);
    assert_eq!(status, 401);
    Ok(())
}

/// A delete racing the creation of an agent that calls the target never leaves the agent's
/// edge pointing at a deleted toolset
#[tokio::test]
async fn delete_racing_a_new_referrer() -> Result<(), Error> {
    // repeat the race a few times, each against a fresh toolset
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    for round in 0..4 {
        let kit = toolset(
            &admin,
            &format!("racing-{round}"),
            &[&a],
            ToolSetEntityRequest::default(),
        )
        .await?;
        // delete the toolset while an agent is created calling it
        let name = format!("dg-racer-{round}");
        let base = AgentEntityRequest::new(generators::gen_agent_config(&name.replace('-', "_")))
            .tool_set(kit);
        let create = admin
            .entities
            .create(generators::gen_agent(&name, &[&a], base));
        let delete = admin.entities.delete(kit);
        let (created, deleted) = tokio::join!(create, delete);
        // either the toolset survived or the agent was never created
        let exists = admin.entities.get(kit).await.is_ok();
        assert!(
            exists || created.is_err(),
            "round {round}: the toolset was deleted and an agent calls it"
        );
        // a toolset that survived is a refused delete
        assert_eq!(exists, deleted.is_err(), "round {round}");
        // let the cleanup settle before the next round
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Entities no edge points at delete with no conflict, whatever they point at
#[tokio::test]
async fn referrers_delete_freely() -> Result<(), Error> {
    // a workflow, an agent, a toolset, and a tool chained together
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (tool_root, tool) = agentic_tool(&[&a], &admin).await?;
    let kit = toolset(
        &admin,
        "chain-kit",
        &[&a],
        ToolSetEntityRequest::default().tool(tool_root.id),
    )
    .await?;
    let caller = agent(&admin, "dg-chain", &[&a], |req| req.tool_set(kit)).await?;
    let lead = workflow(
        &admin,
        "dg-chain-flow",
        &[&a],
        WorkflowCatalogEntityRequest::default()
            .agent(caller)
            .entry_agent(caller),
    )
    .await?;
    // deleting from the top down needs no refusal
    admin.entities.delete(lead).await?;
    admin.entities.delete(caller).await?;
    admin.entities.delete(kit).await?;
    admin.entities.delete(tool.id).await?;
    admin.entities.delete(tool_root.id).await?;
    Ok(())
}
