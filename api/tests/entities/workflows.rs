//! Integration tests for workflow catalogs and workflow instances
//!
//! These cover creating workflow catalogs with their `Orchestrates` edges and the rules those
//! edges follow, the entry agent, the model and limit settings, editing, widening, and deleting
//! catalogs, and submitted workflow instances: their permissions, references, stored hashes,
//! snapshot groups, digest, revisions, revision lock, intrinsic tags, the harness toolset read,
//! and the limits on editing them, and the workflow and toolset resolvers reading the graph
//! through the client.

use reqwest::multipart::Form;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use thorium::models::entities::toolsets::{
    ToolSetBuild, build_tool_set_instance, resolve_tool_set,
};
use thorium::models::entities::workflows::{
    GraphReader, MemoryGraph, WorkflowBuild, build_workflow_instance, resolve_workflow,
};
use thorium::models::{
    AgentEntity, AgentEntityRequest, AgenticToolBacking, AssociationKind, ContainerToolBacking,
    CrawlLimits, Directionality, Entity, EntityKinds, EntityListOpts, EntityMetadata,
    EntityMetadataUpdate, EntityResponse, EntityUpdate, InstancedAgent, InstancedTool,
    InstancedToolBinding, ModelAuthLevel, ModelAuthSource, ModelMatcher, ToolEffect, ToolMechanism,
    ToolSetEntityRequest, ToolSetExclusionReason, ToolSetInclusion, ToolSetInstanceStatus,
    ToolSetInstanceTrigger, ToolSetLimits, ToolSetSelector, ToolSpec, WorkflowCatalogEntity,
    WorkflowCatalogEntityRequest, WorkflowGraphChange, WorkflowInstanceEntity,
    WorkflowInstanceStatus, WorkflowInstanceTrigger, WorkflowLimits,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser, god_group, moon_user};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

mod common;

use common::{
    agentic_tool, changes, changes_since, edges, harness_tool, ids, queued, release_later, sorted,
    timed_refusal, update_metadata,
};

/// Create an agent that calls some tools, toolsets, and sub-agents, and get it back
///
/// # Arguments
///
/// * `client` - The client to create the agent with
/// * `name` - The agent's name, also used for its delegation function name
/// * `groups` - The groups to create the agent in
/// * `req` - Adds the agent's edges to a request with a generated config
async fn agent(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: impl FnOnce(AgentEntityRequest) -> AgentEntityRequest,
) -> Result<Entity, Error> {
    // build a config named after the agent and add its edges
    let base = AgentEntityRequest::new(generators::gen_agent_config(&name.replace('-', "_")));
    let resp = client
        .entities
        .create(generators::gen_agent(name, groups, req(base)))
        .await?;
    client.entities.get(resp.id).await
}

/// Create a workflow catalog and get it back
///
/// # Arguments
///
/// * `client` - The client to create the workflow with
/// * `name` - The workflow's name
/// * `groups` - The groups to create the workflow in
/// * `req` - The workflow's agents and settings
async fn workflow(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: WorkflowCatalogEntityRequest,
) -> Result<Entity, Error> {
    // create the workflow and read it back in all of its groups
    let resp = client
        .entities
        .create(generators::gen_workflow(name, groups, req))
        .await?;
    client.entities.get(resp.id).await
}

/// Submit a workflow instance
///
/// # Arguments
///
/// * `client` - The client to submit with
/// * `instance` - The instance to submit
async fn submit(
    client: &Thorium,
    instance: &WorkflowInstanceEntity,
) -> Result<EntityResponse, Error> {
    // send the instance in its workflow's groups
    client
        .entities
        .create(generators::gen_workflow_instance_req(instance))
        .await
}

/// Get a workflow catalog's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The workflow entity
fn meta(entity: &Entity) -> &WorkflowCatalogEntity {
    // only workflow catalogs have workflow metadata
    match &entity.metadata {
        EntityMetadata::WorkflowCatalog(workflow) => workflow,
        other => panic!("expected a workflow catalog, got {other:?}"),
    }
}

/// Get a workflow instance's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The workflow instance entity
fn instance_meta(entity: &Entity) -> &WorkflowInstanceEntity {
    // only workflow instances have workflow instance metadata
    match &entity.metadata {
        EntityMetadata::WorkflowInstance(instance) => instance,
        other => panic!("expected a workflow instance, got {other:?}"),
    }
}

/// Get an agent's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The agent entity
fn agent_meta(entity: &Entity) -> &AgentEntity {
    // only agents have agent metadata
    match &entity.metadata {
        EntityMetadata::Agent(agent) => agent,
        other => panic!("expected an agent, got {other:?}"),
    }
}

/// Sort a list of groups
///
/// # Arguments
///
/// * `groups` - The groups to sort
fn sorted_groups(groups: &[String]) -> Vec<String> {
    // copy and sort the groups
    let mut groups = groups.to_vec();
    groups.sort();
    groups
}

/// Get a refusal's status code and message
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

/// Send a raw multipart request as a user and get its status and body
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `patch` - The entity to update, or `None` to create one
/// * `fields` - The form fields to send
async fn raw_form(
    user: &TestUser,
    patch: Option<Uuid>,
    fields: &[(&str, String)],
) -> Result<(u16, String), Error> {
    // build the form from the fields
    let form = fields.iter().fold(Form::new(), |form, (name, value)| {
        form.text((*name).to_owned(), value.clone())
    });
    // send it as the user
    let client = reqwest::Client::new();
    let req = match patch {
        Some(id) => client.patch(format!("{}/api/entities/{id}", user.client.host)),
        None => client.post(format!("{}/api/entities/", user.client.host)),
    };
    let resp = req
        .header("authorization", user.auth_header())
        .multipart(form)
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// Read a url's raw response as a user
///
/// # Arguments
///
/// * `user` - The user to read as
/// * `path` - The path to read under the API's host
async fn raw_get_path(user: &TestUser, path: &str) -> Result<(u16, serde_json::Value), Error> {
    // read the path as the user
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", user.client.host))
        .header("authorization", user.auth_header())
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    Ok((status, body))
}

/// Add or delete an entity's tags as a user, returning the status code
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `id` - The entity to change the tags of
/// * `delete` - Whether to delete the tags instead of adding them
/// * `tags` - The tag request body
async fn raw_tags(
    user: &TestUser,
    id: Uuid,
    delete: bool,
    tags: serde_json::Value,
) -> Result<u16, Error> {
    // send the tag request as the user
    let client = reqwest::Client::new();
    let url = format!("{}/api/entities/tags/{id}", user.client.host);
    let req = if delete {
        client.delete(url)
    } else {
        client.post(url)
    };
    let resp = req
        .header("authorization", user.auth_header())
        .json(&tags)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Get the sorted values of one of an entity's tags
///
/// # Arguments
///
/// * `entity` - The entity to read the tag of
/// * `key` - The tag's key
fn tag_values(entity: &Entity, key: &str) -> Vec<String> {
    // collect and sort the tag's values
    let mut values = entity.tags.get(key).map_or_else(Vec::new, |values| {
        values.keys().cloned().collect::<Vec<String>>()
    });
    values.sort();
    values
}

/// Get the ids of the entities of one kind listed in some groups
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `groups` - The groups to list
/// * `kind` - The kind of entity to list
async fn listed(
    client: &Thorium,
    groups: &[String],
    kind: EntityKinds,
) -> Result<Vec<Uuid>, Error> {
    // list the entities of this kind in the groups
    let opts = EntityListOpts::default()
        .groups(groups.to_vec())
        .kind(kind)
        .limit(1_000);
    let mut found = client
        .entities
        .list(&opts)
        .await?
        .data
        .into_iter()
        .map(|line| line.id)
        .collect::<Vec<Uuid>>();
    found.sort();
    found.dedup();
    Ok(found)
}

/// Build an empty workflow update, then let the caller set its fields
///
/// # Arguments
///
/// * `change` - Sets the fields of an empty workflow update
fn workflow_update(change: impl FnOnce(&mut EntityMetadataUpdate)) -> EntityMetadataUpdate {
    // start from an update that changes nothing
    let mut update = EntityMetadataUpdate::WorkflowCatalog {
        entry_agent: None,
        clear_entry_agent: None,
        auto_instance: None,
        limits: None,
        default_models: None,
        model_auth: None,
        add_agents: Vec::new(),
        remove_agents: Vec::new(),
        extend_target_groups: false,
    };
    change(&mut update);
    update
}

/// Build a model matcher for a model id
///
/// # Arguments
///
/// * `model_id` - The model's id
fn model(model_id: &str) -> ModelMatcher {
    ModelMatcher {
        model_id: model_id.to_owned(),
        ..ModelMatcher::default()
    }
}

/// Recompute an instance's agent toolset digests and its digest after its content changed
///
/// # Arguments
///
/// * `instance` - The instance to refresh
/// * `tool_sets` - The digest of each toolset instance its agents were given
fn refresh_digests(instance: &mut WorkflowInstanceEntity, tool_sets: &BTreeMap<Uuid, String>) {
    // each agent's toolset digest covers its tools
    let tools = instance.tools.clone();
    for agent in &mut instance.agents {
        agent.toolset_digest = agent.compute_toolset_digest(&tools).unwrap();
    }
    instance.digest = instance.compute_digest(tool_sets).unwrap();
}

/// An edit of one setting in a workflow metadata update
type SettingEdit<'a> = Box<dyn Fn(&mut EntityMetadataUpdate) + 'a>;

/// Request a manual instance of a workflow as a user and get the raw status and body
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `id` - The id of the workflow to instance
async fn raw_request_instance(user: &TestUser, id: Uuid) -> Result<(u16, String), Error> {
    // post to the manual instance route as the user
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/entities/workflows/{id}/instance",
            user.client.host
        ))
        .header("authorization", user.auth_header())
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// A workflow create whose edge writes fail partway answers with the failure and still queues
/// the workflow's update, which covers the edges it wrote
#[tokio::test]
async fn failed_create_edges_still_queue_update() -> Result<(), Error> {
    // two agents in minerva that europa can edit
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let groups = [minerva.as_str()];
    let one = agent(&europa.client, "one", &groups, |req| req).await?;
    let two = agent(&europa.client, "two", &groups, |req| req).await?;
    // a create whose second edge fails to write
    let name = format!("half-written-{suffix}");
    let id = Uuid::new_v4();
    test_utilities::faults::create_with_id(&name, id);
    test_utilities::faults::fail_edge_write_at(id, 1);
    let req = WorkflowCatalogEntityRequest::default()
        .agent(one.id)
        .agent(two.id);
    let refused = europa
        .client
        .entities
        .create(generators::gen_workflow(&name, &groups, req))
        .await;
    thorium::fail!(refused, 500);
    // queues the workflow's update anyway
    assert_eq!(
        changes(&admin, id).await?,
        vec![queued(
            &europa.username,
            &groups,
            WorkflowGraphChange::WorkflowUpdated { workflow: id },
        )]
    );
    Ok(())
}

/// An edge write that fails partway queues the changes of the edges it wrote, and answers with
/// the write's failure even when queueing them fails too
#[tokio::test]
async fn failed_edge_writes_queue_written_edges() -> Result<(), Error> {
    // two tools in minerva that europa can call
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let groups = [minerva.as_str()];
    let (first, _) = agentic_tool(&groups, &admin).await?;
    let (second, _) = agentic_tool(&groups, &admin).await?;
    // an agent whose second edge fails to write queues the first edge's change
    let id = Uuid::new_v4();
    let name = format!("half-calls-{suffix}");
    test_utilities::faults::create_with_id(&name, id);
    test_utilities::faults::fail_edge_write_at(id, 1);
    let refused = agent(&europa.client, &name, &groups, |req| {
        req.tool(first.id).tool(second.id)
    })
    .await;
    thorium::fail!(refused, 500);
    let queued_edges = changes(&admin, id).await?;
    assert_eq!(queued_edges.len(), 1, "{queued_edges:?}");
    assert!(
        matches!(
            queued_edges[0].change,
            WorkflowGraphChange::EdgeChanged { source, .. } if source == id
        ),
        "{queued_edges:?}"
    );
    // a failed write whose changes can't be queued answers with the write's failure
    let id = Uuid::new_v4();
    let name = format!("unqueued-calls-{suffix}");
    test_utilities::faults::create_with_id(&name, id);
    test_utilities::faults::fail_edge_write_at(id, 1);
    test_utilities::faults::fail_edge_queue(id);
    let refused = agent(&europa.client, &name, &groups, |req| {
        req.tool(first.id).tool(second.id)
    })
    .await;
    thorium::fail!(refused, 500);
    Ok(())
}

/// Creating a catalog queues only its update, each graph setting change queues one update, each
/// agent change queues one edge change, and the other edits queue nothing
#[tokio::test]
// one scenario walks every edit in turn against one catalog
#[allow(clippy::too_many_lines)]
async fn catalog_edit_events() -> Result<(), Error> {
    // two agents in minerva and mars that europa can edit
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let groups = [minerva.as_str(), mars.as_str()];
    let one = agent(&europa.client, "one", &groups, |req| req).await?;
    let two = agent(&europa.client, "two", &groups, |req| req).await?;
    // creating a catalog with one agent queues only its update, which covers its edges
    let req = WorkflowCatalogEntityRequest::default().agent(one.id);
    let id = europa
        .client
        .entities
        .create(generators::gen_workflow("events", &groups, req))
        .await?
        .id;
    let updated = queued(
        &europa.username,
        &groups,
        WorkflowGraphChange::WorkflowUpdated { workflow: id },
    );
    let edge = |target| {
        queued(
            &europa.username,
            &groups,
            WorkflowGraphChange::EdgeChanged {
                source: id,
                target,
                kind: AssociationKind::Orchestrates,
            },
        )
    };
    assert_eq!(changes(&admin, id).await?, vec![updated.clone()]);
    // each graph setting change queues one update
    let settings: [SettingEdit<'_>; 4] = [
        Box::new(|update| {
            if let EntityMetadataUpdate::WorkflowCatalog { entry_agent, .. } = update {
                *entry_agent = Some(one.id);
            }
        }),
        Box::new(|update| {
            if let EntityMetadataUpdate::WorkflowCatalog { auto_instance, .. } = update {
                *auto_instance = Some(false);
            }
        }),
        Box::new(|update| {
            if let EntityMetadataUpdate::WorkflowCatalog { limits, .. } = update {
                *limits = Some(WorkflowLimits {
                    max_alive_sub_agents: Some(3),
                });
            }
        }),
        Box::new(|update| {
            if let EntityMetadataUpdate::WorkflowCatalog { default_models, .. } = update {
                *default_models = Some(vec![model("claude-sonnet")]);
            }
        }),
    ];
    for setting in &settings {
        let seen = changes(&admin, id).await?.len();
        update_metadata(&europa.client, id, workflow_update(setting)).await?;
        assert_eq!(
            changes_since(&admin, id, seen).await?,
            vec![updated.clone()]
        );
    }
    // sending the settings' current values again changes nothing and queues nothing
    let seen = changes(&admin, id).await?.len();
    for setting in &settings {
        update_metadata(&europa.client, id, workflow_update(setting)).await?;
    }
    assert_eq!(changes_since(&admin, id, seen).await?.len(), 0);
    // adding and removing an agent each queue one edge change
    let add = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { add_agents, .. } = update {
            add_agents.push(two.id);
        }
    });
    update_metadata(&europa.client, id, add).await?;
    assert_eq!(changes_since(&admin, id, seen).await?, vec![edge(two.id)]);
    let remove = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(two.id);
        }
    });
    update_metadata(&europa.client, id, remove).await?;
    assert_eq!(
        changes_since(&admin, id, seen).await?,
        vec![edge(two.id), edge(two.id)]
    );
    // name, description, user tag, and model auth edits queue nothing
    let seen = changes(&admin, id).await?.len();
    europa
        .client
        .entities
        .update(
            id,
            EntityUpdate::default()
                .name("renamed")
                .description("edited"),
        )
        .await?;
    let tags = json!({"groups": [&minerva], "tags": {"team": ["blue"]}});
    assert_eq!(raw_tags(&europa, id, false, tags).await?, 204);
    let auth = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { model_auth, .. } = update {
            *model_auth = Some(ModelAuthSource {
                levels: [ModelAuthLevel::Group].into(),
            });
        }
    });
    update_metadata(&europa.client, id, auth).await?;
    assert_eq!(changes_since(&admin, id, seen).await?.len(), 0);
    // a refused update queues nothing
    let refused = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { entry_agent, .. } = update {
            *entry_agent = Some(two.id);
        }
    });
    let (status, _) = refusal(update_metadata(&europa.client, id, refused).await);
    assert_eq!(status, 400);
    assert_eq!(changes_since(&admin, id, seen).await?.len(), 0);
    Ok(())
}

/// The manual instance route queues one manual instance change for a user who can edit the
/// workflow in every group, and refuses everyone else without queuing anything
#[tokio::test]
async fn manual_instance_route() -> Result<(), Error> {
    // a workflow in minerva and mars; europa edits both, callisto only minerva, and amalthea
    // is only in mercury
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .user(&callisto.username),
    )
    .await?;
    let mars = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .monitor(&callisto.username),
    )
    .await?;
    let _mercury = god_group(
        &admin,
        "mercury",
        &suffix,
        &GroupRoles::default().user(&amalthea.username),
    )
    .await?;
    let groups = [minerva.as_str(), mars.as_str()];
    let solo = agent(&admin, "solo", &groups, |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "manual", &groups, req).await?;
    let seen = changes(&admin, created.id).await?.len();
    // europa's request is accepted with an empty body and queues one manual instance change
    let (status, body) = raw_request_instance(&europa, created.id).await?;
    assert_eq!(status, 202);
    assert_eq!(body, String::new());
    let manual = queued(
        &europa.username,
        &groups,
        WorkflowGraphChange::ManualInstance {
            workflow: created.id,
        },
    );
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![manual.clone()]
    );
    // the client method sends the same request
    europa.client.entities.request_instance(created.id).await?;
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![manual.clone(), manual]
    );
    let seen = seen + 2;
    // an unknown id and an agent's id aren't workflows
    assert_eq!(raw_request_instance(&europa, Uuid::new_v4()).await?.0, 404);
    assert_eq!(raw_request_instance(&europa, solo.id).await?.0, 404);
    let agent_changes = changes(&admin, solo.id).await?;
    assert!(
        agent_changes
            .iter()
            .all(|queued| !matches!(queued.change, WorkflowGraphChange::ManualInstance { .. })),
        "{agent_changes:?}"
    );
    // a user outside every group of the workflow can't see it
    assert_eq!(raw_request_instance(&amalthea, created.id).await?.0, 404);
    // a user who can see it but edits only one of its groups is refused
    let (status, body) = raw_request_instance(&callisto, created.id).await?;
    assert_eq!(status, 401);
    assert!(body.contains("manual"), "{body}");
    // none of the refused requests queued anything
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 0);
    Ok(())
}

/// A catalog is created in two groups with its agents, settings, and timestamps, reads its
/// agents and latest instance back, and its instance gets exactly its groups
#[tokio::test]
// one scenario walks the catalog's life against shared fixtures
#[allow(clippy::too_many_lines)]
async fn create_with_agents() -> Result<(), Error> {
    // two agents in minerva and mars, and a third to add later
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (_, tool) = agentic_tool(&[&a, &b], &admin).await?;
    let triage = agent(&admin, "triage", &[&a, &b], |req| req.tool(tool.id)).await?;
    let reverser = agent(&admin, "reverser", &[&a, &b], |req| req).await?;
    let late = agent(&admin, "late", &[&a, &b], |req| req).await?;
    // a catalog with both agents, an entry agent, limits, models, and model auth
    let req = WorkflowCatalogEntityRequest::default()
        .agent(triage.id)
        .agent(reverser.id)
        .entry_agent(triage.id)
        .limits(WorkflowLimits {
            max_alive_sub_agents: Some(4),
        })
        .default_model(model("claude-sonnet"))
        .default_model(model("gpt-5"))
        .model_auth(ModelAuthSource {
            levels: [ModelAuthLevel::User].into(),
        });
    let created = workflow(&europa.client, "Malware Triage", &[&a, &b], req).await?;
    // the catalog reads back every field, its agents, and no latest instance
    let catalog = meta(&created);
    assert_eq!(ids(&catalog.agents), sorted(&[triage.id, reverser.id]));
    assert_eq!(catalog.entry_agent, Some(triage.id));
    assert!(catalog.auto_instance);
    assert_eq!(catalog.limits.max_alive_sub_agents, Some(4));
    assert_eq!(
        catalog
            .default_models
            .iter()
            .map(|matcher| matcher.model_id.clone())
            .collect::<Vec<String>>(),
        vec!["claude-sonnet".to_owned(), "gpt-5".to_owned()]
    );
    assert!(catalog.model_auth.levels.contains(&ModelAuthLevel::User));
    assert_eq!(catalog.latest_instance, None);
    assert_eq!(
        sorted_groups(&created.groups),
        sorted_groups(&[a.clone(), b.clone()])
    );
    assert_eq!(created.submitter, europa.username);
    assert_eq!(catalog.modified, created.created);
    // the agents are Orchestrates edges in both directions
    assert_eq!(
        edges(
            &admin,
            created.id,
            AssociationKind::Orchestrates,
            Directionality::To
        )
        .await?,
        sorted(&[triage.id, reverser.id])
    );
    assert_eq!(
        edges(
            &admin,
            triage.id,
            AssociationKind::Orchestrates,
            Directionality::From
        )
        .await?,
        vec![created.id]
    );
    // a description edit moves modified forward
    tokio::time::sleep(Duration::from_millis(20)).await;
    europa
        .client
        .entities
        .update(
            created.id,
            EntityUpdate::default().description("triage files"),
        )
        .await?;
    let edited = europa.client.entities.get(created.id).await?;
    assert!(meta(&edited).modified > catalog.modified);
    // adding an agent moves the catalog's modified forward but not the agent's
    tokio::time::sleep(Duration::from_millis(20)).await;
    let late_modified = agent_meta(&late).modified;
    let add = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { add_agents, .. } = update {
            add_agents.push(late.id);
        }
    });
    update_metadata(&europa.client, created.id, add).await?;
    let grown = europa.client.entities.get(created.id).await?;
    assert!(meta(&grown).modified > meta(&edited).modified);
    assert_eq!(
        ids(&meta(&grown).agents),
        sorted(&[triage.id, reverser.id, late.id])
    );
    assert_eq!(
        agent_meta(&admin.entities.get(late.id).await?).modified,
        late_modified
    );
    // a submitted instance gets exactly the catalog's groups and becomes its latest instance
    let (_, harness) = harness_tool(&[&a, &b], &admin).await?;
    let instance =
        generators::gen_workflow_instance(&grown, 1, None, &[(&triage, &[&tool, &harness])]);
    let r1 = submit(&europa.client, &instance).await?;
    let stored = admin.entities.get(r1.id).await?;
    assert_eq!(
        sorted_groups(&stored.groups),
        sorted_groups(&created.groups)
    );
    assert_eq!(stored.submitter, europa.username);
    assert_eq!(
        meta(&europa.client.entities.get(created.id).await?).latest_instance,
        Some(r1.id)
    );
    println!(
        "stored rows of {}: {:?}",
        created.id,
        test_utilities::entity_rows(created.id).await?
    );
    Ok(())
}

/// `Orchestrates` edges only reach agents that cover the catalog's groups, and a refused create
/// writes nothing
#[tokio::test]
// one scenario walks every refusal against shared fixtures
#[allow(clippy::too_many_lines)]
async fn orchestrates_refusals() -> Result<(), Error> {
    // europa edits minerva and mars
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (tool_root, _) = agentic_tool(&[&a, &b], &europa.client).await?;
    let tool_set = europa
        .client
        .entities
        .create(generators::gen_toolset(
            "kit",
            &[&a, &b],
            ToolSetEntityRequest::default(),
        ))
        .await?;
    let other = workflow(
        &europa.client,
        "other",
        &[&a, &b],
        WorkflowCatalogEntityRequest::default(),
    )
    .await?;
    let narrow = agent(&europa.client, "narrow", &[&a], |req| req).await?;
    let wide = agent(&europa.client, "wide", &[&a, &b], |req| req).await?;
    let before = listed(
        &admin,
        &[a.clone(), b.clone()],
        EntityKinds::WorkflowCatalog,
    )
    .await?;
    // naming a tool, a toolset, or another workflow is refused
    for target in [tool_root.id, tool_set.id, other.id] {
        let req = WorkflowCatalogEntityRequest::default()
            .agent(wide.id)
            .agent(target);
        let (status, msg) = refusal(
            europa
                .client
                .entities
                .create(generators::gen_workflow("bad", &[&a, &b], req))
                .await,
        );
        assert_eq!(status, 400, "{msg}");
        assert!(msg.contains("workflow_agents"), "{msg}");
    }
    // an agent missing one of the catalog's groups is refused, naming the group
    let req = WorkflowCatalogEntityRequest::default()
        .agent(wide.id)
        .agent(narrow.id);
    let (status, msg) = refusal(
        europa
            .client
            .entities
            .create(generators::gen_workflow("bad", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains(&b) && msg.contains("narrow"), "{msg}");
    // a missing agent is refused
    let req = WorkflowCatalogEntityRequest::default().agent(Uuid::new_v4());
    let (status, _) = refusal(
        europa
            .client
            .entities
            .create(generators::gen_workflow("bad", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    // nothing was written by any refusal
    assert_eq!(
        listed(
            &admin,
            &[a.clone(), b.clone()],
            EntityKinds::WorkflowCatalog
        )
        .await?,
        before
    );
    assert_eq!(
        edges(
            &admin,
            wide.id,
            AssociationKind::Orchestrates,
            Directionality::From
        )
        .await?,
        Vec::<Uuid>::new()
    );
    assert_eq!(
        sorted_groups(&admin.entities.get(narrow.id).await?.groups),
        vec![a.clone()]
    );
    // with extend_target_groups the narrow agent gains the missing group instead
    let req = WorkflowCatalogEntityRequest::default()
        .agent(narrow.id)
        .extend_target_groups();
    let widened = workflow(&europa.client, "widening", &[&a, &b], req).await?;
    assert_eq!(ids(&meta(&widened).agents), vec![narrow.id]);
    assert_eq!(
        sorted_groups(&admin.entities.get(narrow.id).await?.groups),
        sorted_groups(&[a.clone(), b.clone()])
    );
    // an update adding a tool is refused the same way, and removing an agent drops its edge
    let add_tool = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { add_agents, .. } = update {
            add_agents.push(tool_root.id);
        }
    });
    let (status, msg) = refusal(update_metadata(&europa.client, widened.id, add_tool).await);
    assert_eq!(status, 400, "{msg}");
    let remove = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(narrow.id);
        }
    });
    update_metadata(&europa.client, widened.id, remove).await?;
    assert_eq!(
        meta(&europa.client.entities.get(widened.id).await?)
            .agents
            .len(),
        0
    );
    // the generic association route checks Orchestrates edges the same way
    let (status, msg) = refusal(
        europa
            .client
            .associations
            .create(
                &thorium::models::AssociationRequest::new(
                    AssociationKind::Orchestrates,
                    thorium::models::AssociationTarget::Entity {
                        id: widened.id,
                        name: widened.name.clone(),
                    },
                )
                .target(thorium::models::AssociationTarget::Entity {
                    id: tool_root.id,
                    name: tool_root.name.clone(),
                }),
            )
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    Ok(())
}

/// The entry agent must be one of the workflow's agents after the request, and losing it later
/// is accepted
#[tokio::test]
// one scenario walks every entry agent rule against shared fixtures
#[allow(clippy::too_many_lines)]
async fn entry_agent_rules() -> Result<(), Error> {
    // agents a and b in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a_group = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let first = agent(&admin, "first", &[&a_group], |req| req).await?;
    let second = agent(&admin, "second", &[&a_group], |req| req).await?;
    let before = listed(
        &admin,
        std::slice::from_ref(&a_group),
        EntityKinds::WorkflowCatalog,
    )
    .await?;
    // a create naming an entry agent outside its agents writes nothing
    let req = WorkflowCatalogEntityRequest::default()
        .agent(first.id)
        .entry_agent(second.id);
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_workflow("entry", &[&a_group], req))
            .await,
    );
    assert_eq!(status, 400);
    assert_eq!(
        msg,
        format!(
            "entry agent {} is not one of workflow entry's agents",
            second.id
        )
    );
    assert_eq!(
        listed(
            &admin,
            std::slice::from_ref(&a_group),
            EntityKinds::WorkflowCatalog
        )
        .await?,
        before
    );
    assert_eq!(
        edges(
            &admin,
            first.id,
            AssociationKind::Orchestrates,
            Directionality::From
        )
        .await?,
        Vec::<Uuid>::new()
    );
    // a workflow whose entry agent is its only agent
    let req = WorkflowCatalogEntityRequest::default()
        .agent(first.id)
        .entry_agent(first.id);
    let created = workflow(&admin, "entry", &[&a_group], req).await?;
    // setting an entry agent that isn't one of its agents is refused and changes nothing
    let set_second = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { entry_agent, .. } = update {
            *entry_agent = Some(second.id);
        }
    });
    let (status, _) = refusal(update_metadata(&admin, created.id, set_second).await);
    assert_eq!(status, 400);
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).entry_agent,
        Some(first.id)
    );
    // adding the agent and making it the entry agent in one request works
    let add_and_set = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            entry_agent,
            add_agents,
            ..
        } = update
        {
            *entry_agent = Some(second.id);
            add_agents.push(second.id);
        }
    });
    update_metadata(&admin, created.id, add_and_set).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).entry_agent, Some(second.id));
    assert_eq!(ids(&meta(&read).agents), sorted(&[first.id, second.id]));
    // removing an agent while making it the entry agent is refused
    let remove_and_set = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            entry_agent,
            remove_agents,
            ..
        } = update
        {
            *entry_agent = Some(first.id);
            remove_agents.push(first.id);
        }
    });
    let (status, _) = refusal(update_metadata(&admin, created.id, remove_and_set).await);
    assert_eq!(status, 400);
    // setting and clearing the entry agent at once is refused
    let both = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            entry_agent,
            clear_entry_agent,
            ..
        } = update
        {
            *entry_agent = Some(first.id);
            *clear_entry_agent = Some(true);
        }
    });
    let (status, _) = refusal(update_metadata(&admin, created.id, both).await);
    assert_eq!(status, 400);
    // removing only the entry agent's edge is accepted, and the entry agent stays set
    let remove_entry = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(second.id);
        }
    });
    update_metadata(&admin, created.id, remove_entry).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).entry_agent, Some(second.id));
    assert_eq!(ids(&meta(&read).agents), vec![first.id]);
    // clearing the entry agent is accepted
    let clear = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            clear_entry_agent, ..
        } = update
        {
            *clear_entry_agent = Some(true);
        }
    });
    update_metadata(&admin, created.id, clear).await?;
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).entry_agent,
        None
    );
    Ok(())
}

/// A default models update replaces the whole list or clears it, an update without it leaves
/// it alone, and limits, auto instancing, and model auth are replaced as sent
#[tokio::test]
async fn settings_updates() -> Result<(), Error> {
    // a workflow with two default models that europa can edit
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&europa.username),
    )
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .default_model(model("claude-sonnet"))
        .default_model(model("gpt-5"));
    let created = workflow(&admin, "models", &[&a], req).await?;
    let seen = changes(&admin, created.id).await?.len();
    let names = |entity: &Entity| {
        meta(entity)
            .default_models
            .iter()
            .map(|matcher| matcher.model_id.clone())
            .collect::<Vec<String>>()
    };
    // a list replaces the whole list
    let replace = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { default_models, .. } = update {
            *default_models = Some(vec![model("gemini")]);
        }
    });
    update_metadata(&admin, created.id, replace).await?;
    assert_eq!(
        names(&admin.entities.get(created.id).await?),
        vec!["gemini"]
    );
    // an update without the list leaves it alone, while it changes the other settings
    let settings = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            limits,
            auto_instance,
            model_auth,
            ..
        } = update
        {
            *limits = Some(WorkflowLimits {
                max_alive_sub_agents: Some(2),
            });
            *auto_instance = Some(false);
            *model_auth = Some(ModelAuthSource {
                levels: [ModelAuthLevel::Group].into(),
            });
        }
    });
    update_metadata(&admin, created.id, settings).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(names(&read), vec!["gemini"]);
    // the replaced list and the changed settings each queued one update
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 2);
    assert_eq!(meta(&read).limits.max_alive_sub_agents, Some(2));
    assert!(!meta(&read).auto_instance);
    assert!(
        meta(&read)
            .model_auth
            .levels
            .contains(&ModelAuthLevel::Group)
    );
    // an empty list clears it, so the stored catalog hashes like one that never set it
    let clear = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { default_models, .. } = update {
            *default_models = Some(Vec::new());
        }
    });
    update_metadata(&admin, created.id, clear).await?;
    let cleared = admin.entities.get(created.id).await?;
    assert_eq!(meta(&cleared).default_models.len(), 0);
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 3);
    // an update without the list or any other graph setting queues nothing
    update_metadata(&admin, created.id, workflow_update(|_| ())).await?;
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 3);
    let never = workflow(
        &admin,
        "never",
        &[&a],
        WorkflowCatalogEntityRequest::default(),
    )
    .await?;
    let mut first = generators::gen_workflow_instance(&cleared, 1, None, &[]);
    first.limits = WorkflowLimits::default();
    let second = generators::gen_workflow_instance(&never, 1, None, &[]);
    assert_eq!(
        first.compute_digest(&BTreeMap::new())?,
        second.compute_digest(&BTreeMap::new())?
    );
    // a default models field that isn't a list of models is refused and changes nothing
    let fields = [("metadata[workflow_default_models]", "not json".to_owned())];
    let (status, body) = raw_form(&europa, Some(created.id), &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("workflow_default_models"), "{body}");
    assert_eq!(
        meta(&admin.entities.get(created.id).await?)
            .default_models
            .len(),
        0
    );
    Ok(())
}

/// A submitted Ready instance must run its entry agent in a Harness instance and bind each of
/// every agent's tools exactly once
#[tokio::test]
async fn submitted_ready_bindings_complete() -> Result<(), Error> {
    // ganymede edits in juno, which holds two MCP tools and a harness
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let roles = GroupRoles::default().user(&ganymede.username);
    let juno = god_group(&admin, "juno", &suffix, &roles).await?;
    let (scan_root, scan) = agentic_tool(&[&juno], &ganymede.client).await?;
    let (strings_root, _) = agentic_tool(&[&juno], &ganymede.client).await?;
    let (_, harness) = harness_tool(&[&juno], &ganymede.client).await?;
    let lead = agent(&ganymede.client, "lead", &[&juno], |req| {
        req.tool(scan_root.id)
            .tool(strings_root.id)
            .harness(harness.id)
    })
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(lead.id)
        .entry_agent(lead.id);
    let catalog = workflow(&ganymede.client, "Sweep", &[&juno], req).await?;
    let crawl = CrawlLimits::default();
    let resolved = resolve_workflow(&admin, catalog.id, &crawl).await?;
    let mut instance = resolved.instance.clone();
    assert!(instance.is_ready(), "{:?}", instance.status);
    instance.revision = 1;
    let harness_id = harness.id;
    // change the lead and redigest the instance
    let change = |edit: &dyn Fn(&mut InstancedAgent)| {
        let mut changed = instance.clone();
        let agent = changed
            .agents
            .iter_mut()
            .find(|agent| agent.agent == lead.id)
            .unwrap();
        edit(agent);
        changed.digest = changed.compute_digest(&BTreeMap::new()).unwrap();
        changed
    };
    // a Ready instance leaving one of the lead's tools unbound is refused
    let partial = change(&|agent| {
        agent
            .bindings
            .retain(|binding| binding.tool_instance != scan.id);
    });
    let (status, msg) = refusal(submit(&ganymede.client, &partial).await);
    assert_eq!(status, 400, "{msg}");
    assert!(
        msg.contains("doesn't bind") && msg.contains(&scan.id.to_string()),
        "{msg}"
    );
    // a Ready instance without any bindings is refused
    let unbound = change(&|agent| agent.bindings.clear());
    let (status, msg) = refusal(submit(&ganymede.client, &unbound).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("doesn't bind"), "{msg}");
    // a tool bound twice is refused
    let twice = change(&|agent| {
        let first = agent.bindings[0];
        agent.bindings.push(first);
    });
    let (status, msg) = refusal(submit(&ganymede.client, &twice).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("more than once"), "{msg}");
    // move the lead out of its harness, dropping the harness from the instance's tools
    let rehouse = |harness: Option<Uuid>| {
        let mut changed = change(&|agent| {
            agent.harness = harness;
            agent.bindings.clear();
        });
        changed
            .tools
            .retain(|tool| tool.tool_instance != harness_id);
        changed.digest = changed.compute_digest(&BTreeMap::new()).unwrap();
        changed
    };
    // a Ready instance whose entry agent runs in no harness is refused
    let homeless = rehouse(None);
    let (status, msg) = refusal(submit(&ganymede.client, &homeless).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("no Harness instance"), "{msg}");
    // a Ready instance whose entry agent's harness isn't a Harness instance is refused
    let misplaced = rehouse(Some(scan.id));
    let (status, msg) = refusal(submit(&ganymede.client, &misplaced).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("no Harness instance"), "{msg}");
    // the same instance marked Invalid needs no bindings
    let mut invalid = change(&|agent| agent.bindings.clear());
    invalid.status = WorkflowInstanceStatus::Invalid {
        errors: vec!["forced".to_owned()],
    };
    invalid.digest = invalid.compute_digest(&BTreeMap::new()).unwrap();
    submit(&ganymede.client, &invalid).await?;
    Ok(())
}

/// Submitted bindings must name each agent's own tools and mechanisms the entry agent's harness
/// instance can carry them out with, and the bindings the resolver produces are accepted
#[tokio::test]
// one scenario walks every binding rule against shared fixtures
#[allow(clippy::too_many_lines)]
async fn submitted_bindings() -> Result<(), Error> {
    // io edits in minerva, which holds an MCP tool, a native tool, and two harnesses, only one
    // of which provides the native tool
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (scan_root, scan) = agentic_tool(&[&minerva], &io.client).await?;
    let (native_root, native) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_native_spec(),
        &io.client,
    )
    .await?;
    let meta = thorium::models::ToolEntityRequest::new("1.0.0", &generators::gen_harness_spec())?
        .provides(vec![native.id]);
    let created = io
        .client
        .entities
        .create(thorium::models::EntityRequest::new(
            "pal-harness",
            thorium::models::EntityMetadataRequest::Tool(meta),
            vec![minerva.clone()],
        ))
        .await?;
    let providing = created.tool_instance.unwrap();
    let (_, plain) = harness_tool(&[&minerva], &io.client).await?;
    // a lead in the providing harness calls both tools
    let lead = agent(&io.client, "lead", &[&minerva], |req| {
        req.tool(scan_root.id)
            .tool(native_root.id)
            .harness(providing)
    })
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(lead.id)
        .entry_agent(lead.id);
    let catalog = workflow(&io.client, "Triage", &[&minerva], req).await?;
    // the resolver binds the MCP tool to ThoriumApi and the native tool to Native
    let crawl = CrawlLimits::default();
    let resolved = resolve_workflow(&admin, catalog.id, &crawl).await?;
    let mut instance = resolved.instance.clone();
    assert!(instance.is_ready(), "{:?}", instance.status);
    let bindings = instance.agent(lead.id).unwrap().bindings.clone();
    let mechanism_of = |id: Uuid| {
        bindings
            .iter()
            .find(|binding| binding.tool_instance == id)
            .map(|binding| binding.mechanism)
    };
    assert_eq!(mechanism_of(scan.id), Some(ToolMechanism::ThoriumApi));
    assert_eq!(mechanism_of(native.id), Some(ToolMechanism::Native));
    instance.revision = 1;
    // change the lead's bindings and redigest the instance
    let rebind = |change: &dyn Fn(&mut Vec<InstancedToolBinding>)| {
        let mut changed = instance.clone();
        let agent = changed
            .agents
            .iter_mut()
            .find(|agent| agent.agent == lead.id)
            .unwrap();
        change(&mut agent.bindings);
        changed.digest = changed.compute_digest(&BTreeMap::new()).unwrap();
        changed
    };
    // a binding to a tool that isn't one of the agent's tools is refused
    let stranger = rebind(&|bindings| {
        bindings.push(InstancedToolBinding {
            tool_instance: providing,
            mechanism: ToolMechanism::Cli,
        });
    });
    let (status, msg) = refusal(submit(&io.client, &stranger).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("isn't one of its tools"), "{msg}");
    // a binding to a mechanism the harness can't carry the tool out with is refused
    let unsupported = rebind(&|bindings| {
        for binding in bindings.iter_mut() {
            if binding.tool_instance == scan.id {
                binding.mechanism = ToolMechanism::Native;
            }
        }
    });
    let (status, msg) = refusal(submit(&io.client, &unsupported).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("can't be bound to Native"), "{msg}");
    // a native tool bound to Native in a harness that doesn't provide it is refused
    let plain_lead = agent(&io.client, "plain-lead", &[&minerva], |req| {
        req.tool(native.id).harness(plain.id)
    })
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(plain_lead.id)
        .entry_agent(plain_lead.id);
    let plain_catalog = workflow(&io.client, "Plain", &[&minerva], req).await?;
    let unprovided = resolve_workflow(&admin, plain_catalog.id, &crawl).await?;
    assert!(!unprovided.instance.is_ready());
    let mut forced = unprovided.instance.clone();
    forced.revision = 1;
    forced.status = WorkflowInstanceStatus::Ready;
    forced
        .agents
        .iter_mut()
        .find(|agent| agent.agent == plain_lead.id)
        .unwrap()
        .bindings = vec![InstancedToolBinding {
        tool_instance: native.id,
        mechanism: ToolMechanism::Native,
    }];
    forced.digest = forced.compute_digest(&BTreeMap::new()).unwrap();
    let (status, msg) = refusal(submit(&io.client, &forced).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("isn't provided by"), "{msg}");
    // the bindings the resolver produced are accepted
    let resp = submit(&io.client, &instance).await?;
    let stored = io.client.entities.get(resp.id).await?;
    assert_eq!(
        instance_meta(&stored).agent(lead.id).unwrap().bindings,
        bindings
    );
    Ok(())
}

/// A valid submitted instance is created with its name and intrinsic tags, takes only name and
/// description edits, and keeps its digest through renames
#[tokio::test]
// one scenario walks the instance's life against shared fixtures
#[allow(clippy::too_many_lines)]
async fn submitted_instances() -> Result<(), Error> {
    // a tool followed at latest, a tool pinned, and a toolset followed at latest with one member
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let (latest_root, latest) = agentic_tool(&[&a], &admin).await?;
    let (pinned_root, pinned) = agentic_tool(&[&a], &admin).await?;
    let (member_root, member) = agentic_tool(&[&a], &admin).await?;
    let set_req = ToolSetEntityRequest::default().tool(member_root.id);
    let set_resp = admin
        .entities
        .create(generators::gen_toolset("kit", &[&a], set_req))
        .await?;
    let tool_set = admin.entities.get(set_resp.id).await?;
    let set_instance = generators::gen_toolset_instance(&tool_set, 1, None, &[&member]);
    let set_r1 = admin
        .entities
        .create(generators::gen_toolset_instance_req(&set_instance))
        .await?;
    // the entry agent calls all three and delegates to a sub-agent the catalog doesn't name
    let sub = agent(&admin, "sub", &[&a], |req| req).await?;
    let entry = agent(&admin, "entry", &[&a], |req| {
        req.tool(latest_root.id)
            .tool(pinned.id)
            .tool_set(tool_set.id)
            .sub_agent(sub.id)
    })
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(entry.id)
        .entry_agent(entry.id);
    let created = workflow(&io.client, "Triage", &[&a], req).await?;
    // an instance of both agents, with the pinned tool marked and the toolset's member
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let mut instance = generators::gen_workflow_instance(
        &created,
        1,
        None,
        &[
            (&entry, &[&latest, &pinned, &member, &harness]),
            (&sub, &[]),
        ],
    );
    for tool in &mut instance.tools {
        tool.pinned = tool.tool_instance == pinned.id;
    }
    instance.agents[0].tool_sets.push(set_r1.id);
    instance.agents[0].sub_agents.push(sub.id);
    let digests = BTreeMap::from([(set_r1.id, set_instance.digest.clone())]);
    refresh_digests(&mut instance, &digests);
    let seen = changes(&admin, created.id).await?.len();
    let r1 = submit(&io.client, &instance).await?;
    // it's named after the workflow and revision and tagged with what it uses
    assert_eq!(r1.name, "Triage#r1");
    let stored = admin.entities.get(r1.id).await?;
    assert_eq!(
        tag_values(&stored, "WorkflowId"),
        vec![created.id.to_string()]
    );
    assert_eq!(
        tag_values(&stored, "WorkflowRevision"),
        vec!["1".to_owned()]
    );
    assert_eq!(
        tag_values(&stored, "WorkflowDigest"),
        vec![instance.digest.clone()]
    );
    assert_eq!(
        tag_values(&stored, "WorkflowStatus"),
        vec!["Ready".to_owned()]
    );
    let mut agents = vec![entry.id.to_string(), sub.id.to_string()];
    agents.sort();
    assert_eq!(tag_values(&stored, "InstanceAgent"), agents);
    assert_eq!(
        tag_values(&stored, "InstanceFollowsTool"),
        vec![latest_root.id.to_string()]
    );
    assert_eq!(
        tag_values(&stored, "InstanceFollowsToolSet"),
        vec![tool_set.id.to_string()]
    );
    assert!(!tag_values(&stored, "InstanceFollowsTool").contains(&pinned_root.id.to_string()));
    assert!(!tag_values(&stored, "InstanceFollowsTool").contains(&member_root.id.to_string()));
    // it's stored as sent, with its triggered_by naming the submitter
    let read = instance_meta(&stored);
    assert_eq!(read.digest, instance.digest);
    assert_eq!(read.triggered_by, io.username);
    assert_eq!(read.agents.len(), 2);
    // an invalid instance carries the same tags
    let mut invalid = instance.clone();
    invalid.revision = 2;
    invalid.previous = Some(r1.id);
    invalid.status = WorkflowInstanceStatus::Invalid {
        errors: vec!["tool decompile has no latest instance".into()],
    };
    let r2 = submit(&io.client, &invalid).await?;
    let stored_r2 = admin.entities.get(r2.id).await?;
    assert_eq!(
        tag_values(&stored_r2, "WorkflowStatus"),
        vec!["Invalid".to_owned()]
    );
    for key in [
        "InstanceAgent",
        "InstanceFollowsTool",
        "InstanceFollowsToolSet",
    ] {
        assert_eq!(
            tag_values(&stored_r2, key),
            tag_values(&stored, key),
            "{key}"
        );
    }
    // a metadata field can't be patched
    let set_limits = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { limits, .. } = update {
            *limits = Some(WorkflowLimits::default());
        }
    });
    let (status, msg) = refusal(update_metadata(&io.client, r1.id, set_limits).await);
    assert_eq!(status, 400);
    assert!(msg.contains("metadata can't change"), "{msg}");
    // its groups follow its workflow
    let b = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let (status, _) = refusal(
        admin
            .entities
            .update(r1.id, EntityUpdate::default().group(&b))
            .await,
    );
    assert_eq!(status, 400);
    // its name and description can change without touching its metadata or digest
    io.client
        .entities
        .update(
            r1.id,
            EntityUpdate::default()
                .name("Triage first build")
                .description("the first build"),
        )
        .await?;
    let renamed = admin.entities.get(r1.id).await?;
    assert_eq!(renamed.name, "Triage first build");
    assert_eq!(renamed.description, Some("the first build".to_owned()));
    assert_eq!(instance_meta(&renamed), instance_meta(&stored));
    assert_eq!(
        tag_values(&renamed, "WorkflowDigest"),
        vec![instance.digest.clone()]
    );
    // renaming the catalog changes no digest and creates no instance
    io.client
        .entities
        .update(created.id, EntityUpdate::default().name("Triage renamed"))
        .await?;
    assert_eq!(
        listed(
            &admin,
            std::slice::from_ref(&a),
            EntityKinds::WorkflowInstance
        )
        .await?,
        sorted(&[r1.id, r2.id])
    );
    assert_eq!(
        instance_meta(&admin.entities.get(r1.id).await?).digest,
        instance.digest
    );
    // creating and renaming the instances and renaming the catalog queued nothing
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 0);
    assert_eq!(changes(&admin, r1.id).await?.len(), 0);
    assert_eq!(changes(&admin, r2.id).await?.len(), 0);
    println!("stored tags of {}: {:?}", r1.id, stored.tags);
    Ok(())
}

/// A submitted instance that isn't JSON or isn't a workflow instance is refused and writes
/// nothing
#[tokio::test]
async fn malformed_instances() -> Result<(), Error> {
    // a workflow a user can submit instances of
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&callisto.username),
    )
    .await?;
    let created = workflow(
        &callisto.client,
        "malformed",
        &[&a],
        WorkflowCatalogEntityRequest::default(),
    )
    .await?;
    // an instance that isn't JSON is refused
    let fields = [
        ("kind", "WorkflowInstance".to_owned()),
        ("groups[]", a.clone()),
        ("metadata[workflow_instance]", "not json".to_owned()),
    ];
    let (status, body) = raw_form(&callisto, None, &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("workflow_instance"), "{body}");
    // JSON without a digest is refused
    let instance = generators::gen_workflow_instance(&created, 1, None, &[]);
    let mut value = serde_json::to_value(&instance)?;
    value.as_object_mut().unwrap().remove("digest");
    let fields = [
        ("kind", "WorkflowInstance".to_owned()),
        ("groups[]", a.clone()),
        ("metadata[workflow_instance]", value.to_string()),
    ];
    let (status, body) = raw_form(&callisto, None, &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("digest"), "{body}");
    // a form without the instance is refused
    let fields = [
        ("kind", "WorkflowInstance".to_owned()),
        ("groups[]", a.clone()),
    ];
    let (status, _) = raw_form(&callisto, None, &fields).await?;
    assert_eq!(status, 400);
    // an instance with more agents than any workflow resolves to is refused before it's read
    let mut oversized = generators::gen_workflow_instance(&created, 1, None, &[]);
    oversized.agents = (0..=CrawlLimits::default().max_agents_per_workflow)
        .map(|index| thorium::models::InstancedAgent {
            agent: Uuid::new_v4(),
            name: format!("agent-{index}"),
            groups: vec![a.clone()],
            config: thorium::models::AgentConfig::default(),
            config_hash: String::new(),
            harness: None,
            tools: Vec::new(),
            tool_sets: Vec::new(),
            bindings: Vec::new(),
            sub_agents: Vec::new(),
            toolset_digest: String::new(),
        })
        .collect();
    let (status, msg) = refusal(submit(&callisto.client, &oversized).await);
    assert_eq!(status, 400);
    assert!(
        msg.contains("at most max_agents_per_workflow (32) agents, not 33"),
        "{msg}"
    );
    // nothing was written
    assert_eq!(
        listed(
            &admin,
            std::slice::from_ref(&a),
            EntityKinds::WorkflowInstance
        )
        .await?,
        Vec::<Uuid>::new()
    );
    Ok(())
}

/// Submitted instances are refused when the submitter can't edit the workflow everywhere,
/// names someone else, or sends references, hashes, groups, or a digest that don't match what's
/// stored
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn submitted_instance_checks() -> Result<(), Error> {
    // europa edits minerva and mars, ganymede edits only minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .user(&ganymede.username),
    )
    .await?;
    let b = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .monitor(&ganymede.username),
    )
    .await?;
    let hidden = god_group(&admin, "vulcan", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a, &b], &admin).await?;
    let triage = agent(&admin, "triage", &[&a, &b], |req| req.tool(tool.id)).await?;
    let narrow = agent(&admin, "narrow", &[&a], |req| req).await?;
    let secret = agent(&admin, "secret", &[&hidden], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(triage.id)
        .entry_agent(triage.id);
    let created = workflow(&admin, "checked", &[&a, &b], req).await?;
    let (_, harness) = harness_tool(&[&a, &b], &admin).await?;
    let good =
        || generators::gen_workflow_instance(&created, 1, None, &[(&triage, &[&tool, &harness])]);
    // a user who can't edit the workflow in every group is refused
    let mut first = good();
    first.triggered_by = ganymede.username.clone();
    let (status, _) = refusal(submit(&ganymede.client, &first).await);
    assert_eq!(status, 401);
    // a user naming someone else in triggered_by is refused
    let (status, msg) = refusal(submit(&europa.client, &first).await);
    assert_eq!(status, 400);
    assert!(msg.contains("triggered_by"), "{msg}");
    // a wrong digest is refused
    let mut wrong_digest = good();
    wrong_digest.digest = format!("v1:{}", "0".repeat(64));
    let (status, msg) = refusal(submit(&europa.client, &wrong_digest).await);
    assert_eq!(status, 400);
    assert!(msg.contains("digest"), "{msg}");
    // a wrong spec hash or config hash is refused, even with digests that match them
    let mut wrong_spec = good();
    wrong_spec.tools[0].spec_hash = format!("v1:{}", "a".repeat(64));
    refresh_digests(&mut wrong_spec, &BTreeMap::new());
    let (status, msg) = refusal(submit(&europa.client, &wrong_spec).await);
    assert_eq!(status, 400);
    assert!(msg.contains("spec_hash"), "{msg}");
    let mut wrong_config = good();
    wrong_config.agents[0].config.max_turns = Some(99);
    wrong_config.agents[0].config_hash = wrong_config.agents[0].config.config_hash()?;
    refresh_digests(&mut wrong_config, &BTreeMap::new());
    let (status, msg) = refusal(submit(&europa.client, &wrong_config).await);
    assert_eq!(status, 400);
    assert!(msg.contains("config_hash"), "{msg}");
    let mut unhashed = good();
    unhashed.agents[0].config.max_turns = Some(99);
    let (status, msg) = refusal(submit(&europa.client, &unhashed).await);
    assert_eq!(status, 400);
    assert!(msg.contains("config"), "{msg}");
    // a missing or hidden agent or tool instance is a 404, and a visible non-agent is a 400
    for id in [Uuid::new_v4(), secret.id] {
        let mut missing = good();
        missing.agents[0].agent = id;
        missing.entry_agent = Some(id);
        let (status, msg) = refusal(submit(&europa.client, &missing).await);
        assert_eq!(status, 404, "{msg}");
        assert!(!msg.contains(&hidden), "{msg}");
    }
    let mut missing_tool = good();
    let ghost = Uuid::new_v4();
    missing_tool.tools[0].tool_instance = ghost;
    missing_tool.agents[0].tools = vec![ghost];
    let (status, _) = refusal(submit(&europa.client, &missing_tool).await);
    assert_eq!(status, 404);
    let mut not_agent = good();
    not_agent.agents[0].agent = tool.id;
    not_agent.entry_agent = Some(tool.id);
    let (status, msg) = refusal(submit(&europa.client, &not_agent).await);
    assert_eq!(status, 400);
    assert!(msg.contains("is not an agent"), "{msg}");
    // references inside the instance must point at its own agents and tools
    let mut dangling_tool = good();
    dangling_tool.agents[0].tools.push(Uuid::new_v4());
    let (status, _) = refusal(submit(&europa.client, &dangling_tool).await);
    assert_eq!(status, 400);
    let mut dangling_sub = good();
    dangling_sub.agents[0].sub_agents.push(narrow.id);
    let (status, _) = refusal(submit(&europa.client, &dangling_sub).await);
    assert_eq!(status, 400);
    let mut no_entry = good();
    no_entry.entry_agent = None;
    refresh_digests(&mut no_entry, &BTreeMap::new());
    let (status, msg) = refusal(submit(&europa.client, &no_entry).await);
    assert_eq!(status, 400);
    assert!(msg.contains("entry agent"), "{msg}");
    let mut wrong_toolset = good();
    wrong_toolset.agents[0].toolset_digest = format!("v1:{}", "b".repeat(64));
    let (status, msg) = refusal(submit(&europa.client, &wrong_toolset).await);
    assert_eq!(status, 400);
    assert!(msg.contains("toolset_digest"), "{msg}");
    // recorded groups that differ from the stored ones are refused
    let mut wrong_workflow_groups = good();
    wrong_workflow_groups.workflow_groups = vec![a.clone()];
    let (status, msg) = refusal(submit(&europa.client, &wrong_workflow_groups).await);
    assert_eq!(status, 400);
    assert!(msg.contains("checked"), "{msg}");
    let mut wrong_agent_groups = good();
    wrong_agent_groups.agents[0].groups = vec![a.clone()];
    let (status, _) = refusal(submit(&europa.client, &wrong_agent_groups).await);
    assert_eq!(status, 400);
    let mut wrong_tool_groups = good();
    wrong_tool_groups.tools[0].groups = vec![b.clone()];
    let (status, _) = refusal(submit(&europa.client, &wrong_tool_groups).await);
    assert_eq!(status, 400);
    // an agent that doesn't cover the workflow's groups breaks containment
    let mut uncontained =
        generators::gen_workflow_instance(&created, 1, None, &[(&narrow, &[&harness])]);
    uncontained.triggered_by = europa.username.clone();
    let (status, msg) = refusal(submit(&europa.client, &uncontained).await);
    assert_eq!(status, 400);
    assert!(msg.contains(&b), "{msg}");
    // a revision gap is a conflict
    let gap =
        generators::gen_workflow_instance(&created, 2, None, &[(&triage, &[&tool, &harness])]);
    let (status, _) = refusal(submit(&europa.client, &gap).await);
    assert_eq!(status, 409);
    // nothing was written by any refusal
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).latest_instance,
        None
    );
    // a user naming themselves is created
    let mut own = good();
    own.triggered_by = europa.username.clone();
    let first_id = submit(&europa.client, &own).await?.id;
    let stored = admin.entities.get(first_id).await?;
    assert_eq!(instance_meta(&stored).triggered_by, europa.username);
    // an admin may name anyone
    let mut by_admin = generators::gen_workflow_instance(
        &created,
        2,
        Some(first_id),
        &[(&triage, &[&tool, &harness])],
    );
    by_admin.triggered_by = ganymede.username.clone();
    let second_id = submit(&admin, &by_admin).await?.id;
    let stored = admin.entities.get(second_id).await?;
    assert_eq!(instance_meta(&stored).triggered_by, ganymede.username);
    assert_eq!(stored.submitter, "thorium");
    Ok(())
}

/// A toolset instance an agent was given must be ready and its members must be among the
/// agent's tools
#[tokio::test]
async fn submitted_toolset_references() -> Result<(), Error> {
    // a toolset with a ready and an invalid instance and an agent that calls it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (member_root, member) = agentic_tool(&[&a], &admin).await?;
    let (_, other) = agentic_tool(&[&a], &admin).await?;
    let set_req = ToolSetEntityRequest::default().tool(member_root.id);
    let set_resp = admin
        .entities
        .create(generators::gen_toolset("kit", &[&a], set_req))
        .await?;
    let tool_set = admin.entities.get(set_resp.id).await?;
    let ready = generators::gen_toolset_instance(&tool_set, 1, None, &[&member]);
    let ready_id = admin
        .entities
        .create(generators::gen_toolset_instance_req(&ready))
        .await?
        .id;
    let mut invalid = generators::gen_toolset_instance(&tool_set, 2, Some(ready_id), &[&member]);
    invalid.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["broken".into()],
    };
    let invalid_id = admin
        .entities
        .create(generators::gen_toolset_instance_req(&invalid))
        .await?
        .id;
    let caller = agent(&admin, "caller", &[&a], |req| req.tool_set(tool_set.id)).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(caller.id)
        .entry_agent(caller.id);
    let created = workflow(&admin, "kits", &[&a], req).await?;
    // an invalid toolset instance is refused
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let mut with_invalid =
        generators::gen_workflow_instance(&created, 1, None, &[(&caller, &[&member, &harness])]);
    with_invalid.agents[0].tool_sets.push(invalid_id);
    refresh_digests(
        &mut with_invalid,
        &BTreeMap::from([(invalid_id, invalid.digest.clone())]),
    );
    let (status, msg) = refusal(submit(&admin, &with_invalid).await);
    assert_eq!(status, 400);
    assert!(msg.contains("Ready"), "{msg}");
    // a member the agent wasn't given is refused
    let mut missing_member =
        generators::gen_workflow_instance(&created, 1, None, &[(&caller, &[&other, &harness])]);
    missing_member.agents[0].tool_sets.push(ready_id);
    refresh_digests(
        &mut missing_member,
        &BTreeMap::from([(ready_id, ready.digest.clone())]),
    );
    let (status, msg) = refusal(submit(&admin, &missing_member).await);
    assert_eq!(status, 400);
    assert!(msg.contains("member"), "{msg}");
    // a ready instance whose members the agent has is created
    let mut good =
        generators::gen_workflow_instance(&created, 1, None, &[(&caller, &[&member, &harness])]);
    good.agents[0].tool_sets.push(ready_id);
    refresh_digests(
        &mut good,
        &BTreeMap::from([(ready_id, ready.digest.clone())]),
    );
    submit(&admin, &good).await?;
    Ok(())
}

/// Revisions run r1, r2, r3 with each `previous` naming the one before, gaps and repeats are
/// conflicts that write nothing, and `latest_instance` is the newest ready instance
#[tokio::test]
async fn revisions_and_latest() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "versions", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let build = |revision, previous| {
        generators::gen_workflow_instance(&created, revision, previous, &[(&solo, &[&harness])])
    };
    // a first instance must be revision 1, and the refusal writes nothing
    let (status, _) = refusal(submit(&admin, &build(2, None)).await);
    assert_eq!(status, 409);
    assert_eq!(
        edges(
            &admin,
            created.id,
            AssociationKind::BuiltFrom,
            Directionality::From
        )
        .await?,
        Vec::<Uuid>::new()
    );
    // submit r1, r2, and an invalid r3
    let r1 = submit(&admin, &build(1, None)).await?;
    let r2 = submit(&admin, &build(2, Some(r1.id))).await?;
    // a second r2 is a conflict, and an r3 that skips r2 as its previous is refused
    let (status, _) = refusal(submit(&admin, &build(2, Some(r1.id))).await);
    assert_eq!(status, 409);
    let (status, _) = refusal(submit(&admin, &build(3, Some(r1.id))).await);
    assert_eq!(status, 400);
    let mut invalid = build(3, Some(r2.id));
    invalid.status = WorkflowInstanceStatus::Invalid {
        errors: vec!["no harness".into()],
    };
    let r3 = submit(&admin, &invalid).await?;
    // the names and previous links follow the revisions
    assert_eq!(
        [r1.name.as_str(), r2.name.as_str(), r3.name.as_str()],
        ["versions#r1", "versions#r2", "versions#r3"]
    );
    let mut previous = None;
    for id in [r1.id, r2.id, r3.id] {
        let entity = admin.entities.get(id).await?;
        assert_eq!(instance_meta(&entity).previous, previous);
        previous = Some(id);
    }
    // the newest ready instance is r2, and each instance is linked to its workflow
    assert_eq!(
        meta(&io.client.entities.get(created.id).await?).latest_instance,
        Some(r2.id)
    );
    assert_eq!(
        edges(
            &admin,
            created.id,
            AssociationKind::BuiltFrom,
            Directionality::From
        )
        .await?,
        sorted(&[r1.id, r2.id, r3.id])
    );
    // after r2 is deleted the latest is r1, and the next revision is still r4
    admin.entities.delete(r2.id).await?;
    assert_eq!(
        meta(&io.client.entities.get(created.id).await?).latest_instance,
        Some(r1.id)
    );
    let r4 = submit(&admin, &build(4, Some(r3.id))).await?;
    assert_eq!(r4.name, "versions#r4");
    Ok(())
}

/// Two submissions of the same revision at once create exactly one instance
#[tokio::test]
async fn concurrent_submissions() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "racing", &[&a], req).await?;
    // send two r1 submissions at once
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let instance = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]);
    let (left, right) = tokio::join!(submit(&admin, &instance), submit(&admin, &instance));
    // exactly one is created and the other is a conflict
    let results = [left, right];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in results {
        if let Err(err) = result {
            assert_eq!(err.status().map(|status| status.as_u16()), Some(409));
        }
    }
    // the next revision is r2
    let winner = meta(&admin.entities.get(created.id).await?)
        .latest_instance
        .unwrap();
    let next =
        generators::gen_workflow_instance(&created, 2, Some(winner), &[(&solo, &[&harness])]);
    assert_eq!(submit(&admin, &next).await?.name, "racing#r2");
    Ok(())
}

/// The revision lock is released after every submission, a crashed holder blocks only its own
/// workflow until it expires, and twenty racing submissions create one instance
#[tokio::test]
async fn revision_lock() -> Result<(), Error> {
    // two workflows with one agent each
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = || {
        WorkflowCatalogEntityRequest::default()
            .agent(solo.id)
            .entry_agent(solo.id)
    };
    let locked = workflow(&admin, "locked", &[&a], req()).await?;
    let free = workflow(&admin, "free", &[&a], req()).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let build = |workflow: &Entity, revision, previous| {
        generators::gen_workflow_instance(workflow, revision, previous, &[(&solo, &[&harness])])
    };
    // the lock is released after a created and a refused submission
    let r1 = submit(&admin, &build(&locked, 1, None)).await?;
    assert!(!test_utilities::instance_lock_exists(locked.id).await?);
    let mut wrong = build(&locked, 2, Some(r1.id));
    wrong.digest = format!("v1:{}", "1".repeat(64));
    let (status, _) = refusal(submit(&admin, &wrong).await);
    assert_eq!(status, 400);
    assert!(!test_utilities::instance_lock_exists(locked.id).await?);
    // a crashed holder's lock refuses r2 after about two seconds
    test_utilities::hold_instance_lock_for(locked.id, "crashed-holder", 3_000).await?;
    let r2 = build(&locked, 2, Some(r1.id));
    let start = Instant::now();
    let (status, msg) = refusal(submit(&admin, &r2).await);
    let waited = start.elapsed();
    assert_eq!(status, 409);
    assert_eq!(msg, "another instance of `locked` is being created; retry");
    assert!(waited >= Duration::from_millis(1_800), "{waited:?}");
    assert!(waited < Duration::from_millis(3_000), "{waited:?}");
    assert_eq!(
        meta(&admin.entities.get(locked.id).await?).latest_instance,
        Some(r1.id)
    );
    // once it expires the same submission is created
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let r2_id = submit(&admin, &r2).await?.id;
    // while one workflow's lock is held another workflow's submission doesn't wait
    test_utilities::hold_instance_lock(locked.id).await?;
    let start = Instant::now();
    submit(&admin, &build(&free, 1, None)).await?;
    assert!(
        start.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        start.elapsed()
    );
    test_utilities::release_instance_lock(locked.id).await?;
    // twenty racing submissions of r3 create exactly one instance
    let r3 = build(&locked, 3, Some(r2_id));
    let racing = (0..20).map(|_| submit(&admin, &r3));
    let results = futures::future::join_all(racing).await;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in &results {
        if let Err(err) = result {
            assert_eq!(err.status().map(|status| status.as_u16()), Some(409));
        }
    }
    // revisions stay contiguous, so r4 follows
    let winner = meta(&admin.entities.get(locked.id).await?)
        .latest_instance
        .unwrap();
    assert_eq!(admin.entities.get(winner).await?.name, "locked#r3");
    let r4 = build(&locked, 4, Some(winner));
    assert_eq!(submit(&admin, &r4).await?.name, "locked#r4");
    assert!(!test_utilities::instance_lock_exists(locked.id).await?);
    Ok(())
}

/// A user outside a workflow's groups can't read, list, or find it or its instances by tag
#[tokio::test]
async fn visibility() -> Result<(), Error> {
    // a workflow and an instance in minerva, a member and an outsider
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let elsewhere = god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default().user(&amalthea.username),
    )
    .await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "private", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let instance = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    // the outsider gets a 404 for both
    for id in [created.id, instance.id] {
        let (status, _) = refusal(amalthea.client.entities.get(id).await);
        assert_eq!(status, 404);
    }
    // neither is in the outsider's listings or tag listings
    for kind in [EntityKinds::WorkflowCatalog, EntityKinds::WorkflowInstance] {
        assert_eq!(
            listed(&amalthea.client, std::slice::from_ref(&elsewhere), kind).await?,
            Vec::<Uuid>::new()
        );
    }
    let tagged = EntityListOpts::default()
        .tag("WorkflowId", created.id.to_string())
        .limit(100);
    assert_eq!(amalthea.client.entities.list(&tagged).await?.data.len(), 0);
    // the member sees both, lists both, and finds the instance by tag
    io.client.entities.get(created.id).await?;
    io.client.entities.get(instance.id).await?;
    assert_eq!(
        listed(
            &io.client,
            std::slice::from_ref(&a),
            EntityKinds::WorkflowCatalog
        )
        .await?,
        vec![created.id]
    );
    assert_eq!(
        listed(
            &io.client,
            std::slice::from_ref(&a),
            EntityKinds::WorkflowInstance
        )
        .await?,
        vec![instance.id]
    );
    let found = io
        .client
        .entities
        .list(&tagged)
        .await?
        .data
        .into_iter()
        .map(|line| line.id)
        .collect::<Vec<Uuid>>();
    assert_eq!(found, vec![instance.id]);
    Ok(())
}

/// A reader is only shown the snapshot groups they can see, on every read path, while the
/// stored snapshot keeps them all
#[tokio::test]
// one scenario checks every read path against shared fixtures
#[allow(clippy::too_many_lines)]
async fn instance_snapshot_groups_filtered() -> Result<(), Error> {
    // ganymede is only in minerva, and the workflow, its agent, and its tool are in both
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&ganymede.username),
    )
    .await?;
    let mars = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&minerva, &mars], &admin).await?;
    let solo = agent(&admin, "solo", &[&minerva, &mars], |req| req.tool(tool.id)).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "snap", &[&minerva, &mars], req).await?;
    let (_, harness) = harness_tool(&[&minerva, &mars], &admin).await?;
    let r1 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&tool, &harness])]),
    )
    .await?;
    let only = vec![minerva.clone()];
    let check = |instance: &WorkflowInstanceEntity| {
        assert_eq!(instance.workflow_groups, only);
        assert_eq!(instance.agents[0].groups, only);
        assert_eq!(instance.tools[0].groups, only);
    };
    // ganymede's read names only minerva in every snapshot
    check(instance_meta(&ganymede.client.entities.get(r1.id).await?));
    // so does his details listing
    let opts = EntityListOpts::default()
        .groups(vec![minerva.clone()])
        .kind(EntityKinds::WorkflowInstance)
        .limit(100);
    let listed = ganymede.client.entities.list_details(&opts).await?;
    let found = listed
        .data
        .iter()
        .find(|entity| entity.id == r1.id)
        .expect("the instance is listed");
    check(instance_meta(found));
    // and so does the harness toolset read
    let toolset = ganymede.client.entities.toolset(r1.id, solo.id).await?;
    assert_eq!(toolset.agent.groups, only);
    assert_eq!(toolset.tools[0].tool.groups, only);
    // the admin sees every group, so the stored snapshot is whole
    let both = sorted_groups(&[minerva.clone(), mars.clone()]);
    let read = admin.entities.get(r1.id).await?;
    assert_eq!(sorted_groups(&instance_meta(&read).workflow_groups), both);
    assert_eq!(sorted_groups(&instance_meta(&read).agents[0].groups), both);
    assert_eq!(sorted_groups(&instance_meta(&read).tools[0].groups), both);
    let toolset = admin.entities.toolset(r1.id, solo.id).await?;
    assert_eq!(sorted_groups(&toolset.tools[0].tool.groups), both);
    Ok(())
}

/// Widening a tool doesn't rewrite an instance's snapshot, and a later submission must record
/// the tool's new groups
#[tokio::test]
async fn snapshots_not_rewritten_by_widening() -> Result<(), Error> {
    // a workflow in minerva whose agent calls a tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let c = god_group(&admin, "ceres", &suffix, &GroupRoles::default()).await?;
    let (tool_root, tool) = agentic_tool(&[&a], &admin).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req.tool(tool.id)).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "widening", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let stale =
        generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&tool, &harness])]);
    let r1 = submit(&admin, &stale).await?;
    // the tool gains ceres
    admin
        .entities
        .update(tool_root.id, EntityUpdate::default().group(&c))
        .await?;
    // r1's snapshot still records only minerva, read back and in its stored rows
    let read = admin.entities.get(r1.id).await?;
    assert_eq!(instance_meta(&read).tools[0].groups, vec![a.clone()]);
    for (group, data) in test_utilities::entity_rows(r1.id).await? {
        println!("stored row of {} in {group}: {data}", r1.id);
        let value: serde_json::Value = serde_json::from_str(&data)?;
        assert_eq!(
            value["WorkflowInstance"]["tools"][0]["groups"],
            json!([a.clone()])
        );
    }
    // a submission still recording the old groups is refused
    let mut old =
        generators::gen_workflow_instance(&created, 2, Some(r1.id), &[(&solo, &[&tool, &harness])]);
    old.tools[0].groups = vec![a.clone()];
    refresh_digests(&mut old, &BTreeMap::new());
    let (status, _) = refusal(submit(&admin, &old).await);
    assert_eq!(status, 400);
    // one recording the new groups is created
    let widened = admin.entities.get(tool.id).await?;
    let fresh = generators::gen_workflow_instance(
        &created,
        2,
        Some(r1.id),
        &[(&solo, &[&widened, &harness])],
    );
    assert_eq!(
        sorted_groups(&fresh.tools[0].groups),
        sorted_groups(&[a.clone(), c.clone()])
    );
    submit(&admin, &fresh).await?;
    Ok(())
}

/// The harness toolset read returns an agent's snapshot and the stored spec of each of its
/// tools, unchanged after a tool publishes a new version
#[tokio::test]
async fn toolset_read_route() -> Result<(), Error> {
    // an agent with an own tool and a toolset member, and an outsider only in vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default().user(&amalthea.username),
    )
    .await?;
    let (own_root, own) = agentic_tool(&[&a], &admin).await?;
    let (member_root, member) = agentic_tool(&[&a], &admin).await?;
    let set_req = ToolSetEntityRequest::default().tool(member_root.id);
    let set_resp = admin
        .entities
        .create(generators::gen_toolset("kit", &[&a], set_req))
        .await?;
    let tool_set = admin.entities.get(set_resp.id).await?;
    let set_instance = generators::gen_toolset_instance(&tool_set, 1, None, &[&member]);
    let set_r1 = admin
        .entities
        .create(generators::gen_toolset_instance_req(&set_instance))
        .await?;
    let solo = agent(&admin, "solo", &[&a], |req| {
        req.tool(own_root.id).tool_set(tool_set.id)
    })
    .await?;
    let other = agent(&admin, "other", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "harnessed", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let mut instance = generators::gen_workflow_instance(
        &created,
        1,
        None,
        &[(&solo, &[&own, &member, &harness])],
    );
    instance.agents[0].tool_sets.push(set_r1.id);
    refresh_digests(
        &mut instance,
        &BTreeMap::from([(set_r1.id, set_instance.digest.clone())]),
    );
    let r1 = submit(&admin, &instance).await?;
    // the agent's snapshot, stored with its lists in id order, and both tools' stored specs are
    // returned
    let read = io.client.entities.toolset(r1.id, solo.id).await?;
    let mut stored = instance.clone();
    stored.sort_lists();
    assert_eq!(read.agent, stored.agents[0]);
    let spec_of = |entity: &Entity| match &entity.metadata {
        EntityMetadata::ToolInstance(stored) => stored.spec.clone(),
        other => panic!("expected a tool instance, got {other:?}"),
    };
    let returned = read
        .tools
        .iter()
        .map(|tool| (tool.tool.tool_instance, tool.spec.clone()))
        .collect::<BTreeMap<_, _>>();
    let expected = BTreeMap::from([(own.id, spec_of(&own)), (member.id, spec_of(&member))]);
    assert_eq!(returned, expected);
    // a new version of the member tool changes nothing the instance returns
    admin
        .entities
        .update(
            member_root.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    assert_eq!(io.client.entities.toolset(r1.id, solo.id).await?, read);
    // an agent not in the instance, an id that isn't an instance, and an outsider are 404s
    let (status, _) = refusal(io.client.entities.toolset(r1.id, other.id).await);
    assert_eq!(status, 404);
    let (status, _) = refusal(io.client.entities.toolset(created.id, solo.id).await);
    assert_eq!(status, 404);
    let (status, _) = refusal(amalthea.client.entities.toolset(r1.id, solo.id).await);
    assert_eq!(status, 404);
    // a read without an agent is a 400
    let path = format!("/api/entities/workflows/instances/{}/toolset", r1.id);
    let (status, _) = raw_get_path(&io, &path).await?;
    assert_eq!(status, 400);
    let selected: Vec<&InstancedTool> = read.tools.iter().map(|tool| &tool.tool).collect();
    assert!(selected.iter().all(|tool| !tool.pinned));
    Ok(())
}

/// Users add and remove their own tags on workflow, toolset, and tool instances without
/// changing any digest, but can't remove the tags Thorium sets
#[tokio::test]
async fn instance_user_tags() -> Result<(), Error> {
    // a workflow instance, a toolset instance, and a tool instance in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&europa.username),
    )
    .await?;
    let (tool_root, tool) = agentic_tool(&[&a], &admin).await?;
    let set_resp = admin
        .entities
        .create(generators::gen_toolset(
            "kit",
            &[&a],
            ToolSetEntityRequest::default(),
        ))
        .await?;
    let tool_set = admin.entities.get(set_resp.id).await?;
    let set_r1 = admin
        .entities
        .create(generators::gen_toolset_instance_req(
            &generators::gen_toolset_instance(&tool_set, 1, None, &[&tool]),
        ))
        .await?;
    let solo = agent(&admin, "solo", &[&a], |req| req.tool(tool_root.id)).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "tagged", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let instance =
        generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&tool, &harness])]);
    let r1 = submit(&admin, &instance).await?;
    // users add and remove their own tags on each, and nothing they hash changes
    let own = json!({"tags": {"Owner": ["europa"]}});
    for id in [r1.id, set_r1.id, tool.id] {
        let before = admin.entities.get(id).await?;
        assert_eq!(raw_tags(&europa, id, false, own.clone()).await?, 204);
        let tagged = admin.entities.get(id).await?;
        assert_eq!(tag_values(&tagged, "Owner"), vec!["europa".to_owned()]);
        assert_eq!(
            serde_json::to_value(&tagged.metadata)?,
            serde_json::to_value(&before.metadata)?
        );
        assert_eq!(raw_tags(&europa, id, true, own.clone()).await?, 204);
        assert_eq!(tag_values(&admin.entities.get(id).await?, "Owner").len(), 0);
    }
    // the workflow instance's intrinsic tags can't be removed
    for (key, value) in [
        ("WorkflowId", created.id.to_string()),
        ("InstanceAgent", solo.id.to_string()),
        ("InstanceFollowsTool", tool_root.id.to_string()),
    ] {
        let status = raw_tags(&europa, r1.id, true, json!({"tags": {key: [value]}})).await?;
        assert_eq!(status, 400, "{key}");
    }
    let after = admin.entities.get(r1.id).await?;
    assert_eq!(
        tag_values(&after, "InstanceFollowsTool"),
        vec![tool_root.id.to_string()]
    );
    assert_eq!(instance_meta(&after).digest, instance.digest);
    Ok(())
}

/// An agent a workflow orchestrates can't be deleted until the workflow drops it, and old
/// instances stay readable with its snapshot after it is deleted
#[tokio::test]
async fn delete_agent_keeps_snapshots() -> Result<(), Error> {
    // a workflow with two agents and an instance of both
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let keep = agent(&admin, "keep", &[&a], |req| req).await?;
    let doomed = agent(&admin, "doomed", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(keep.id)
        .agent(doomed.id)
        .entry_agent(keep.id);
    let created = workflow(&admin, "pruned", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let instance = generators::gen_workflow_instance(
        &created,
        1,
        None,
        &[(&keep, &[&harness]), (&doomed, &[])],
    );
    let r1 = submit(&admin, &instance).await?;
    // the agent can't be deleted while the workflow orchestrates it
    let (status, msg) = refusal(admin.entities.delete(doomed.id).await);
    assert_eq!(status, 409);
    assert!(msg.contains("pruned"), "{msg}");
    let seen = changes(&admin, doomed.id).await?.len();
    // the workflow drops the agent, then the agent is deleted
    let remove = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(doomed.id);
        }
    });
    update_metadata(&admin, created.id, remove).await?;
    admin.entities.delete(doomed.id).await?;
    // dropping its edge queued one edge change
    let username = admin.users.info().await?.username;
    assert_eq!(
        changes_since(&admin, doomed.id, seen).await?,
        vec![queued(
            &username,
            &[&a],
            WorkflowGraphChange::EdgeChanged {
                source: created.id,
                target: doomed.id,
                kind: AssociationKind::Orchestrates,
            },
        )]
    );
    // the workflow no longer orchestrates it
    assert_eq!(
        edges(
            &admin,
            created.id,
            AssociationKind::Orchestrates,
            Directionality::To
        )
        .await?,
        vec![keep.id]
    );
    assert_eq!(
        ids(&meta(&admin.entities.get(created.id).await?).agents),
        vec![keep.id]
    );
    // the old instance still reads with the deleted agent's snapshot
    let read = admin.entities.get(r1.id).await?;
    let snapshot = instance_meta(&read)
        .agent(doomed.id)
        .expect("the deleted agent's snapshot");
    assert_eq!(snapshot.name, "doomed");
    assert_eq!(snapshot.config_hash, agent_meta(&doomed).config_hash);
    Ok(())
}

/// Deleting a workflow removes its `Orchestrates` edges, creates no instance, and leaves its
/// instances readable
#[tokio::test]
async fn delete_catalog() -> Result<(), Error> {
    // a workflow with one agent and one instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "doomed", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let r1 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    let seen = changes(&admin, created.id).await?.len();
    // delete the workflow
    admin.entities.delete(created.id).await?;
    // removing its edge queued one edge change and nothing else
    let username = admin.users.info().await?.username;
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![queued(
            &username,
            &[&a],
            WorkflowGraphChange::EdgeChanged {
                source: created.id,
                target: solo.id,
                kind: AssociationKind::Orchestrates,
            },
        )]
    );
    // its edges are gone and no instance was created
    assert_eq!(
        edges(
            &admin,
            solo.id,
            AssociationKind::Orchestrates,
            Directionality::From
        )
        .await?,
        Vec::<Uuid>::new()
    );
    assert_eq!(
        listed(
            &admin,
            std::slice::from_ref(&a),
            EntityKinds::WorkflowInstance
        )
        .await?,
        vec![r1.id]
    );
    // the workflow is gone and its instance is still readable
    let (status, _) = refusal(admin.entities.get(created.id).await);
    assert_eq!(status, 404);
    let kept = admin.entities.get(r1.id).await?;
    assert_eq!(instance_meta(&kept).workflow, created.id);
    Ok(())
}

/// A workflow instance in two groups is deleted from both by its submitter or by a manager of
/// both, while a member who isn't the submitter and a manager of only one group can't delete it
#[tokio::test]
async fn instance_delete_needs_every_group() -> Result<(), Error> {
    // the submitter and a plain member are in both groups, and two managers lead one or both
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let submitter = moon_user(&admin, "io", &suffix).await?;
    let member = moon_user(&admin, "callisto", &suffix).await?;
    let partial = moon_user(&admin, "europa", &suffix).await?;
    let full = moon_user(&admin, "ganymede", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .user(&submitter.username)
            .user(&member.username)
            .manager(&partial.username)
            .manager(&full.username),
    )
    .await?;
    let b = god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default()
            .user(&submitter.username)
            .user(&member.username)
            .user(&partial.username)
            .manager(&full.username),
    )
    .await?;
    // a workflow in both groups, and the submitter's first instance of it
    let solo = agent(&admin, "solo", &[&a, &b], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "shared", &[&a, &b], req).await?;
    let (_, harness) = harness_tool(&[&a, &b], &admin).await?;
    let r1 = submit(
        &submitter.client,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    // a member who isn't the submitter can't delete it, and neither can a manager of one group
    let (status, msg) = refusal(member.client.entities.delete(r1.id).await);
    assert_eq!(status, 401, "{msg}");
    let (status, msg) = refusal(partial.client.entities.delete(r1.id).await);
    assert_eq!(status, 401, "{msg}");
    // both refusals left the instance in both groups
    let both = sorted_groups(&[a.clone(), b.clone()]);
    assert_eq!(
        sorted_groups(&admin.entities.get(r1.id).await?.groups),
        both
    );
    // the submitter's next instance follows it
    let r2 = submit(
        &submitter.client,
        &generators::gen_workflow_instance(&created, 2, Some(r1.id), &[(&solo, &[&harness])]),
    )
    .await?;
    // a manager of both groups deletes the first from both
    full.client.entities.delete(r1.id).await?;
    let (status, _) = refusal(admin.entities.get(r1.id).await);
    assert_eq!(status, 404);
    assert_eq!(
        listed(
            &admin,
            &[a.clone(), b.clone()],
            EntityKinds::WorkflowInstance
        )
        .await?,
        vec![r2.id]
    );
    // the submitter deletes their own instance from both groups
    assert_eq!(
        sorted_groups(&admin.entities.get(r2.id).await?.groups),
        both
    );
    submitter.client.entities.delete(r2.id).await?;
    let (status, _) = refusal(admin.entities.get(r2.id).await);
    assert_eq!(status, 404);
    assert_eq!(
        listed(&admin, &[a, b], EntityKinds::WorkflowInstance).await?,
        Vec::<Uuid>::new()
    );
    Ok(())
}

/// A `BuiltFrom` edge can't be written through the generic route, so no instance can be
/// attached to another workflow
#[tokio::test]
async fn forged_built_from() -> Result<(), Error> {
    // io's workflow has an instance and a victim workflow has one
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = || {
        WorkflowCatalogEntityRequest::default()
            .agent(solo.id)
            .entry_agent(solo.id)
    };
    let mine = workflow(&io.client, "mine", &[&a], req()).await?;
    let victim = workflow(&admin, "victim", &[&a], req()).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let m1 = submit(
        &io.client,
        &generators::gen_workflow_instance(&mine, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    let v1 = submit(
        &admin,
        &generators::gen_workflow_instance(&victim, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    // io's instance can't be linked to the victim through the generic route
    let forged = admin.entities.get(m1.id).await?;
    let request = thorium::models::AssociationRequest::new(
        AssociationKind::BuiltFrom,
        thorium::models::AssociationTarget::Entity {
            id: forged.id,
            name: forged.name.clone(),
        },
    )
    .target(thorium::models::AssociationTarget::Entity {
        id: victim.id,
        name: victim.name.clone(),
    });
    let (status, msg) = refusal(io.client.associations.create(&request).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("BuiltFrom"), "{msg}");
    // the victim's latest instance and next revision are unchanged
    assert_eq!(
        meta(&admin.entities.get(victim.id).await?).latest_instance,
        Some(v1.id)
    );
    let v2 = submit(
        &admin,
        &generators::gen_workflow_instance(&victim, 2, Some(v1.id), &[(&solo, &[&harness])]),
    )
    .await?;
    assert_eq!(v2.name, "victim#r2");
    Ok(())
}

/// A submission or tag request can't set a workflow instance's intrinsic tag keys, in any case
#[tokio::test]
async fn intrinsic_tag_keys_refused() -> Result<(), Error> {
    // io edits minerva, where an agent, a tool, and two workflows live
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let other = agent(&admin, "other", &[&a], |req| req).await?;
    let (tool_root, _) = agentic_tool(&[&a], &admin).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&io.client, "forged", &[&a], req).await?;
    let victim = workflow(
        &io.client,
        "victim",
        &[&a],
        WorkflowCatalogEntityRequest::default(),
    )
    .await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let instance = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]);
    // a submission naming any intrinsic key is refused, and nothing is stored
    for (key, value) in [
        ("InstanceAgent", other.id.to_string()),
        ("WorkflowId", victim.id.to_string()),
        ("InstanceFollowsTool", tool_root.id.to_string()),
        ("InstanceFollowsToolSet", tool_root.id.to_string()),
        ("WorkflowStatus", "Ready".to_owned()),
        ("WorkflowRevision", "9".to_owned()),
        ("WorkflowDigest", "0".repeat(64)),
        ("workflowrevision", "9".to_owned()),
    ] {
        let req = generators::gen_workflow_instance_req(&instance).tag(key, value);
        let (status, msg) = refusal(io.client.entities.create(req).await);
        assert_eq!(status, 400, "{key}: {msg}");
        assert!(msg.contains(key), "{msg}");
    }
    assert_eq!(
        listed(
            &admin,
            std::slice::from_ref(&a),
            EntityKinds::WorkflowInstance
        )
        .await?,
        Vec::<Uuid>::new()
    );
    // the instance submits with only the submitter's own tags
    let req = generators::gen_workflow_instance_req(&instance).tag("Owner", "io");
    let r1 = io.client.entities.create(req).await?;
    // the tag route refuses the intrinsic keys too, and keeps the stored tags
    for (key, value) in [
        ("InstanceFollowsTool", tool_root.id.to_string()),
        ("InstanceFollowsToolSet", tool_root.id.to_string()),
        ("InstanceAgent", other.id.to_string()),
        ("WorkflowId", victim.id.to_string()),
        ("WorkflowRevision", "9".to_owned()),
        ("WorkflowDigest", "0".repeat(64)),
        ("WorkflowStatus", "Ready".to_owned()),
        ("workflowdigest", "0".repeat(64)),
    ] {
        let status = raw_tags(&io, r1.id, false, json!({"tags": {key: [value]}})).await?;
        assert_eq!(status, 400, "{key}");
    }
    let stored = admin.entities.get(r1.id).await?;
    assert_eq!(
        tag_values(&stored, "InstanceAgent"),
        vec![solo.id.to_string()]
    );
    assert_eq!(
        tag_values(&stored, "WorkflowId"),
        vec![created.id.to_string()]
    );
    assert_eq!(
        tag_values(&stored, "InstanceFollowsTool"),
        Vec::<String>::new()
    );
    assert_eq!(tag_values(&stored, "Owner"), vec!["io".to_owned()]);
    // the victim lists no instance by its tag
    let opts = EntityListOpts::default()
        .groups(vec![a.clone()])
        .tag("WorkflowId", victim.id.to_string())
        .kind(EntityKinds::WorkflowInstance)
        .limit(10);
    assert_eq!(admin.entities.list(&opts).await?.data.len(), 0);
    Ok(())
}

/// A workflow gains a group only when everything its existing instances snapshotted is in it,
/// so an old snapshot of an agent it no longer orchestrates never reaches a new group
#[tokio::test]
async fn widening_needs_contained_snapshots() -> Result<(), Error> {
    // io edits minerva and vesta, and callisto is only in vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let mars = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let vesta = god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default()
            .user(&io.username)
            .user(&callisto.username),
    )
    .await?;
    // a secret agent in minerva and mars that calls a minerva tool, and a public one in vesta
    let (tool_root, tool) = agentic_tool(&[&minerva, &mars], &admin).await?;
    let secret = agent(&admin, "secret", &[&minerva, &mars], |req| {
        req.tool(tool_root.id)
    })
    .await?;
    let public = agent(&admin, "public", &[&minerva, &vesta], |req| req).await?;
    // io orchestrates the secret agent and submits r1
    let req = WorkflowCatalogEntityRequest::default()
        .agent(secret.id)
        .entry_agent(secret.id);
    let created = workflow(&io.client, "leaky", &[&minerva], req).await?;
    let (_, harness) = harness_tool(&[&minerva, &mars], &admin).await?;
    let r1 = submit(
        &io.client,
        &generators::gen_workflow_instance(&created, 1, None, &[(&secret, &[&tool, &harness])]),
    )
    .await?;
    // io swaps the secret agent for the public one
    let swap = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            add_agents,
            remove_agents,
            entry_agent,
            ..
        } = update
        {
            add_agents.push(public.id);
            remove_agents.push(secret.id);
            *entry_agent = Some(public.id);
        }
    });
    update_metadata(&io.client, created.id, swap).await?;
    // widening to vesta is refused, naming the old instance and what it snapshotted
    let (status, msg) = refusal(
        io.client
            .entities
            .update(created.id, EntityUpdate::default().group(&vesta))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("leaky#r1"), "{msg}");
    assert!(msg.contains("secret"), "{msg}");
    // nothing changed: callisto can't read the old snapshot and the workflow stays in minerva
    assert_eq!(refusal(callisto.client.entities.get(r1.id).await).0, 404);
    assert_eq!(
        admin.entities.get(r1.id).await?.groups,
        vec![minerva.clone()]
    );
    assert_eq!(
        admin.entities.get(created.id).await?.groups,
        vec![minerva.clone()]
    );
    // once the old instance is deleted the workflow widens
    admin.entities.delete(r1.id).await?;
    io.client
        .entities
        .update(created.id, EntityUpdate::default().group(&vesta))
        .await?;
    assert_eq!(
        sorted_groups(&admin.entities.get(created.id).await?.groups),
        sorted_groups(&[minerva.clone(), vesta.clone()])
    );
    Ok(())
}

/// Every tool a submitted instance lists is used by one of its agents or backs one of its
/// tools, so no listed tool skips the group checks
#[tokio::test]
async fn listed_tools_need_a_user() -> Result<(), Error> {
    // io edits minerva and vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let vesta = god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    // a tool only in minerva, a tool in both, and an agent in both
    let (_, narrow) = agentic_tool(&[&minerva], &admin).await?;
    let (_, wide) = agentic_tool(&[&minerva, &vesta], &admin).await?;
    let solo = agent(&admin, "solo", &[&minerva, &vesta], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&io.client, "orphan", &[&minerva, &vesta], req).await?;
    // a tool no agent is given is refused, even when its groups cover the workflow's
    for listed in [&narrow, &wide] {
        let mut instance = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[])]);
        instance.tools.push(generators::gen_instanced_tool(
            &admin.entities.get(listed.id).await?,
        ));
        refresh_digests(&mut instance, &BTreeMap::new());
        let (status, msg) = refusal(submit(&io.client, &instance).await);
        assert_eq!(status, 400, "{msg}");
        assert!(msg.contains(&listed.name), "{msg}");
    }
    // a dependency that backs none of the instance's tools is refused too
    let mut instance = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[])]);
    let mut dependency = generators::gen_instanced_tool(&admin.entities.get(wide.id).await?);
    dependency.dependency = true;
    dependency.pinned = true;
    instance.tools.push(dependency);
    refresh_digests(&mut instance, &BTreeMap::new());
    let (status, msg) = refusal(submit(&io.client, &instance).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("backs"), "{msg}");
    // nothing was stored
    assert_eq!(
        listed(
            &admin,
            std::slice::from_ref(&minerva),
            EntityKinds::WorkflowInstance
        )
        .await?,
        Vec::<Uuid>::new()
    );
    Ok(())
}

/// An edge added through the generic association route marks its workflow, agent, or toolset
/// source modified, as its own update does
#[tokio::test]
async fn generic_edges_mark_source_modified() -> Result<(), Error> {
    // a bare workflow, agent, and toolset in minerva, and the targets of their edges
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let caller = agent(&admin, "caller", &[&a], |req| req).await?;
    let (tool_root, _) = agentic_tool(&[&a], &admin).await?;
    let created = workflow(
        &admin,
        "bare",
        &[&a],
        WorkflowCatalogEntityRequest::default(),
    )
    .await?;
    let set_resp = admin
        .entities
        .create(generators::gen_toolset(
            "kit",
            &[&a],
            ToolSetEntityRequest::default(),
        ))
        .await?;
    let kit = admin.entities.get(set_resp.id).await?;
    // read each source's modified time
    let modified = |entity: &Entity| match &entity.metadata {
        EntityMetadata::WorkflowCatalog(meta) => meta.modified,
        EntityMetadata::Agent(meta) => meta.modified,
        EntityMetadata::ToolSet(meta) => meta.modified,
        other => panic!("{other:?} has no modified time"),
    };
    // add one edge from each source through the generic route
    for (kind, source, target) in [
        (AssociationKind::Orchestrates, &created, &solo),
        (AssociationKind::Calls, &caller, &tool_root),
        (AssociationKind::Includes, &kit, &tool_root),
    ] {
        let before = modified(&admin.entities.get(source.id).await?);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let request = thorium::models::AssociationRequest::new(
            kind,
            thorium::models::AssociationTarget::Entity {
                id: source.id,
                name: source.name.clone(),
            },
        )
        .target(thorium::models::AssociationTarget::Entity {
            id: target.id,
            name: target.name.clone(),
        });
        admin.associations.create(&request).await?;
        let after = admin.entities.get(source.id).await?;
        assert!(
            modified(&after) > before,
            "{kind}: {before} -> {}",
            modified(&after)
        );
    }
    Ok(())
}

/// Reading a workflow and submitting its next instance cost the same after hundreds of
/// revisions as after one
#[tokio::test]
async fn revision_history_cost_is_flat() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "history", &[&a], req).await?;
    // time the median of a few catalog reads
    let read_time = || async {
        let mut times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            admin.entities.get(created.id).await?;
            times.push(start.elapsed());
        }
        times.sort();
        Ok::<Duration, Error>(times[2])
    };
    // submit 200 revisions, timing the first and last few submissions and reads
    let mut previous = None;
    let mut first_submits = Vec::new();
    let mut last_submits = Vec::new();
    let mut first_read = Duration::ZERO;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    for rev in 1..=200_u64 {
        let instance =
            generators::gen_workflow_instance(&created, rev, previous, &[(&solo, &[&harness])]);
        let start = Instant::now();
        previous = Some(submit(&admin, &instance).await?.id);
        let took = start.elapsed();
        if rev <= 5 {
            first_submits.push(took);
        } else if rev > 195 {
            last_submits.push(took);
        }
        if rev == 5 {
            first_read = read_time().await?;
        }
    }
    let last_read = read_time().await?;
    first_submits.sort();
    last_submits.sort();
    let (first_submit, last_submit) = (first_submits[2], last_submits[2]);
    println!(
        "r5: read {first_read:?} submit {first_submit:?}; r200: read {last_read:?} submit {last_submit:?}"
    );
    assert!(
        last_read <= first_read * 2 + Duration::from_millis(20),
        "a read after 200 revisions took {last_read:?}, after 5 {first_read:?}"
    );
    assert!(
        last_submit <= first_submit * 2 + Duration::from_millis(30),
        "a submission after 200 revisions took {last_submit:?}, after 5 {first_submit:?}"
    );
    Ok(())
}

/// A workflow keeps its newest and latest ready instance as instances are submitted and
/// deleted, and the next revision follows the highest one it gave out
#[tokio::test]
async fn newest_and_latest_kept() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "kept", &[&a], req).await?;
    assert_eq!(meta(&created).newest_instance, None);
    // a ready r1 is both, and an invalid r2 is only the newest
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let r1 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    let mut invalid =
        generators::gen_workflow_instance(&created, 2, Some(r1.id), &[(&solo, &[&harness])]);
    invalid.status = WorkflowInstanceStatus::Invalid {
        errors: vec!["broken".to_owned()],
    };
    refresh_digests(&mut invalid, &BTreeMap::new());
    let r2 = submit(&admin, &invalid).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r2.id));
    assert_eq!(meta(&read).latest_instance, Some(r1.id));
    // deleting r2 makes r1 the newest again, and r3 supersedes it
    admin.entities.delete(r2.id).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r1.id));
    assert_eq!(meta(&read).latest_instance, Some(r1.id));
    let (status, _) = refusal(
        submit(
            &admin,
            &generators::gen_workflow_instance(&created, 3, Some(r2.id), &[(&solo, &[&harness])]),
        )
        .await,
    );
    assert_eq!(status, 400);
    let r3 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 3, Some(r1.id), &[(&solo, &[&harness])]),
    )
    .await?;
    // deleting every instance clears both, and the revision still moves on
    admin.entities.delete(r3.id).await?;
    admin.entities.delete(r1.id).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, None);
    assert_eq!(meta(&read).latest_instance, None);
    let r4 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 4, None, &[(&solo, &[&harness])]),
    )
    .await?;
    assert_eq!(r4.name, "kept#r4");
    // a settings update keeps them
    let update = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { auto_instance, .. } = update {
            *auto_instance = Some(false);
        }
    });
    update_metadata(&admin, created.id, update).await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r4.id));
    assert_eq!(meta(&read).latest_instance, Some(r4.id));
    Ok(())
}

/// The names of the instances tagged as a workflow's, in name order
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `workflow` - The id of the workflow
async fn tagged_names(client: &Thorium, workflow: Uuid) -> Result<Vec<String>, Error> {
    // list the instances by the workflow's tag
    let opts = EntityListOpts::default()
        .tag("WorkflowId", workflow.to_string())
        .limit(100);
    let mut names = client
        .entities
        .list(&opts)
        .await?
        .data
        .into_iter()
        .map(|line| line.name)
        .collect::<Vec<String>>();
    names.sort();
    Ok(names)
}

/// An instance write that fails after its revision is reserved burns that revision: the workflow
/// still names no instance, the same revision sent again is a conflict, and the next revision is
/// taken by exactly one of two submissions racing for it
#[tokio::test]
async fn failed_instance_write_leaves_workflow_usable() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let name = format!("faulty-{suffix}");
    let created = workflow(&admin, &name, &[&a], req).await?;
    // r1's instance write fails after r1 is reserved
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let r1 = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]);
    test_utilities::faults::fail_before_entity_insert(&format!("{name}#r1"));
    let (status, _) = refusal(submit(&admin, &r1).await);
    assert_eq!(status, 500);
    // the workflow names no instance, but revision 1 is taken
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, None);
    assert_eq!(meta(&read).latest_instance, None);
    assert_eq!(meta(&read).highest_revision, 1);
    // the same submission sent again is a conflict naming revision 2
    let (status, msg) = refusal(submit(&admin, &r1).await);
    assert_eq!(status, 409, "{msg}");
    assert!(msg.contains("revision 2"), "{msg}");
    // two r2 submissions at once create exactly one instance and the other is a conflict
    let r2 = generators::gen_workflow_instance(&created, 2, None, &[(&solo, &[&harness])]);
    let (left, right) = tokio::join!(submit(&admin, &r2), submit(&admin, &r2));
    let results = [left, right];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let mut retried = None;
    for result in results {
        match result {
            Ok(resp) => retried = Some(resp),
            Err(err) => assert_eq!(err.status().map(|status| status.as_u16()), Some(409)),
        }
    }
    let retried = retried.unwrap();
    assert_eq!(retried.name, format!("{name}#r2"));
    // r2 becomes the newest and latest instance, and is the only instance
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(retried.id));
    assert_eq!(meta(&read).latest_instance, Some(retried.id));
    assert_eq!(meta(&read).highest_revision, 2);
    assert_eq!(
        tagged_names(&admin, created.id).await?,
        vec![format!("{name}#r2")]
    );
    Ok(())
}

/// An instance write that fails after the instance is written and before its link never lets
/// a retry reuse the instance's revision
#[tokio::test]
async fn failed_instance_link_never_reuses_revision() -> Result<(), Error> {
    // a workflow with one agent
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let name = format!("orphan-{suffix}");
    let created = workflow(&admin, &name, &[&a], req).await?;
    // r1's write fails right after the instance is written
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let r1 = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]);
    test_utilities::faults::fail_after_entity_insert(&format!("{name}#r1"));
    let (status, _) = refusal(submit(&admin, &r1).await);
    assert_eq!(status, 500);
    // the same r1 sent again is a conflict naming revision 2
    let (status, msg) = refusal(submit(&admin, &r1).await);
    assert_eq!(status, 409, "{msg}");
    assert!(msg.contains("revision 2"), "{msg}");
    // r2 is taken and becomes the newest and latest instance
    let r2 = generators::gen_workflow_instance(&created, 2, None, &[(&solo, &[&harness])]);
    let r2 = submit(&admin, &r2).await?;
    assert_eq!(r2.name, format!("{name}#r2"));
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r2.id));
    assert_eq!(meta(&read).latest_instance, Some(r2.id));
    // no two instances share a name
    let names = tagged_names(&admin, created.id).await?;
    assert_eq!(names, vec![format!("{name}#r1"), format!("{name}#r2")]);
    Ok(())
}

/// A workflow whose revisions have a gap is read, rebuilt, and superseded as one without it
#[tokio::test]
async fn revision_gap_reads() -> Result<(), Error> {
    // a workflow whose agent pins a harness, so it resolves to a ready instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req.harness(harness.id)).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let name = format!("gapped-{suffix}");
    let created = workflow(&admin, &name, &[&a], req).await?;
    let crawl = CrawlLimits::default();
    let build = || {
        build_workflow_instance(
            &admin,
            created.id,
            &crawl,
            WorkflowInstanceTrigger::Manual,
            "thorium",
        )
    };
    // r1 is burned by a failed write
    let WorkflowBuild::New(first) = build().await? else {
        panic!("the first build should be new");
    };
    assert_eq!(first.instance.revision, 1);
    test_utilities::faults::fail_before_entity_insert(&format!("{name}#r1"));
    let (status, _) = refusal(submit(&admin, &first.instance).await);
    assert_eq!(status, 500);
    // the next build skips the burned revision and supersedes no instance, and is accepted
    let WorkflowBuild::New(second) = build().await? else {
        panic!("the build after a burned revision should be new");
    };
    assert_eq!(second.instance.revision, 2);
    assert_eq!(second.instance.previous, None);
    let r2 = submit(&admin, &second.instance).await?;
    assert_eq!(r2.name, format!("{name}#r2"));
    // the workflow names r2 as its newest and latest instance and a rebuild finds it current
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r2.id));
    assert_eq!(meta(&read).latest_instance, Some(r2.id));
    assert!(matches!(build().await?, WorkflowBuild::Current { newest, .. } if newest == r2.id));
    assert_eq!(
        tagged_names(&admin, created.id).await?,
        vec![format!("{name}#r2")]
    );
    // the instance reads back with its revision and no previous instance
    let stored = admin.entities.get(r2.id).await?;
    let EntityMetadata::WorkflowInstance(instance) = &stored.metadata else {
        panic!("r2 should be a workflow instance");
    };
    assert_eq!(instance.revision, 2);
    assert_eq!(instance.previous, None);
    // r3 supersedes r2 across the gap
    let r3 = generators::gen_workflow_instance(&created, 3, Some(r2.id), &[(&solo, &[&harness])]);
    let r3 = submit(&admin, &r3).await?;
    assert_eq!(r3.name, format!("{name}#r3"));
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r3.id));
    assert_eq!(meta(&read).highest_revision, 3);
    Ok(())
}

/// A workflow that names a newest or latest instance that's gone finds both again among its
/// instances when the next instance is submitted
#[tokio::test]
async fn missing_newest_instance_found_again() -> Result<(), Error> {
    // a workflow with a ready r1 and r2
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "found", &[&a], req).await?;
    let (_, harness) = harness_tool(&[&a], &admin).await?;
    let r1 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    let r2 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 2, Some(r1.id), &[(&solo, &[&harness])]),
    )
    .await?;
    // r2's delete fails after r2 is removed, so the workflow still names it
    test_utilities::faults::fail_after_entity_delete(r2.id);
    let (status, _) = refusal(admin.entities.delete(r2.id).await);
    assert_eq!(status, 500);
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r2.id));
    // r3 supersedes r1, the newest instance left
    let r3 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 3, Some(r1.id), &[(&solo, &[&harness])]),
    )
    .await?;
    let read = admin.entities.get(created.id).await?;
    assert_eq!(meta(&read).newest_instance, Some(r3.id));
    assert_eq!(meta(&read).latest_instance, Some(r3.id));
    Ok(())
}

/// A workflow gains a group only when its agents are in it, its instances follow its group
/// changes without their snapshots changing, and an instance's groups can't be changed
#[tokio::test]
async fn catalog_group_changes() -> Result<(), Error> {
    // a workflow in minerva with one instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let solo = agent(&admin, "solo", &[&a], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "growing", &[&a], req).await?;
    let (harness_root, harness) = harness_tool(&[&a], &admin).await?;
    let r1 = submit(
        &admin,
        &generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[&harness])]),
    )
    .await?;
    // adding mars is refused while the agent isn't in it
    let (status, msg) = refusal(
        admin
            .entities
            .update(created.id, EntityUpdate::default().group(&b))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains(&b), "{msg}");
    // once the agent and its harness are in mars the workflow and its instance gain it together
    admin
        .entities
        .update(solo.id, EntityUpdate::default().group(&b))
        .await?;
    admin
        .entities
        .update(harness_root.id, EntityUpdate::default().group(&b))
        .await?;
    admin
        .entities
        .update(created.id, EntityUpdate::default().group(&b))
        .await?;
    let widened = admin.entities.get(r1.id).await?;
    assert_eq!(
        sorted_groups(&widened.groups),
        sorted_groups(&[a.clone(), b.clone()])
    );
    assert_eq!(instance_meta(&widened).workflow_groups, vec![a.clone()]);
    // the instance keeps its followed and agent tags in the new group
    let in_mars = admin
        .entities
        .list(
            &EntityListOpts::default()
                .groups(vec![b.clone()])
                .tag("InstanceAgent", solo.id.to_string())
                .limit(10),
        )
        .await?;
    assert_eq!(
        in_mars
            .data
            .iter()
            .map(|line| line.id)
            .collect::<Vec<Uuid>>(),
        vec![r1.id]
    );
    // an instance's groups can't be changed directly
    let (status, _) = refusal(
        admin
            .entities
            .update(r1.id, EntityUpdate::default().remove_group(&b))
            .await,
    );
    assert_eq!(status, 400);
    // removing mars from the workflow removes it from the instance too
    admin
        .entities
        .update(created.id, EntityUpdate::default().remove_group(&b))
        .await?;
    assert_eq!(admin.entities.get(r1.id).await?.groups, vec![a.clone()]);
    Ok(())
}

/// A submission is refused at once, without waiting on the workflow's lock, when the submitter
/// can't see or edit the workflow
#[tokio::test]
async fn instance_submission_authorizes_before_locking() -> Result<(), Error> {
    // ganymede edits minerva but only monitors mars, and amalthea is only in vulcan
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&ganymede.username),
    )
    .await?;
    let mars = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default().monitor(&ganymede.username),
    )
    .await?;
    let vulcan = god_group(
        &admin,
        "vulcan",
        &suffix,
        &GroupRoles::default().user(&amalthea.username),
    )
    .await?;
    let solo = agent(&admin, "solo", &[&minerva, &mars], |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(solo.id)
        .entry_agent(solo.id);
    let created = workflow(&admin, "oracle", &[&minerva, &mars], req).await?;
    // with another request holding the workflow's lock, ganymede's submission is a 401 at once
    let mut instance = generators::gen_workflow_instance(&created, 1, None, &[(&solo, &[])]);
    instance.triggered_by = ganymede.username.clone();
    let mut req = generators::gen_workflow_instance_req(&instance);
    req.groups = vec![minerva.clone()];
    test_utilities::hold_instance_lock(created.id).await?;
    let ((elapsed, status), released) = tokio::join!(
        timed_refusal(ganymede.client.entities.create(req)),
        release_later(created.id, Duration::from_millis(3_000))
    );
    released?;
    assert_eq!(status, 401);
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    // amalthea can't see the workflow, so hers is a 404 at once
    instance.triggered_by = amalthea.username.clone();
    let mut req = generators::gen_workflow_instance_req(&instance);
    req.groups = vec![vulcan.clone()];
    test_utilities::hold_instance_lock(created.id).await?;
    let ((elapsed, status), released) = tokio::join!(
        timed_refusal(amalthea.client.entities.create(req)),
        release_later(created.id, Duration::from_millis(3_000))
    );
    released?;
    assert_eq!(status, 404);
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    // nothing was written
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).latest_instance,
        None
    );
    Ok(())
}

/// Copy entities and their outgoing graph edges, as the API returns them, into an in-memory graph
///
/// # Arguments
///
/// * `client` - The client to read the entities with
/// * `ids` - The entities to copy
async fn mirror(client: &Thorium, ids: &[Uuid]) -> Result<MemoryGraph, Error> {
    // read every entity first, so each edge knows the kinds of both ends
    let mut read = Vec::with_capacity(ids.len());
    for id in ids {
        read.push(client.entities.get(*id).await?);
    }
    let mut graph = MemoryGraph::default();
    for entity in &read {
        graph.insert(entity.clone());
    }
    // add each entity's outgoing graph edges as two rows
    for entity in &read {
        let mut outgoing: Vec<(AssociationKind, Uuid)> = Vec::new();
        match &entity.metadata {
            EntityMetadata::WorkflowCatalog(meta) => {
                outgoing.extend(
                    meta.agents
                        .iter()
                        .map(|a| (AssociationKind::Orchestrates, a.id)),
                );
            }
            EntityMetadata::Agent(meta) => {
                outgoing.extend(
                    meta.sub_agents
                        .iter()
                        .map(|a| (AssociationKind::DelegatesTo, a.id)),
                );
                outgoing.extend(
                    meta.latest_tools
                        .iter()
                        .chain(&meta.pinned_tools)
                        .chain(&meta.tool_sets)
                        .map(|target| (AssociationKind::Calls, target.id)),
                );
                outgoing.extend(
                    meta.harness
                        .iter()
                        .map(|target| (AssociationKind::RunsIn, target.id)),
                );
            }
            EntityMetadata::ToolSet(meta) => {
                outgoing.extend(
                    meta.tools
                        .iter()
                        .map(|target| (AssociationKind::Includes, target.id)),
                );
            }
            _ => (),
        }
        for (kind, target) in outgoing {
            graph.edge(kind, entity.id, target);
        }
    }
    Ok(graph)
}

/// The resolver gives the same instance through the Thorium client as through an in-memory copy
/// of the same graph, its snapshot records groups the catalog isn't in, the API accepts what it
/// resolves, and the client's tag lookups search only the groups they're given
#[tokio::test]
// one scenario builds a graph and checks it through both readers and the API
#[allow(clippy::too_many_lines)]
async fn resolver_parity() -> Result<(), Error> {
    // io edits minerva and mars; the workflow is in minerva, its agents and tools in both
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let both = [minerva.as_str(), mars.as_str()];
    // a tool followed at latest, a pinned tool, and a container tool pinning an Image instance
    let (latest_root, latest) = agentic_tool(&both, &admin).await?;
    let (pinned_root, pinned) = agentic_tool(&both, &admin).await?;
    let (image_root, image) =
        generators::tool(&both, "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let mut spec = generators::gen_agentic_spec();
    if let ToolSpec::Agentic(agentic) = &mut spec {
        agentic.backing = AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: image.id,
            ..ContainerToolBacking::default()
        });
        agentic.timeout = Some(60);
    }
    let (backed_root, backed) = generators::tool(&both, "1.0.0", &spec, &admin).await?;
    // the entry agent runs in a harness at latest, calls two tools, and delegates to a sub-agent
    // that calls the container tool
    let (harness_root, harness) = harness_tool(&both, &admin).await?;
    let sub = agent(&admin, "helper", &both, |req| req.tool(backed_root.id)).await?;
    let entry = agent(&admin, "lead", &both, |req| {
        req.tool(latest_root.id)
            .tool(pinned.id)
            .sub_agent(sub.id)
            .harness(harness_root.id)
    })
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(entry.id)
        .entry_agent(entry.id);
    let created = workflow(&io.client, "Triage", &[&minerva], req).await?;
    // resolve through the client and through an in-memory copy of the same graph
    let limits = CrawlLimits::default();
    let through_client = resolve_workflow(&admin, created.id, &limits).await?;
    let ids = [
        created.id,
        entry.id,
        sub.id,
        latest_root.id,
        latest.id,
        pinned_root.id,
        pinned.id,
        image_root.id,
        image.id,
        backed_root.id,
        backed.id,
        harness_root.id,
        harness.id,
    ];
    let graph = mirror(&admin, &ids).await?;
    let through_fixture = resolve_workflow(&graph, created.id, &limits).await?;
    assert_eq!(through_client.instance, through_fixture.instance);
    assert_eq!(through_client.dedupe_key(), through_fixture.dedupe_key());
    assert_eq!(through_client.follows_tools, through_fixture.follows_tools);
    // the instance is ready, with every tool and the snapshot groups as stored
    let instance = &through_client.instance;
    assert!(instance.is_ready(), "{:?}", instance.status);
    assert_eq!(instance.workflow_groups, vec![minerva.clone()]);
    assert_eq!(instance.agents.len(), 2);
    assert_eq!(instance.tools.len(), 5);
    assert_eq!(instance.agent(entry.id).unwrap().harness, Some(harness.id));
    let stored = sorted_groups(&entry.groups);
    assert_eq!(instance.agent(entry.id).unwrap().groups, stored);
    assert_eq!(
        instance.tool(pinned.id).unwrap().groups,
        sorted_groups(&pinned.groups)
    );
    assert!(instance.tool(pinned.id).unwrap().pinned);
    assert!(instance.tool(image.id).unwrap().dependency);
    assert_eq!(
        through_client.follows_tools,
        std::collections::BTreeSet::from([latest_root.id, backed_root.id, harness_root.id])
    );
    // the built instance is accepted by the API and tagged with what the resolver followed
    let WorkflowBuild::New(built) = build_workflow_instance(
        &admin,
        created.id,
        &limits,
        WorkflowInstanceTrigger::Manual,
        &io.username,
    )
    .await?
    else {
        panic!("the first build should be new");
    };
    assert_eq!(built.instance.revision, 1);
    println!(
        "resolved instance: {}",
        serde_json::to_string_pretty(&built.instance)?
    );
    let r1 = submit(&admin, &built.instance).await?;
    let stored_r1 = admin.entities.get(r1.id).await?;
    println!("stored {} tags: {:?}", r1.name, stored_r1.tags.keys());
    let mut follows = tag_values(&stored_r1, "InstanceFollowsTool");
    follows.sort();
    let mut expected = built
        .follows_tools
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>();
    expected.sort();
    assert_eq!(follows, expected);
    // building again finds the stored instance current through its tags
    let again = build_workflow_instance(
        &admin,
        created.id,
        &limits,
        WorkflowInstanceTrigger::Manual,
        &io.username,
    )
    .await?;
    assert!(matches!(again, WorkflowBuild::Current { newest, .. } if newest == r1.id));
    // a tag lookup only searches the groups it is given
    let id = created.id.to_string();
    let in_minerva = admin
        .tagged(
            EntityKinds::WorkflowInstance,
            "WorkflowId",
            &id,
            std::slice::from_ref(&minerva),
        )
        .await?;
    assert_eq!(ids_of(&in_minerva), vec![r1.id]);
    let in_mars = admin
        .tagged(
            EntityKinds::WorkflowInstance,
            "WorkflowId",
            &id,
            std::slice::from_ref(&mars),
        )
        .await?;
    assert!(in_mars.is_empty());
    Ok(())
}

/// The toolset resolver gives the same instance through the Thorium client as through an
/// in-memory copy of the same graph: its explicit members, the tools its selector's tag is on in
/// every one of its groups, and every tool left out with why; the API accepts what it builds,
/// and a workflow whose agent follows the toolset resolves with its members and is accepted too
#[tokio::test]
// one scenario builds a toolset and a workflow and checks both through both readers and the API
#[allow(clippy::too_many_lines)]
async fn toolset_resolver_parity() -> Result<(), Error> {
    // io works in minerva and mars, where every tool is
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let both = [minerva.as_str(), mars.as_str()];
    let value = format!("ghidra-{suffix}");
    // two explicit members, one followed at latest and one pinned
    let (scan_root, scan) = agentic_tool(&both, &admin).await?;
    let (_, hash) = agentic_tool(&both, &admin).await?;
    // a tool tagged in both groups, one tagged only in minerva, one that declares an effect,
    // and an Image tool
    let (decompile_root, decompile) = agentic_tool(&both, &admin).await?;
    let (rename_root, _) = agentic_tool(&both, &admin).await?;
    let mut writes = generators::gen_agentic_spec();
    if let ToolSpec::Agentic(agentic) = &mut writes {
        agentic
            .capabilities
            .effects
            .insert(ToolEffect::WritesThorium);
    }
    let (patch_root, patch) = generators::tool(&both, "1.0.0", &writes, &admin).await?;
    let (cli_root, _) =
        generators::tool(&both, "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let tagged: [(&Entity, &[&str]); 4] = [
        (&decompile_root, &both),
        (&patch_root, &both),
        (&cli_root, &both),
        (&rename_root, &both[..1]),
    ];
    for (root, groups) in tagged {
        let tags = json!({"groups": groups, "tags": {"toolset": [value.clone()]}});
        assert_eq!(raw_tags(&io, root.id, false, tags).await?, 204);
    }
    // a read only toolset in both groups with the selector and the two explicit members
    let req = ToolSetEntityRequest::default()
        .tool(scan_root.id)
        .tool(hash.id)
        .read_only(true)
        .selector(ToolSetSelector::new("toolset", value.clone()));
    let created = admin
        .entities
        .create(generators::gen_toolset("ghidra-read-only", &both, req))
        .await?;
    // resolve through the client and through an in-memory copy of the same graph
    let limits = ToolSetLimits::default();
    let through_client = resolve_tool_set(&admin, created.id, &limits).await?;
    let graph = mirror(
        &admin,
        &[
            created.id,
            scan_root.id,
            scan.id,
            hash.id,
            decompile_root.id,
            decompile.id,
            rename_root.id,
            patch_root.id,
            patch.id,
            cli_root.id,
        ],
    )
    .await?;
    let through_fixture = resolve_tool_set(&graph, created.id, &limits).await?;
    assert_eq!(through_client.instance, through_fixture.instance);
    assert_eq!(through_client.dedupe_key(), through_fixture.dedupe_key());
    // the instance is ready with the explicit and selected members
    let instance = &through_client.instance;
    assert!(instance.is_ready(), "{:?}", instance.status);
    let mut members = instance
        .members
        .iter()
        .map(|member| (member.tool_instance, member.inclusion))
        .collect::<Vec<(Uuid, ToolSetInclusion)>>();
    members.sort();
    let mut expected = vec![
        (scan.id, ToolSetInclusion::ExplicitLatest),
        (hash.id, ToolSetInclusion::ExplicitPinned),
        (decompile.id, ToolSetInclusion::Selector),
    ];
    expected.sort();
    assert_eq!(members, expected);
    // the partly tagged tool, the tool with an effect, and the Image tool are left out
    let reasons = instance
        .excluded
        .iter()
        .map(|exclusion| (exclusion.tool, exclusion.reason))
        .collect::<BTreeMap<Uuid, ToolSetExclusionReason>>();
    assert_eq!(
        reasons,
        BTreeMap::from([
            (rename_root.id, ToolSetExclusionReason::NotTaggedInAllGroups),
            (patch_root.id, ToolSetExclusionReason::NotReadOnly),
            (cli_root.id, ToolSetExclusionReason::NotAgentic),
        ])
    );
    let partial = instance
        .excluded
        .iter()
        .find(|exclusion| exclusion.tool == rename_root.id)
        .unwrap();
    assert_eq!(partial.missing_groups, vec![mars.clone()]);
    assert_eq!(partial.describe(), format!("not tagged in `{mars}`"));
    assert_eq!(instance.warnings.len(), 1, "{:?}", instance.warnings);
    // the built instance is accepted by the API and tagged with the tools it follows at latest
    let ToolSetBuild::New(built) = build_tool_set_instance(
        &admin,
        created.id,
        &limits,
        ToolSetInstanceTrigger::Manual,
        &io.username,
    )
    .await?
    else {
        panic!("the first toolset build should be new");
    };
    assert_eq!(built.instance.revision, 1);
    println!(
        "resolved toolset instance: {}",
        serde_json::to_string_pretty(&built.instance)?
    );
    let set_r1 = admin
        .entities
        .create(generators::gen_toolset_instance_req(&built.instance))
        .await?;
    let stored = admin.entities.get(set_r1.id).await?;
    println!("stored {} tags: {:?}", set_r1.name, stored.tags.keys());
    let mut follows = vec![scan_root.id.to_string(), decompile_root.id.to_string()];
    follows.sort();
    assert_eq!(tag_values(&stored, "ToolSetFollowsTool"), follows);
    // building again finds the stored instance current through its tags
    let again = build_tool_set_instance(
        &admin,
        created.id,
        &limits,
        ToolSetInstanceTrigger::Manual,
        &io.username,
    )
    .await?;
    assert!(matches!(again, ToolSetBuild::Current { newest, .. } if newest == set_r1.id));
    // a bounded tag lookup searches only its group and stops at the bound
    let bounded = admin
        .tagged_up_to(
            EntityKinds::Tool,
            "toolset",
            &value,
            std::slice::from_ref(&minerva),
            2,
        )
        .await?;
    assert_eq!(bounded.len(), 2);
    let in_mars = admin
        .tagged_up_to(
            EntityKinds::Tool,
            "toolset",
            &value,
            std::slice::from_ref(&mars),
            10,
        )
        .await?;
    assert_eq!(
        ids(&in_mars),
        sorted(&[decompile_root.id, patch_root.id, cli_root.id])
    );
    // a workflow whose entry agent follows the toolset at latest and pins a harness
    let (_, runtime) = harness_tool(&[&minerva], &admin).await?;
    let lead = agent(&admin, "lead", &[&minerva], |req| {
        req.tool_set(created.id).harness(runtime.id)
    })
    .await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(lead.id)
        .entry_agent(lead.id);
    let catalog = workflow(&io.client, "Ghidra triage", &[&minerva], req).await?;
    let crawl = CrawlLimits::default();
    let wf_client = resolve_workflow(&admin, catalog.id, &crawl).await?;
    let wf_graph = mirror(
        &admin,
        &[
            catalog.id,
            lead.id,
            created.id,
            set_r1.id,
            scan.id,
            hash.id,
            decompile.id,
            runtime.id,
        ],
    )
    .await?;
    let wf_fixture = resolve_workflow(&wf_graph, catalog.id, &crawl).await?;
    assert_eq!(wf_client.instance, wf_fixture.instance);
    assert_eq!(wf_client.dedupe_key(), wf_fixture.dedupe_key());
    // the agent is given the toolset instance's members, pinned as the toolset included them
    let resolved = &wf_client.instance;
    assert!(resolved.is_ready(), "{:?}", resolved.status);
    let snapshot = resolved.agent(lead.id).unwrap();
    assert_eq!(snapshot.tool_sets, vec![set_r1.id]);
    assert_eq!(snapshot.tools, sorted(&[scan.id, hash.id, decompile.id]));
    assert!(resolved.tool(hash.id).unwrap().pinned);
    assert!(!resolved.tool(scan.id).unwrap().pinned);
    assert!(!resolved.tool(decompile.id).unwrap().pinned);
    // the API accepts the built workflow instance and tags the followed toolset only
    let WorkflowBuild::New(built_wf) = build_workflow_instance(
        &admin,
        catalog.id,
        &crawl,
        WorkflowInstanceTrigger::Manual,
        &io.username,
    )
    .await?
    else {
        panic!("the first workflow build should be new");
    };
    println!(
        "resolved workflow instance: {}",
        serde_json::to_string_pretty(&built_wf.instance)?
    );
    let wf_r1 = submit(&admin, &built_wf.instance).await?;
    let stored_wf = admin.entities.get(wf_r1.id).await?;
    assert_eq!(
        tag_values(&stored_wf, "InstanceFollowsToolSet"),
        vec![created.id.to_string()]
    );
    assert_eq!(
        tag_values(&stored_wf, "InstanceFollowsTool"),
        Vec::<String>::new()
    );
    Ok(())
}

/// Get the ids of a list of entities in their order
///
/// # Arguments
///
/// * `entities` - The entities
fn ids_of(entities: &[Entity]) -> Vec<Uuid> {
    // keep the order the entities came in
    entities.iter().map(|entity| entity.id).collect()
}

/// Add one graph edge through the generic association route
///
/// # Arguments
///
/// * `client` - The client to add the edge with
/// * `kind` - The kind of edge
/// * `source` - The edge's source
/// * `target` - The edge's target
async fn generic_edge(
    client: &Thorium,
    kind: AssociationKind,
    source: &Entity,
    target: &Entity,
) -> Result<(), Error> {
    // name both ends as entities
    let request = thorium::models::AssociationRequest::new(
        kind,
        thorium::models::AssociationTarget::Entity {
            id: source.id,
            name: source.name.clone(),
        },
    )
    .target(thorium::models::AssociationTarget::Entity {
        id: target.id,
        name: target.name.clone(),
    });
    client.associations.create(&request).await?;
    Ok(())
}

/// A source can't be given more outgoing graph edges of a kind than a resolution reads: calls
/// past twice the agent tool limit, delegations or orchestrated agents past the workflow agent
/// limit, or includes past what a toolset considers
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn outgoing_edge_caps() -> Result<(), Error> {
    // the default limits, and enough tools and agents to pass each of them
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let crawl = CrawlLimits::default();
    let max_includes = ToolSetLimits::default().max_candidates();
    let mut tools = Vec::new();
    for _ in 0..=max_includes {
        tools.push(agentic_tool(&[&a], &admin).await?.0);
    }
    let mut agents = Vec::new();
    for index in 0..=crawl.max_agents_per_workflow {
        agents.push(agent(&admin, &format!("helper-{index}"), &[&a], |req| req).await?);
    }
    // an agent calling one more tool than twice the tool limit is refused, naming the limit
    let calls = 2 * crawl.max_tools_per_agent;
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_agent(
                "greedy",
                &[&a],
                tools[..=calls].iter().fold(
                    AgentEntityRequest::new(generators::gen_agent_config("greedy")),
                    |req, tool| req.tool(tool.id),
                ),
            ))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("max_tools_per_agent (128)"), "{msg}");
    // at the limit it's created, and one more call through either route is refused
    let full = agent(&admin, "full", &[&a], |req| {
        tools[..calls]
            .iter()
            .fold(req, |req, tool| req.tool(tool.id))
    })
    .await?;
    let (status, msg) =
        refusal(generic_edge(&admin, AssociationKind::Calls, &full, &tools[calls]).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("`Calls`"), "{msg}");
    let (status, _) =
        refusal(update_metadata(&admin, full.id, edge_update(&[tools[calls].id], &[], &[])).await);
    assert_eq!(status, 400);
    // a call removed in the same update makes room for another
    update_metadata(
        &admin,
        full.id,
        edge_update(&[tools[calls].id], &[tools[0].id], &[]),
    )
    .await?;
    // an agent delegating to one more agent than a workflow may reach is refused
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_agent(
                "manager",
                &[&a],
                agents.iter().fold(
                    AgentEntityRequest::new(generators::gen_agent_config("manager")),
                    |req, sub| req.sub_agent(sub.id),
                ),
            ))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("max_agents_per_workflow (32)"), "{msg}");
    // a workflow orchestrating one more agent than its limit is refused
    let req = agents
        .iter()
        .fold(WorkflowCatalogEntityRequest::default(), |req, agent| {
            req.agent(agent.id)
        });
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_workflow("crowded", &[&a], req))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("`Orchestrates`"), "{msg}");
    // at its limit it's created, and one more agent through the generic route is refused
    let req = agents[..crawl.max_agents_per_workflow]
        .iter()
        .fold(WorkflowCatalogEntityRequest::default(), |req, agent| {
            req.agent(agent.id)
        });
    let full_wf = workflow(&admin, "full", &[&a], req).await?;
    let last = &agents[crawl.max_agents_per_workflow];
    let (status, _) =
        refusal(generic_edge(&admin, AssociationKind::Orchestrates, &full_wf, last).await);
    assert_eq!(status, 400);
    // a toolset including one more tool than it considers is refused
    let req = tools
        .iter()
        .fold(ToolSetEntityRequest::default(), |req, tool| {
            req.tool(tool.id)
        });
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("huge", &[&a], req))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("max_tools_per_toolset (64)"), "{msg}");
    Ok(())
}

/// A source already over an edge cap can drop edges a few at a time and swap one edge for
/// another, but can't gain more
#[tokio::test]
async fn over_cap_source_sheds_edges() -> Result<(), Error> {
    // an agent at its call cap, and two more tools
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let calls = 2 * CrawlLimits::default().max_tools_per_agent;
    let mut tools = Vec::new();
    for _ in 0..calls + 3 {
        tools.push(agentic_tool(&[&a], &admin).await?.0);
    }
    let full = agent(&admin, "full", &[&a], |req| {
        tools[..calls]
            .iter()
            .fold(req, |req, tool| req.tool(tool.id))
    })
    .await?;
    // two calls written past the cap, as before the cap or a lowered limit
    for tool in &tools[calls..calls + 2] {
        test_utilities::faults::write_edges_uncapped(full.id);
        generic_edge(&admin, AssociationKind::Calls, &full, tool).await?;
    }
    // removing one call, which still leaves it over the cap, works
    update_metadata(&admin, full.id, edge_update(&[], &[tools[0].id], &[])).await?;
    // so does swapping one call for another
    update_metadata(
        &admin,
        full.id,
        edge_update(&[tools[calls + 2].id], &[tools[1].id], &[]),
    )
    .await?;
    // gaining a call is still refused
    let (status, msg) =
        refusal(update_metadata(&admin, full.id, edge_update(&[tools[0].id], &[], &[])).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("`Calls`"), "{msg}");
    Ok(())
}

/// Build an agent update that adds and removes tool calls and sub-agents
///
/// # Arguments
///
/// * `add_tools` - The tools to add calls to
/// * `remove_tools` - The tools to remove calls to
/// * `add_sub_agents` - The agents to delegate to
fn edge_update(
    add_tools: &[Uuid],
    remove_tools: &[Uuid],
    add_sub_agents: &[Uuid],
) -> EntityMetadataUpdate {
    // only the edge fields are set
    EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: add_tools.to_vec(),
        remove_tools: remove_tools.to_vec(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: add_sub_agents.to_vec(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    }
}
