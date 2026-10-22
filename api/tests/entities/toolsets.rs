//! Integration tests for toolsets and toolset instances
//!
//! These cover creating toolsets with their `Includes` edges and the rules those edges follow,
//! agents attaching toolsets, the selector matching only tools tagged in every one of a
//! toolset's groups, editing and deleting toolsets, and submitted toolset instances: their
//! permissions, member and snapshot checks, digest, revisions, revision lock, intrinsic tags,
//! and the limits on editing them.

use reqwest::multipart::Form;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use thorium::models::{
    AgentEntity, AgentEntityRequest, AssociationKind, AssociationRequest, AssociationTarget,
    Directionality, Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataUpdate,
    EntityResponse, EntityUpdate, SelectorMatches, ToolSetEntity, ToolSetEntityRequest,
    ToolSetExclusion, ToolSetExclusionReason, ToolSetInclusion, ToolSetInstanceEntity,
    ToolSetInstanceStatus, ToolSetSelector, UserRole, WorkflowGraphChange,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser, god_group, moon_user};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

mod common;

use common::{
    QueuedChange, agentic_tool, changes, changes_since, edges, ids, ordered_ids, queued,
    release_later, sorted, timed, timed_refusal, update_metadata,
};

/// Create a toolset and get it back
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
) -> Result<Entity, Error> {
    // create the toolset and read it back in all of its groups
    let resp = client
        .entities
        .create(generators::gen_toolset(name, groups, req))
        .await?;
    client.entities.get(resp.id).await
}

/// Submit a toolset instance
///
/// # Arguments
///
/// * `client` - The client to submit with
/// * `instance` - The instance to submit
async fn submit(
    client: &Thorium,
    instance: &ToolSetInstanceEntity,
) -> Result<EntityResponse, Error> {
    // send the instance in its toolset's groups
    client
        .entities
        .create(generators::gen_toolset_instance_req(instance))
        .await
}

/// Get a toolset's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The toolset entity
fn meta(entity: &Entity) -> &ToolSetEntity {
    // only toolsets have toolset metadata
    match &entity.metadata {
        EntityMetadata::ToolSet(tool_set) => tool_set,
        other => panic!("expected a toolset, got {other:?}"),
    }
}

/// Get a toolset instance's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The toolset instance entity
fn instance_meta(entity: &Entity) -> &ToolSetInstanceEntity {
    // only toolset instances have toolset instance metadata
    match &entity.metadata {
        EntityMetadata::ToolSetInstance(instance) => instance,
        other => panic!("expected a toolset instance, got {other:?}"),
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

/// Get an entity's raw JSON as a user
///
/// # Arguments
///
/// * `user` - The user to read as
/// * `id` - The entity to read
async fn raw_get(user: &TestUser, id: Uuid) -> Result<(u16, serde_json::Value), Error> {
    // read the entity as the user
    let resp = reqwest::Client::new()
        .get(format!("{}/api/entities/{id}", user.client.host))
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
    // build the tag request for this entity
    let url = format!("{}/api/entities/tags/{id}", user.client.host);
    let client = reqwest::Client::new();
    let req = if delete {
        client.delete(url)
    } else {
        client.post(url)
    };
    // send it as the user
    let resp = req
        .header("authorization", user.auth_header())
        .json(&tags)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Get the values of one of an entity's tags
///
/// # Arguments
///
/// * `entity` - The entity to read
/// * `key` - The tag key
fn tag_values(entity: &Entity, key: &str) -> Vec<String> {
    // collect the values in every group, sorted and deduplicated
    let mut values = entity.tags.get(key).map_or_else(Vec::new, |values| {
        values.keys().cloned().collect::<Vec<String>>()
    });
    values.sort();
    values
}

/// Build an empty toolset metadata update
fn toolset_update() -> EntityMetadataUpdate {
    // an update that changes nothing
    EntityMetadataUpdate::ToolSet {
        read_only: None,
        selector: None,
        clear_selector: None,
        auto_instance: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        extend_target_groups: false,
    }
}

/// Build an agent request that calls some toolsets
///
/// # Arguments
///
/// * `function_name` - The agent's delegation function name
/// * `tool_sets` - The toolsets and toolset instances to call
fn agent_req(function_name: &str, tool_sets: &[Uuid]) -> AgentEntityRequest {
    // a config with toolset edges
    let mut req = AgentEntityRequest::new(generators::gen_agent_config(function_name));
    req.tool_sets = tool_sets.to_vec();
    req
}

/// Deprecate or un-deprecate a tool instance
///
/// # Arguments
///
/// * `client` - The client to send the update with
/// * `id` - The tool instance
/// * `deprecated` - Whether the instance is deprecated
async fn deprecate(client: &Thorium, id: Uuid, deprecated: bool) -> Result<(), Error> {
    // set the instance's deprecated flag
    let meta = EntityMetadataUpdate::ToolInstance {
        deprecated: Some(deprecated),
    };
    client
        .entities
        .update(id, EntityUpdate::default().metadata(meta))
        .await?;
    Ok(())
}

/// List the toolsets and toolset instances a user can find in a listing
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `groups` - The groups to list in
async fn listed(client: &Thorium, groups: &[String]) -> Result<Vec<Uuid>, Error> {
    // list both toolset kinds
    let opts = EntityListOpts::default()
        .groups(groups.to_vec())
        .kinds([EntityKinds::ToolSet, EntityKinds::ToolSetInstance])
        .limit(1000);
    let found = client.entities.list(&opts).await?;
    Ok(found.data.into_iter().map(|line| line.id).collect())
}

/// Sort queued changes so changes queued by one request can be compared in any order
///
/// # Arguments
///
/// * `changes` - The changes to sort
fn sorted_changes(mut changes: Vec<QueuedChange>) -> Vec<QueuedChange> {
    // order by the change's debug form, which is stable for equal changes
    changes.sort_by_key(|queued| format!("{:?}", queued.change));
    changes
}

/// Request a manual instance of a toolset as a user and get the raw status and body
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `id` - The id of the toolset to instance
async fn raw_request_instance(user: &TestUser, id: Uuid) -> Result<(u16, String), Error> {
    // post to the manual toolset instance route as the user
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/entities/toolsets/{id}/instance",
            user.client.host
        ))
        .header("authorization", user.auth_header())
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// Widening a toolset through an agent's edge queues one change of the toolset in its groups
/// after the widening
#[tokio::test]
async fn widened_toolset_queues_change() -> Result<(), Error> {
    // a toolset in vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let minerva = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let vesta = god_group(&admin, "vesta", &suffix, &GroupRoles::default()).await?;
    let hoard = toolset(&admin, "hoard", &[&vesta], ToolSetEntityRequest::default()).await?;
    let seen = changes(&admin, hoard.id).await?.len();
    // an agent in minerva and vesta calls the toolset, widening it into minerva
    let req = agent_req("caller", &[hoard.id]).extend_target_groups();
    admin
        .entities
        .create(generators::gen_agent("caller", &[&minerva, &vesta], req))
        .await?;
    // the widening queued one toolset change in both groups
    let username = admin.users.info().await?.username;
    let widened = changes_since(&admin, hoard.id, seen)
        .await?
        .into_iter()
        .filter(|change| matches!(change.change, WorkflowGraphChange::ToolSetChanged { .. }))
        .collect::<Vec<QueuedChange>>();
    assert_eq!(
        widened,
        vec![queued(
            &username,
            &[&minerva, &vesta],
            WorkflowGraphChange::ToolSetChanged { tool_set: hoard.id },
        )]
    );
    Ok(())
}

/// The manual toolset instance route queues one request in every group of the toolset for a
/// user who can edit all of them, and refuses everyone else without queueing anything
#[tokio::test]
async fn manual_instance_route() -> Result<(), Error> {
    // europa edits minerva and mars, callisto edits only minerva, and amalthea sees neither
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
    let (tool_root, _) = agentic_tool(&groups, &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool_root.id);
    let created = toolset(&admin, "manual", &groups, req).await?;
    let caller = admin
        .entities
        .create(generators::gen_agent(
            "manual-caller",
            &groups,
            agent_req("manual_caller", &[]),
        ))
        .await?;
    let seen = changes(&admin, created.id).await?.len();
    // europa's request is accepted with an empty body and queues one request in both groups
    let (status, body) = raw_request_instance(&europa, created.id).await?;
    assert_eq!(status, 202);
    assert_eq!(body, String::new());
    let manual = queued(
        &europa.username,
        &groups,
        WorkflowGraphChange::ManualToolSetInstance {
            tool_set: created.id,
        },
    );
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![manual.clone()]
    );
    // the client method sends the same request
    europa
        .client
        .entities
        .request_tool_set_instance(created.id)
        .await?;
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![manual.clone(), manual]
    );
    let seen = seen + 2;
    // an unknown id, a tool, and an agent aren't toolsets
    assert_eq!(raw_request_instance(&europa, Uuid::new_v4()).await?.0, 404);
    assert_eq!(raw_request_instance(&europa, tool_root.id).await?.0, 404);
    assert_eq!(raw_request_instance(&europa, caller.id).await?.0, 404);
    // a workflow isn't a toolset either, and a toolset isn't a workflow
    let flow = admin
        .entities
        .create(generators::gen_workflow(
            "manual-flow",
            &groups,
            thorium::models::WorkflowCatalogEntityRequest::default(),
        ))
        .await?;
    assert_eq!(raw_request_instance(&europa, flow.id).await?.0, 404);
    let status = europa
        .client
        .entities
        .request_instance(created.id)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(status, Some(404));
    // a user outside every group of the toolset can't see it
    assert_eq!(raw_request_instance(&amalthea, created.id).await?.0, 404);
    // a user who can see it but edits only one of its groups is refused
    let (status, body) = raw_request_instance(&callisto, created.id).await?;
    assert_eq!(status, 401);
    assert!(body.contains("manual"), "{body}");
    // none of the refused requests queued anything
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 0);
    Ok(())
}

/// A submitted instance that is Invalid queues nothing, and a Ready one queues one update of
/// its toolset recorded as caused by the user it names
#[tokio::test]
async fn only_ready_instances_queue_updates() -> Result<(), Error> {
    // a toolset with one member
    let admin = test_utilities::admin_client().await?;
    let admin_name = admin.users.info().await?.username;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "ready-only", &[&a], ToolSetEntityRequest::default()).await?;
    let seen = changes(&admin, created.id).await?.len();
    // an invalid instance is stored but queues nothing
    let mut invalid = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    invalid.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["broken on purpose".to_owned()],
    };
    let r1 = submit(&admin, &invalid).await?;
    assert_eq!(changes_since(&admin, created.id, seen).await?, Vec::new());
    assert_eq!(changes(&admin, r1.id).await?, Vec::new());
    // a ready instance queues one update naming it, caused by the user it names
    let mut ready = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]);
    ready.triggered_by = "europa-named".to_owned();
    let r2 = submit(&admin, &ready).await?;
    let update = queued(
        "europa-named",
        &[&a],
        WorkflowGraphChange::ToolSetUpdated {
            tool_set: created.id,
            tool_set_instance: r2.id,
        },
    );
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![update.clone()]
    );
    assert_eq!(changes(&admin, r2.id).await?, vec![update]);
    // an instance submitted without naming anyone is caused by its submitter
    let r3 = generators::gen_toolset_instance(&created, 3, Some(r2.id), &[&tool]);
    let r3 = submit(&admin, &r3).await?;
    let queued_r3 = changes(&admin, r3.id).await?;
    assert_eq!(queued_r3.len(), 1, "{queued_r3:?}");
    assert_eq!(queued_r3[0].user, admin_name);
    Ok(())
}

/// Creating a toolset creates its `Includes` edges, lists both tools, and its first instance
/// gets exactly the toolset's groups
#[tokio::test]
async fn create_with_includes() -> Result<(), Error> {
    // a toolset in minerva and mars that includes one tool at latest and another pinned
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (latest, _) = agentic_tool(&[&a, &b], &admin).await?;
    let (_, pinned) = agentic_tool(&[&a, &b], &admin).await?;
    let req = ToolSetEntityRequest::default()
        .tool(latest.id)
        .tool(pinned.id);
    let created = toolset(&admin, "ghidra-read-only", &[&a, &b], req).await?;
    // creating it queued its own change and one edge change per include, in both groups
    let admin_name = admin.users.info().await?.username;
    let include = |target: Uuid| WorkflowGraphChange::EdgeChanged {
        source: created.id,
        target,
        kind: AssociationKind::Includes,
    };
    assert_eq!(
        sorted_changes(changes(&admin, created.id).await?),
        sorted_changes(vec![
            queued(
                &admin_name,
                &[&a, &b],
                WorkflowGraphChange::ToolSetChanged {
                    tool_set: created.id
                }
            ),
            queued(&admin_name, &[&a, &b], include(latest.id)),
            queued(&admin_name, &[&a, &b], include(pinned.id)),
        ])
    );
    // both tools are listed, the toolset was last modified when it was created, and it's
    // in both groups
    assert_eq!(ids(&meta(&created).tools), sorted(&[latest.id, pinned.id]));
    assert_eq!(meta(&created).modified, created.created);
    assert!(meta(&created).auto_instance);
    assert!(!meta(&created).read_only);
    assert_eq!(meta(&created).latest_instance, None::<Uuid>);
    assert_eq!(
        sorted_groups(&created.groups),
        sorted_groups(&[a.clone(), b.clone()])
    );
    // the includes are stored as edges from the toolset
    let out = edges(
        &admin,
        created.id,
        AssociationKind::Includes,
        Directionality::To,
    )
    .await?;
    assert_eq!(out, sorted(&[latest.id, pinned.id]));
    let back = edges(
        &admin,
        pinned.id,
        AssociationKind::Includes,
        Directionality::From,
    )
    .await?;
    assert_eq!(back, vec![created.id]);
    // a submitted instance gets exactly the toolset's groups and becomes its latest
    let latest_instance = admin.entities.get(latest.id).await?;
    let latest_instance = match &latest_instance.metadata {
        EntityMetadata::Tool(tool) => admin.entities.get(tool.latest.unwrap()).await?,
        other => panic!("expected a tool, got {other:?}"),
    };
    let instance =
        generators::gen_toolset_instance(&created, 1, None, &[&latest_instance, &pinned]);
    let resp = submit(&admin, &instance).await?;
    let stored = admin.entities.get(resp.id).await?;
    assert_eq!(
        sorted_groups(&stored.groups),
        sorted_groups(&created.groups)
    );
    assert_eq!(stored.kind, EntityKinds::ToolSetInstance);
    let after = admin.entities.get(created.id).await?;
    assert_eq!(meta(&after).latest_instance, Some(resp.id));
    Ok(())
}

/// `Includes` edges only point at agentic tools, never both ways at one tool, never at a tool
/// missing the toolset's groups, and never at a toolset
#[tokio::test]
async fn include_refusals() -> Result<(), Error> {
    // a toolset's groups and the tools it might include
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (image, _) =
        generators::tool(&[&a, &b], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (agentic, agentic_instance) = agentic_tool(&[&a, &b], &admin).await?;
    let (narrow, _) = agentic_tool(&[&a], &admin).await?;
    let other = toolset(&admin, "other", &[&a, &b], ToolSetEntityRequest::default()).await?;
    // an Image tool is refused
    let req = ToolSetEntityRequest::default().tool(image.id);
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("images", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("toolsets only include Agentic tools"), "{msg}");
    // a tool at latest and one of its instances is refused
    let req = ToolSetEntityRequest::default()
        .tool(agentic.id)
        .tool(agentic_instance.id);
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("both-ways", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("can't include"), "{msg}");
    // a tool missing one of the toolset's groups is refused, naming the group
    let req = ToolSetEntityRequest::default().tool(narrow.id);
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("narrow", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains(&b), "{msg}");
    // a toolset can't include another toolset
    let req = ToolSetEntityRequest::default().tool(other.id);
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("nested", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("toolset_tools"), "{msg}");
    // none of the refused toolsets was written
    let opts = EntityListOpts::default()
        .groups(vec![a.clone(), b.clone()])
        .kind(EntityKinds::ToolSet)
        .limit(100);
    let names = admin
        .entities
        .list(&opts)
        .await?
        .data
        .into_iter()
        .map(|line| line.name)
        .collect::<Vec<String>>();
    assert_eq!(names, vec!["other".to_owned()]);
    // the same rules hold when an update adds the include
    let mut add = toolset_update();
    if let EntityMetadataUpdate::ToolSet { add_tools, .. } = &mut add {
        add_tools.push(image.id);
    }
    let (status, _) = refusal(update_metadata(&admin, other.id, add).await);
    assert_eq!(status, 400);
    assert_eq!(meta(&admin.entities.get(other.id).await?).tools.len(), 0);
    Ok(())
}

/// A submitted instance carries its toolset's intrinsic tags, which users can't remove
#[tokio::test]
async fn intrinsic_tags() -> Result<(), Error> {
    // a toolset with a selector and an instance with a latest and a pinned member
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
    let selector = ToolSetSelector::new("toolset", "ghidra");
    let req = ToolSetEntityRequest::default().selector(selector.clone());
    let created = toolset(&admin, "ghidra", &[&a], req).await?;
    let (latest_tool, latest) = agentic_tool(&[&a], &admin).await?;
    let (pinned_tool, pinned) = agentic_tool(&[&a], &admin).await?;
    let mut instance = generators::gen_toolset_instance(&created, 1, None, &[&latest, &pinned]);
    for member in &mut instance.members {
        if member.tool_instance == pinned.id {
            member.inclusion = ToolSetInclusion::ExplicitPinned;
        }
    }
    instance.digest = instance.compute_digest()?;
    let resp = europa
        .client
        .entities
        .create(generators::gen_toolset_instance_req(&instance))
        .await?;
    // every intrinsic tag is written, and only the latest member is followed
    let stored = admin.entities.get(resp.id).await?;
    assert_eq!(
        tag_values(&stored, "ToolSetId"),
        vec![created.id.to_string()]
    );
    assert_eq!(tag_values(&stored, "ToolSetRevision"), vec!["1".to_owned()]);
    assert_eq!(
        tag_values(&stored, "ToolSetDigest"),
        vec![instance.digest.clone()]
    );
    assert_eq!(
        tag_values(&stored, "ToolSetStatus"),
        vec!["Ready".to_owned()]
    );
    assert_eq!(
        tag_values(&stored, "ToolSetSelector"),
        vec!["toolset=ghidra".to_owned()]
    );
    assert_eq!(
        tag_values(&stored, "ToolSetFollowsTool"),
        vec![latest_tool.id.to_string()]
    );
    assert!(!tag_values(&stored, "ToolSetFollowsTool").contains(&pinned_tool.id.to_string()));
    // the name is the toolset's name and revision
    assert_eq!(stored.name, "ghidra#r1".to_owned());
    // a user can't remove the intrinsic tags but can add and remove their own
    for (key, value) in [
        ("ToolSetId", created.id.to_string()),
        ("ToolSetStatus", "Ready".to_owned()),
        ("ToolSetFollowsTool", latest_tool.id.to_string()),
    ] {
        let status = raw_tags(&europa, resp.id, true, json!({"tags": {key: [value]}})).await?;
        assert_eq!(status, 400);
    }
    let own = json!({"tags": {"Owner": ["europa"]}});
    assert_eq!(raw_tags(&europa, resp.id, false, own.clone()).await?, 204);
    assert_eq!(raw_tags(&europa, resp.id, true, own).await?, 204);
    let after = admin.entities.get(resp.id).await?;
    assert_eq!(
        tag_values(&after, "ToolSetId"),
        vec![created.id.to_string()]
    );
    Ok(())
}

/// Agents call toolsets at latest or pinned to a ready instance, never both ways, and only
/// toolsets that cover their groups
#[tokio::test]
async fn agent_calls_toolsets() -> Result<(), Error> {
    // two toolsets in minerva and mars, one with a ready instance and one with an invalid one
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (_, tool) = agentic_tool(&[&a, &b], &admin).await?;
    let followed = toolset(
        &admin,
        "followed",
        &[&a, &b],
        ToolSetEntityRequest::default(),
    )
    .await?;
    let pinned_set = toolset(&admin, "pinned", &[&a, &b], ToolSetEntityRequest::default()).await?;
    let ready = generators::gen_toolset_instance(&pinned_set, 1, None, &[&tool]);
    let ready_id = submit(&admin, &ready).await?.id;
    let mut invalid = generators::gen_toolset_instance(&pinned_set, 2, Some(ready_id), &[&tool]);
    invalid.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["a member has no latest".into()],
    };
    let invalid_id = submit(&admin, &invalid).await?.id;
    // an agent calls one toolset at latest and the other's ready instance
    let req = agent_req("toolset_user", &[followed.id, ready_id]);
    let agent = io
        .client
        .entities
        .create(generators::gen_agent("toolset-user", &[&a, &b], req))
        .await?;
    let stored = io.client.entities.get(agent.id).await?;
    let tool_sets = &agent_meta(&stored).tool_sets;
    assert_eq!(ids(tool_sets), sorted(&[followed.id, ready_id]));
    let kinds = tool_sets
        .iter()
        .map(|entity| (entity.id, entity.kind))
        .collect::<BTreeMap<Uuid, EntityKinds>>();
    assert_eq!(kinds[&followed.id], EntityKinds::ToolSet);
    assert_eq!(kinds[&ready_id], EntityKinds::ToolSetInstance);
    let calls = edges(&admin, agent.id, AssociationKind::Calls, Directionality::To).await?;
    assert_eq!(calls, sorted(&[followed.id, ready_id]));
    // calling the pinned toolset at latest too is refused
    let add_latest = EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        add_tool_sets: vec![pinned_set.id],
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    };
    let (status, msg) = refusal(
        io.client
            .entities
            .update(agent.id, EntityUpdate::default().metadata(add_latest))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("at its latest version and pinned"), "{msg}");
    // an invalid instance can't be pinned
    let req = agent_req("invalid_user", &[invalid_id]);
    let (status, msg) = refusal(
        io.client
            .entities
            .create(generators::gen_agent("invalid-user", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("Invalid"), "{msg}");
    // a toolset that misses one of the agent's groups is refused
    let narrow = toolset(&admin, "narrow", &[&a], ToolSetEntityRequest::default()).await?;
    let req = agent_req("narrow_user", &[narrow.id]);
    let (status, msg) = refusal(
        io.client
            .entities
            .create(generators::gen_agent("narrow-user", &[&a, &b], req))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains(&b), "{msg}");
    // the agent's edges didn't change
    let calls = edges(&admin, agent.id, AssociationKind::Calls, Directionality::To).await?;
    assert_eq!(calls, sorted(&[followed.id, ready_id]));
    Ok(())
}

/// A valid submitted instance is created, a wrong digest or spec hash is a `400`, a revision gap
/// is a `409`, and an instance only takes a new name or description
#[tokio::test]
async fn submitted_instances() -> Result<(), Error> {
    // a toolset with one member
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "radare", &[&a], ToolSetEntityRequest::default()).await?;
    let seen = changes(&admin, created.id).await?.len();
    // a wrong digest is refused
    let mut wrong_digest = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    wrong_digest.digest = format!("v1:{}", "0".repeat(64));
    let (status, msg) = refusal(submit(&admin, &wrong_digest).await);
    assert_eq!(status, 400);
    assert!(msg.contains("digest"), "{msg}");
    // a wrong spec hash is refused, even with a digest that matches it
    let mut wrong_hash = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    wrong_hash.members[0].spec_hash = format!("v1:{}", "a".repeat(64));
    wrong_hash.digest = wrong_hash.compute_digest()?;
    let (status, msg) = refusal(submit(&admin, &wrong_hash).await);
    assert_eq!(status, 400);
    assert!(msg.contains("spec_hash"), "{msg}");
    // a revision gap is a conflict
    let gap = generators::gen_toolset_instance(&created, 2, None, &[&tool]);
    let (status, _) = refusal(submit(&admin, &gap).await);
    assert_eq!(status, 409);
    // nothing was written by the refusals
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).latest_instance,
        None::<Uuid>
    );
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
    assert_eq!(changes_since(&admin, created.id, seen).await?, Vec::new());
    // a valid instance is created
    let valid = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    let resp = submit(&admin, &valid).await?;
    let seen = changes(&admin, created.id).await?.len();
    let stored = admin.entities.get(resp.id).await?;
    assert_eq!(instance_meta(&stored).digest.clone(), valid.digest.clone());
    assert_eq!(
        instance_meta(&stored).triggered_by.clone(),
        "thorium".to_owned()
    );
    // a metadata field can't be patched
    let mut read_only = toolset_update();
    if let EntityMetadataUpdate::ToolSet {
        read_only: field, ..
    } = &mut read_only
    {
        *field = Some(true);
    }
    let (status, msg) = refusal(update_metadata(&admin, resp.id, read_only).await);
    assert_eq!(status, 400);
    assert!(msg.contains("metadata can't change"), "{msg}");
    let deprecated = EntityMetadataUpdate::ToolInstance {
        deprecated: Some(true),
    };
    let (status, _) = refusal(update_metadata(&admin, resp.id, deprecated).await);
    assert_eq!(status, 400);
    // its groups follow its toolset
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (status, _) = refusal(
        admin
            .entities
            .update(resp.id, EntityUpdate::default().group(&b))
            .await,
    );
    assert_eq!(status, 400);
    // its name and description can change
    admin
        .entities
        .update(
            resp.id,
            EntityUpdate::default()
                .name("radare-pinned")
                .description("the first build"),
        )
        .await?;
    let renamed = admin.entities.get(resp.id).await?;
    assert_eq!(renamed.name, "radare-pinned".to_owned());
    // none of the edits of the instance queued anything
    assert_eq!(changes_since(&admin, created.id, seen).await?, Vec::new());
    assert_eq!(renamed.description, Some("the first build".to_owned()));
    assert_eq!(
        instance_meta(&renamed).clone(),
        instance_meta(&stored).clone()
    );
    Ok(())
}

/// A submitted instance that isn't JSON or isn't a toolset instance is refused and writes nothing
#[tokio::test]
async fn malformed_instances() -> Result<(), Error> {
    // a toolset a user can submit instances of
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
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "binja", &[&a], ToolSetEntityRequest::default()).await?;
    // an instance that isn't JSON is refused
    let fields = [
        ("kind", "ToolSetInstance".to_owned()),
        ("groups[]", a.clone()),
        ("metadata[toolset_instance]", "not json".to_owned()),
    ];
    let (status, body) = raw_form(&callisto, None, &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("toolset_instance"), "{body}");
    // JSON without a digest is refused
    let instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    let mut value = serde_json::to_value(&instance)?;
    value.as_object_mut().unwrap().remove("digest");
    let fields = [
        ("kind", "ToolSetInstance".to_owned()),
        ("groups[]", a.clone()),
        ("metadata[toolset_instance]", value.to_string()),
    ];
    let (status, body) = raw_form(&callisto, None, &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("digest"), "{body}");
    // a form without the instance is refused
    let fields = [
        ("kind", "ToolSetInstance".to_owned()),
        ("groups[]", a.clone()),
    ];
    let (status, _) = raw_form(&callisto, None, &fields).await?;
    assert_eq!(status, 400);
    // nothing was written
    assert_eq!(
        listed(&admin, std::slice::from_ref(&a)).await?,
        vec![created.id]
    );
    Ok(())
}

/// A toolset an agent calls can't be deleted until the agent is; deleting it then removes its
/// includes and leaves its instances
#[tokio::test]
async fn delete_toolset() -> Result<(), Error> {
    // a toolset that includes a tool, an agent that calls it, and one instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (tool_root, tool) = agentic_tool(&[&a], &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool_root.id);
    let created = toolset(&admin, "doomed", &[&a], req).await?;
    let instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    let instance_id = submit(&admin, &instance).await?.id;
    let agent = admin
        .entities
        .create(generators::gen_agent(
            "caller",
            &[&a],
            agent_req("caller", &[created.id]),
        ))
        .await?;
    // the delete is refused naming the agent, and the toolset keeps its edges
    let (status, msg) = refusal(admin.entities.delete(created.id).await);
    assert_eq!(status, 409);
    assert!(msg.contains("caller"), "{msg}");
    admin.entities.get(created.id).await?;
    assert_eq!(
        edges(&admin, agent.id, AssociationKind::Calls, Directionality::To).await?,
        vec![created.id]
    );
    // deleting the agent queues its removed call as one edge change
    let admin_name = admin.users.info().await?.username;
    let seen_agent = changes(&admin, agent.id).await?.len();
    admin.entities.delete(agent.id).await?;
    assert_eq!(
        changes_since(&admin, agent.id, seen_agent).await?,
        vec![queued(
            &admin_name,
            &[&a],
            WorkflowGraphChange::EdgeChanged {
                source: agent.id,
                target: created.id,
                kind: AssociationKind::Calls,
            }
        )]
    );
    // deleting the toolset queues its removed include as one edge change
    let seen_tool = changes(&admin, tool_root.id).await?.len();
    admin.entities.delete(created.id).await?;
    assert_eq!(
        changes_since(&admin, tool_root.id, seen_tool).await?,
        vec![queued(
            &admin_name,
            &[&a],
            WorkflowGraphChange::EdgeChanged {
                source: created.id,
                target: tool_root.id,
                kind: AssociationKind::Includes,
            }
        )]
    );
    // its includes are gone
    assert_eq!(
        edges(
            &admin,
            tool_root.id,
            AssociationKind::Includes,
            Directionality::From
        )
        .await?,
        Vec::<Uuid>::new()
    );
    // the toolset is gone and its instance is still readable
    let (status, _) = refusal(admin.entities.get(created.id).await);
    assert_eq!(status, 404);
    let kept = admin.entities.get(instance_id).await?;
    assert_eq!(instance_meta(&kept).tool_set, created.id);
    Ok(())
}

/// Name, description, tag, and setting edits change exactly what they name and build no
/// instance; the selector is cleared or replaced as a whole
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn settings_edits() -> Result<(), Error> {
    // a toolset with a selector and one included tool
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (first, _) = agentic_tool(&[&a], &admin).await?;
    let (second, _) = agentic_tool(&[&a], &admin).await?;
    let req = ToolSetEntityRequest::default()
        .tool(first.id)
        .selector(ToolSetSelector::new("toolset", "ghidra"));
    let created = toolset(&admin, "editable", &[&a], req).await?;
    let admin_name = admin.users.info().await?.username;
    let settings_change = queued(
        &admin_name,
        &[&a],
        WorkflowGraphChange::ToolSetChanged {
            tool_set: created.id,
        },
    );
    let seen = changes(&admin, created.id).await?.len();
    // a name, description, and user tag edit builds no instance
    admin
        .entities
        .update(
            created.id,
            EntityUpdate::default()
                .name("edited")
                .description("renamed"),
        )
        .await?;
    let tagger = generators::named_user_with_role(
        &admin,
        &generators::readable_name("thebe", &suffix),
        UserRole::Admin,
    )
    .await?;
    let tags = json!({"groups": [a.clone()], "tags": {"Team": ["red"]}});
    assert_eq!(raw_tags(&tagger, created.id, false, tags).await?, 204);
    let edited = admin.entities.get(created.id).await?;
    assert_eq!(edited.name, "edited".to_owned());
    assert_eq!(changes_since(&admin, created.id, seen).await?, Vec::new());
    assert_eq!(meta(&edited).latest_instance, None::<Uuid>);
    assert!(meta(&edited).modified > created.created);
    assert_eq!(
        listed(&admin, std::slice::from_ref(&a)).await?,
        vec![created.id]
    );
    // auto_instance and read_only change on their own
    let mut settings = toolset_update();
    if let EntityMetadataUpdate::ToolSet {
        auto_instance,
        read_only,
        ..
    } = &mut settings
    {
        *auto_instance = Some(false);
        *read_only = Some(true);
    }
    update_metadata(&admin, created.id, settings).await?;
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![settings_change.clone()]
    );
    let changed = admin.entities.get(created.id).await?;
    assert!(!meta(&changed).auto_instance);
    assert!(meta(&changed).read_only);
    assert_eq!(
        meta(&changed).selector.clone(),
        Some(ToolSetSelector::new("toolset", "ghidra"))
    );
    // a different selector replaces the pair
    let mut replace = toolset_update();
    if let EntityMetadataUpdate::ToolSet { selector, .. } = &mut replace {
        *selector = Some(ToolSetSelector::new("suite", "radare"));
    }
    update_metadata(&admin, created.id, replace).await?;
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![settings_change.clone(), settings_change.clone()]
    );
    let replaced = admin.entities.get(created.id).await?;
    assert_eq!(
        meta(&replaced).selector.clone(),
        Some(ToolSetSelector::new("suite", "radare"))
    );
    // clearing the selector removes it
    let mut clear = toolset_update();
    if let EntityMetadataUpdate::ToolSet { clear_selector, .. } = &mut clear {
        *clear_selector = Some(true);
    }
    update_metadata(&admin, created.id, clear).await?;
    assert_eq!(changes_since(&admin, created.id, seen).await?.len(), 3);
    let seen = seen + 3;
    let (_, raw) = raw_get(&tagger, created.id).await?;
    assert!(
        raw["metadata"]["ToolSet"].get("selector").is_none(),
        "{raw}"
    );
    // setting and clearing a selector at once is refused
    let mut both = toolset_update();
    if let EntityMetadataUpdate::ToolSet {
        selector,
        clear_selector,
        ..
    } = &mut both
    {
        *selector = Some(ToolSetSelector::new("a", "b"));
        *clear_selector = Some(true);
    }
    let (status, _) = refusal(update_metadata(&admin, created.id, both).await);
    assert_eq!(status, 400);
    // including and removing tools changes exactly those edges
    let mut swap = toolset_update();
    if let EntityMetadataUpdate::ToolSet {
        add_tools,
        remove_tools,
        ..
    } = &mut swap
    {
        add_tools.push(second.id);
        remove_tools.push(first.id);
    }
    update_metadata(&admin, created.id, swap).await?;
    // swapping tools queues an edge change for each and no settings change
    let include = |target: Uuid| WorkflowGraphChange::EdgeChanged {
        source: created.id,
        target,
        kind: AssociationKind::Includes,
    };
    assert_eq!(
        sorted_changes(changes_since(&admin, created.id, seen).await?),
        sorted_changes(vec![
            queued(&admin_name, &[&a], include(first.id)),
            queued(&admin_name, &[&a], include(second.id)),
        ])
    );
    let seen = seen + 2;
    let swapped = admin.entities.get(created.id).await?;
    assert_eq!(ids(&meta(&swapped).tools), vec![second.id]);
    assert_eq!(
        edges(
            &admin,
            created.id,
            AssociationKind::Includes,
            Directionality::To
        )
        .await?,
        vec![second.id]
    );
    // removing a tool it doesn't include and adding one it does change nothing
    let mut noop = toolset_update();
    if let EntityMetadataUpdate::ToolSet {
        add_tools,
        remove_tools,
        ..
    } = &mut noop
    {
        add_tools.push(second.id);
        remove_tools.push(first.id);
    }
    update_metadata(&admin, created.id, noop).await?;
    assert_eq!(changes_since(&admin, created.id, seen).await?, Vec::new());
    assert_eq!(
        edges(
            &admin,
            created.id,
            AssociationKind::Includes,
            Directionality::To
        )
        .await?,
        vec![second.id]
    );
    Ok(())
}

/// Submitted instance checks: who may submit, whom `triggered_by` names, visible and existing
/// members, and snapshot groups that match and stay contained
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
    let (_, narrow) = agentic_tool(&[&a], &admin).await?;
    let (_, secret) = agentic_tool(&[&a, &b, &hidden], &admin).await?;
    let created = toolset(&admin, "shared", &[&a, &b], ToolSetEntityRequest::default()).await?;
    // a user who can't edit the toolset in every group is refused
    let mut first = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    first.triggered_by = ganymede.username.clone();
    let (status, _) = refusal(submit(&ganymede.client, &first).await);
    assert_eq!(status, 401);
    // a user naming someone else in triggered_by is refused
    first.triggered_by = ganymede.username.clone();
    let (status, msg) = refusal(submit(&europa.client, &first).await);
    assert_eq!(status, 400);
    assert!(msg.contains("triggered_by"), "{msg}");
    // a member that doesn't exist is a 404
    let mut missing = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    missing.members[0].tool_instance = Uuid::new_v4();
    missing.triggered_by = europa.username.clone();
    let (status, _) = refusal(submit(&europa.client, &missing).await);
    assert_eq!(status, 404);
    // recorded groups that differ from the stored ones are refused
    let mut wrong_set_groups = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    wrong_set_groups.tool_set_groups = vec![a.clone()];
    wrong_set_groups.triggered_by = europa.username.clone();
    let (status, msg) = refusal(submit(&europa.client, &wrong_set_groups).await);
    assert_eq!(status, 400);
    assert!(msg.contains("shared"), "{msg}");
    let mut wrong_member_groups = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    wrong_member_groups.members[0].groups = vec![a.clone()];
    wrong_member_groups.triggered_by = europa.username.clone();
    let (status, _) = refusal(submit(&europa.client, &wrong_member_groups).await);
    assert_eq!(status, 400);
    // a member that doesn't cover the toolset's groups is refused
    let mut uncontained = generators::gen_toolset_instance(&created, 1, None, &[&narrow]);
    uncontained.triggered_by = europa.username.clone();
    let (status, msg) = refusal(submit(&europa.client, &uncontained).await);
    assert_eq!(status, 400);
    assert!(msg.contains(&b), "{msg}");
    // a member stored in a group the user can't see is recorded without naming that group
    let mut hidden_member = generators::gen_toolset_instance(&created, 1, None, &[&secret]);
    hidden_member.members[0].groups = vec![a.clone(), b.clone()];
    hidden_member.triggered_by = europa.username.clone();
    let (status, msg) = refusal(submit(&europa.client, &hidden_member).await);
    assert_eq!(status, 400);
    assert!(!msg.contains(&hidden), "{msg}");
    // nothing was written by any refusal
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).latest_instance,
        None::<Uuid>
    );
    // a user naming themselves is created
    let mut own = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    own.triggered_by = europa.username.clone();
    let first_id = submit(&europa.client, &own).await?.id;
    let stored = admin.entities.get(first_id).await?;
    assert_eq!(
        instance_meta(&stored).triggered_by.clone(),
        europa.username.clone()
    );
    assert_eq!(stored.submitter, europa.username.clone());
    // an unset triggered_by names the submitter
    let unset = generators::gen_toolset_instance(&created, 2, Some(first_id), &[&tool]);
    let second_id = submit(&europa.client, &unset).await?.id;
    let stored = admin.entities.get(second_id).await?;
    assert_eq!(
        instance_meta(&stored).triggered_by.clone(),
        europa.username.clone()
    );
    // an admin may name anyone
    let mut by_admin = generators::gen_toolset_instance(&created, 3, Some(second_id), &[&tool]);
    by_admin.triggered_by = ganymede.username.clone();
    let third_id = submit(&admin, &by_admin).await?.id;
    let stored = admin.entities.get(third_id).await?;
    assert_eq!(
        instance_meta(&stored).triggered_by.clone(),
        ganymede.username.clone()
    );
    assert_eq!(stored.submitter, "thorium".to_owned());
    // a member the submitter can't see in any group answers like a missing one, without naming
    // its groups
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let c = god_group(
        &admin,
        "juno",
        &suffix,
        &GroupRoles::default().user(&callisto.username),
    )
    .await?;
    let theirs = toolset(&admin, "theirs", &[&c], ToolSetEntityRequest::default()).await?;
    let (_, only_hidden) = agentic_tool(&[&hidden], &admin).await?;
    let mut hidden_only = generators::gen_toolset_instance(&theirs, 1, None, &[&only_hidden]);
    hidden_only.triggered_by = callisto.username.clone();
    let (status, msg) = refusal(submit(&callisto.client, &hidden_only).await);
    assert_eq!(status, 404);
    assert!(!msg.contains(&hidden), "{msg}");
    Ok(())
}

/// Revisions run r1, r2, r3 with each `previous` naming the one before, and `latest_instance`
/// is the newest ready instance
#[tokio::test]
async fn revisions_and_latest() -> Result<(), Error> {
    // a toolset with one member
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
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "versions", &[&a], ToolSetEntityRequest::default()).await?;
    // a toolset with no ready instance has no latest instance in its JSON
    let (status, raw) = raw_get(&io, created.id).await?;
    assert_eq!(status, 200);
    assert!(
        raw["metadata"]["ToolSet"].get("latest_instance").is_none(),
        "{raw}"
    );
    // a first instance must be revision 1
    let early = generators::gen_toolset_instance(&created, 2, None, &[&tool]);
    let (status, _) = refusal(submit(&admin, &early).await);
    assert_eq!(status, 409);
    // submit r1, r2, and an invalid r3
    let r1 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    let r2 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]),
    )
    .await?;
    // a second r2 is a conflict, and an r3 that skips r2 as its previous is refused
    let again = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]);
    let (status, _) = refusal(submit(&admin, &again).await);
    assert_eq!(status, 409);
    let skipped = generators::gen_toolset_instance(&created, 3, Some(r1.id), &[&tool]);
    let (status, _) = refusal(submit(&admin, &skipped).await);
    assert_eq!(status, 400);
    let mut invalid = generators::gen_toolset_instance(&created, 3, Some(r2.id), &[&tool]);
    invalid.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["function_name collision".into()],
    };
    let r3 = submit(&admin, &invalid).await?;
    // the names and previous links follow the revisions
    assert_eq!(r1.name, "versions#r1".to_owned());
    assert_eq!(r2.name, "versions#r2".to_owned());
    assert_eq!(r3.name, "versions#r3".to_owned());
    let stored = [r1.id, r2.id, r3.id];
    let mut previous = None;
    for id in stored {
        let entity = admin.entities.get(id).await?;
        assert_eq!(instance_meta(&entity).previous, previous);
        previous = Some(id);
    }
    // the newest ready instance is r2
    assert_eq!(
        meta(&io.client.entities.get(created.id).await?).latest_instance,
        Some(r2.id)
    );
    // after r2 is deleted the latest is r1
    admin.entities.delete(r2.id).await?;
    assert_eq!(
        meta(&io.client.entities.get(created.id).await?).latest_instance,
        Some(r1.id)
    );
    Ok(())
}

/// Two submissions of the same revision at once create exactly one instance
#[tokio::test]
async fn concurrent_submissions() -> Result<(), Error> {
    // a toolset with one member
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "racing", &[&a], ToolSetEntityRequest::default()).await?;
    // send two r1 submissions at once
    let instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    let (left, right) = tokio::join!(submit(&admin, &instance), submit(&admin, &instance));
    // exactly one is created and the other is a conflict
    let results = [left, right];
    let created_count = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(created_count, 1);
    for result in results {
        if let Err(err) = result {
            assert_eq!(err.status().map(|status| status.as_u16()), Some(409));
        }
    }
    // the next revision is r2
    let winner = meta(&admin.entities.get(created.id).await?)
        .latest_instance
        .unwrap();
    let next = generators::gen_toolset_instance(&created, 2, Some(winner), &[&tool]);
    assert_eq!(submit(&admin, &next).await?.name, "racing#r2".to_owned());
    Ok(())
}

/// The revision lock is released after every submission, a crashed holder blocks only its own
/// toolset until it expires, and twenty racing submissions create one instance
#[tokio::test]
async fn revision_lock() -> Result<(), Error> {
    // two toolsets with one member each
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let locked = toolset(&admin, "locked", &[&a], ToolSetEntityRequest::default()).await?;
    let free = toolset(&admin, "free", &[&a], ToolSetEntityRequest::default()).await?;
    // the lock is released after a created and a refused submission
    let r1 = submit(
        &admin,
        &generators::gen_toolset_instance(&locked, 1, None, &[&tool]),
    )
    .await?;
    assert!(!test_utilities::instance_lock_exists(locked.id).await?);
    let mut wrong = generators::gen_toolset_instance(&locked, 2, Some(r1.id), &[&tool]);
    wrong.digest = format!("v1:{}", "1".repeat(64));
    let (status, _) = refusal(submit(&admin, &wrong).await);
    assert_eq!(status, 400);
    assert!(!test_utilities::instance_lock_exists(locked.id).await?);
    // a crashed holder's lock refuses r2 after about two seconds
    test_utilities::hold_instance_lock_for(locked.id, "crashed-holder", 3_000).await?;
    let r2 = generators::gen_toolset_instance(&locked, 2, Some(r1.id), &[&tool]);
    let start = Instant::now();
    let (status, msg) = refusal(submit(&admin, &r2).await);
    let waited = start.elapsed();
    assert_eq!(status, 409);
    assert_eq!(
        msg,
        "another instance of `locked` is being created; retry".to_owned()
    );
    assert!(waited >= Duration::from_millis(1_800), "{waited:?}");
    assert!(waited < Duration::from_millis(3_000), "{waited:?}");
    assert_eq!(
        meta(&admin.entities.get(locked.id).await?).latest_instance,
        Some(r1.id)
    );
    // once it expires the same submission is created
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let r2_id = submit(&admin, &r2).await?.id;
    // while one toolset's lock is held another toolset's submission doesn't wait
    test_utilities::hold_instance_lock(locked.id).await?;
    let start = Instant::now();
    submit(
        &admin,
        &generators::gen_toolset_instance(&free, 1, None, &[&tool]),
    )
    .await?;
    assert!(
        start.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        start.elapsed()
    );
    test_utilities::release_instance_lock(locked.id).await?;
    // twenty racing submissions of r3 create exactly one instance
    let r3 = generators::gen_toolset_instance(&locked, 3, Some(r2_id), &[&tool]);
    let racing = (0..20).map(|_| submit(&admin, &r3));
    let results = futures::future::join_all(racing).await;
    let created = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(created, 1);
    for result in &results {
        if let Err(err) = result {
            assert_eq!(err.status().map(|status| status.as_u16()), Some(409));
        }
    }
    // revisions stay contiguous, so r4 follows
    let winner = meta(&admin.entities.get(locked.id).await?)
        .latest_instance
        .unwrap();
    assert_eq!(
        admin.entities.get(winner).await?.name,
        "locked#r3".to_owned()
    );
    let r4 = generators::gen_toolset_instance(&locked, 4, Some(winner), &[&tool]);
    assert_eq!(submit(&admin, &r4).await?.name, "locked#r4".to_owned());
    assert!(!test_utilities::instance_lock_exists(locked.id).await?);
    Ok(())
}

/// A user outside a toolset's groups can't read, list, or find it or its instances by tag
#[tokio::test]
async fn visibility() -> Result<(), Error> {
    // a toolset and an instance in minerva, a member and an outsider
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
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "private", &[&a], ToolSetEntityRequest::default()).await?;
    let instance = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    // the outsider gets a 404 for both
    for id in [created.id, instance.id] {
        let (status, _) = refusal(amalthea.client.entities.get(id).await);
        assert_eq!(status, 404);
    }
    // neither is in the outsider's listings or tag listings
    assert_eq!(
        listed(&amalthea.client, std::slice::from_ref(&elsewhere)).await?,
        Vec::<Uuid>::new()
    );
    let tagged = EntityListOpts::default()
        .tag("ToolSetId", created.id.to_string())
        .limit(100);
    assert_eq!(amalthea.client.entities.list(&tagged).await?.data.len(), 0);
    // the member sees both, lists both, and finds the instance by tag
    io.client.entities.get(created.id).await?;
    io.client.entities.get(instance.id).await?;
    let mut found = listed(&io.client, std::slice::from_ref(&a)).await?;
    found.sort();
    assert_eq!(found, sorted(&[created.id, instance.id]));
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

/// The selector's key and value are sent together and neither may be empty
#[tokio::test]
async fn selector_fields() -> Result<(), Error> {
    // a user who can create toolsets
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let thebe = moon_user(&admin, "thebe", &suffix).await?;
    let a = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&thebe.username),
    )
    .await?;
    let base = |name: &str| {
        vec![
            ("kind", "ToolSet".to_owned()),
            ("name", name.to_owned()),
            ("groups[]", a.clone()),
        ]
    };
    // a key alone or a value alone is refused, naming the missing field
    let mut key_only = base("key-only");
    key_only.push(("metadata[toolset_selector_key]", "toolset".to_owned()));
    let (status, body) = raw_form(&thebe, None, &key_only).await?;
    assert_eq!(status, 400);
    assert!(body.contains("toolset_selector_value"), "{body}");
    let mut value_only = base("value-only");
    value_only.push(("metadata[toolset_selector_value]", "ghidra".to_owned()));
    let (status, body) = raw_form(&thebe, None, &value_only).await?;
    assert_eq!(status, 400);
    assert!(body.contains("toolset_selector_key"), "{body}");
    // an empty part is refused, naming it
    let mut empty = base("empty");
    empty.push(("metadata[toolset_selector_key]", String::new()));
    empty.push(("metadata[toolset_selector_value]", "ghidra".to_owned()));
    let (status, body) = raw_form(&thebe, None, &empty).await?;
    assert_eq!(status, 400);
    assert!(body.contains("toolset_selector_key"), "{body}");
    // nothing was written
    assert_eq!(
        listed(&admin, std::slice::from_ref(&a)).await?,
        Vec::<Uuid>::new()
    );
    // both parts together set the selector
    let mut both = base("both");
    both.push(("metadata[toolset_selector_key]", "toolset".to_owned()));
    both.push(("metadata[toolset_selector_value]", "ghidra".to_owned()));
    let (status, body) = raw_form(&thebe, None, &both).await?;
    assert_eq!(status, 200);
    let id: Uuid = serde_json::from_str::<serde_json::Value>(&body)?["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stored = thebe.client.entities.get(id).await?;
    assert_eq!(
        meta(&stored).selector.clone(),
        Some(ToolSetSelector::new("toolset", "ghidra"))
    );
    // an update with only one part is refused and changes nothing
    let fields = [("metadata[toolset_selector_key]", "suite".to_owned())];
    let (status, body) = raw_form(&thebe, Some(id), &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("toolset_selector_value"), "{body}");
    let fields = [
        ("metadata[toolset_selector_key]", "suite".to_owned()),
        ("metadata[toolset_selector_value]", String::new()),
    ];
    let (status, body) = raw_form(&thebe, Some(id), &fields).await?;
    assert_eq!(status, 400);
    assert!(body.contains("toolset_selector_value"), "{body}");
    let stored = thebe.client.entities.get(id).await?;
    assert_eq!(
        meta(&stored).selector.clone(),
        Some(ToolSetSelector::new("toolset", "ghidra"))
    );
    Ok(())
}

/// A toolset's selector matches a tool tagged in every one of its groups and leaves out one
/// tagged in only some of them, naming the groups it lacks
#[tokio::test]
async fn selector_matches_every_group() -> Result<(), Error> {
    // a toolset in minerva and mars with a selector
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let tagger = generators::named_user_with_role(
        &admin,
        &generators::readable_name("thebe", &suffix),
        UserRole::Admin,
    )
    .await?;
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let selector = ToolSetSelector::new("toolset", format!("ghidra-{suffix}"));
    let created = toolset(
        &admin,
        "ghidra",
        &[&a, &b],
        ToolSetEntityRequest::default().selector(selector.clone()),
    )
    .await?;
    // one tool tagged in both groups, one only in minerva, and one only on its instance
    let (both, _) = agentic_tool(&[&a, &b], &admin).await?;
    let (only_a, _) = agentic_tool(&[&a, &b], &admin).await?;
    let (instance_tagged, instance) = agentic_tool(&[&a, &b], &admin).await?;
    let tag = |groups: Vec<String>| json!({"groups": groups, "tags": {selector.key.clone(): [selector.value.clone()]}});
    assert_eq!(
        raw_tags(&tagger, both.id, false, tag(vec![a.clone(), b.clone()])).await?,
        204
    );
    assert_eq!(
        raw_tags(&tagger, only_a.id, false, tag(vec![a.clone()])).await?,
        204
    );
    assert_eq!(
        raw_tags(&tagger, instance.id, false, tag(vec![a.clone(), b.clone()])).await?,
        204
    );
    // list the tools carrying the tag in each of the toolset's groups
    let mut carriers: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for group in &created.groups {
        let opts = EntityListOpts::default()
            .groups(vec![group.clone()])
            .kind(EntityKinds::Tool)
            .tag(selector.key.clone(), selector.value.clone())
            .limit(100);
        for line in admin.entities.list(&opts).await?.data {
            carriers.entry(line.id).or_default().insert(group.clone());
        }
    }
    // only the tool tagged in both groups matches, and the instance's tag isn't a tool's
    let SelectorMatches { matched, partial } =
        ToolSetSelector::partition(&created.groups, &carriers);
    assert_eq!(matched, vec![both.id]);
    assert_eq!(partial.clone(), vec![(only_a.id, vec![b.clone()])]);
    assert!(!carriers.contains_key(&instance_tagged.id));
    // the left out tool is described as not tagged in mars
    let exclusion = ToolSetExclusion {
        tool: only_a.id,
        tool_instance: None,
        name: only_a.name.clone(),
        inclusion: ToolSetInclusion::Selector,
        reason: ToolSetExclusionReason::NotTaggedInAllGroups,
        missing_groups: partial[0].1.clone(),
    };
    assert_eq!(exclusion.describe(), format!("not tagged in `{b}`"));
    Ok(())
}

/// A deprecated tool instance can't be newly pinned by an agent or a toolset until it's
/// un-deprecated, which doesn't move its tool's latest
#[tokio::test]
async fn deprecated_pins() -> Result<(), Error> {
    // a tool with two versions whose first is deprecated
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    let second_spec = generators::gen_agentic_spec();
    admin
        .entities
        .update(tool.id, generators::gen_tool_publish("1.1.0", &second_spec))
        .await?;
    deprecate(&admin, first.id, true).await?;
    let latest_before = match admin.entities.get(tool.id).await?.metadata {
        EntityMetadata::Tool(tool) => tool.latest,
        other => panic!("expected a tool, got {other:?}"),
    };
    // neither an agent nor a toolset can pin it
    let mut agent = AgentEntityRequest::new(generators::gen_agent_config("pinner"));
    agent.tools = vec![first.id];
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_agent("pinner", &[&a], agent.clone()))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("deprecated"), "{msg}");
    let req = ToolSetEntityRequest::default().tool(first.id);
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("pinned", &[&a], req.clone()))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains("deprecated"), "{msg}");
    // once it's un-deprecated both pins succeed and latest stays put
    deprecate(&admin, first.id, false).await?;
    admin
        .entities
        .create(generators::gen_agent("pinner", &[&a], agent))
        .await?;
    admin
        .entities
        .create(generators::gen_toolset("pinned", &[&a], req))
        .await?;
    let latest_after = match admin.entities.get(tool.id).await?.metadata {
        EntityMetadata::Tool(tool) => tool.latest,
        other => panic!("expected a tool, got {other:?}"),
    };
    assert_eq!(latest_after, latest_before);
    Ok(())
}

/// A toolset's instances follow its group changes, an instance's groups can't change, and an
/// agent that calls the toolset blocks removing its group
#[tokio::test]
async fn group_changes() -> Result<(), Error> {
    // a toolset in minerva with one instance
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let roles = GroupRoles::default();
    let a = god_group(&admin, "minerva", &suffix, &roles).await?;
    let b = god_group(&admin, "mars", &suffix, &roles).await?;
    let (tool_root, tool) = agentic_tool(&[&a], &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool_root.id);
    let created = toolset(&admin, "growing", &[&a], req).await?;
    let instance = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    // adding mars is refused while an included tool isn't in it
    let (status, msg) = refusal(
        admin
            .entities
            .update(created.id, EntityUpdate::default().group(&b))
            .await,
    );
    assert_eq!(status, 400);
    assert!(msg.contains(&b), "{msg}");
    // once the tool is in mars the toolset and its instance gain it together, queueing one
    // change of the toolset in both groups
    admin
        .entities
        .update(tool_root.id, EntityUpdate::default().group(&b))
        .await?;
    let seen = changes(&admin, created.id).await?.len();
    admin
        .entities
        .update(created.id, EntityUpdate::default().group(&b))
        .await?;
    let admin_name = admin.users.info().await?.username;
    let changed = WorkflowGraphChange::ToolSetChanged {
        tool_set: created.id,
    };
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![queued(&admin_name, &[&a, &b], changed.clone())]
    );
    let widened = admin.entities.get(instance.id).await?;
    assert_eq!(
        sorted_groups(&widened.groups),
        sorted_groups(&[a.clone(), b.clone()])
    );
    // the instance's own snapshot isn't rewritten
    assert_eq!(
        instance_meta(&widened).tool_set_groups.clone(),
        vec![a.clone()]
    );
    // an agent in mars that calls the toolset blocks removing mars
    let agent = admin
        .entities
        .create(generators::gen_agent(
            "mars-caller",
            &[&b],
            agent_req("mars_caller", &[created.id]),
        ))
        .await?;
    let (status, msg) = refusal(
        admin
            .entities
            .update(created.id, EntityUpdate::default().remove_group(&b))
            .await,
    );
    assert_eq!(status, 409);
    assert!(msg.contains("mars-caller"), "{msg}");
    // once the agent is gone mars is removed from the toolset and its instance
    admin.entities.delete(agent.id).await?;
    let seen = changes(&admin, created.id).await?.len();
    admin
        .entities
        .update(created.id, EntityUpdate::default().remove_group(&b))
        .await?;
    assert_eq!(
        changes_since(&admin, created.id, seen).await?,
        vec![queued(&admin_name, &[&a], changed)]
    );
    let narrowed = admin.entities.get(instance.id).await?;
    assert_eq!(narrowed.groups.clone(), vec![a.clone()]);
    Ok(())
}

/// Build an association request between two entities
///
/// # Arguments
///
/// * `kind` - The kind of association
/// * `source` - The association's source
/// * `target` - The association's target
fn link(kind: AssociationKind, source: &Entity, target: &Entity) -> AssociationRequest {
    // link the two entities by id and name
    AssociationRequest::new(
        kind,
        AssociationTarget::Entity {
            id: source.id,
            name: source.name.clone(),
        },
    )
    .target(AssociationTarget::Entity {
        id: target.id,
        name: target.name.clone(),
    })
}

/// A `BuiltFrom` edge can't be written through the generic route, so no instance can be
/// attached to another toolset
#[tokio::test]
async fn forged_built_from() -> Result<(), Error> {
    // io edits minerva only; io's toolset has two instances and a victim toolset has one
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
    let b = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let (_, tool_a) = agentic_tool(&[&a], &admin).await?;
    let (_, tool_ab) = agentic_tool(&[&a, &b], &admin).await?;
    let mine = toolset(&io.client, "mine", &[&a], ToolSetEntityRequest::default()).await?;
    let m1 = submit(
        &io.client,
        &generators::gen_toolset_instance(&mine, 1, None, &[&tool_a]),
    )
    .await?;
    let m2 = submit(
        &io.client,
        &generators::gen_toolset_instance(&mine, 2, Some(m1.id), &[&tool_a]),
    )
    .await?;
    let victim = toolset(&admin, "victim", &[&a, &b], ToolSetEntityRequest::default()).await?;
    let v1 = submit(
        &admin,
        &generators::gen_toolset_instance(&victim, 1, None, &[&tool_ab]),
    )
    .await?;
    // io's instance can't be linked to the victim through the generic route
    let forged = admin.entities.get(m2.id).await?;
    let (status, msg) = refusal(
        io.client
            .associations
            .create(&link(AssociationKind::BuiltFrom, &forged, &victim))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("BuiltFrom"), "{msg}");
    // the victim's latest instance and next revision are unchanged
    assert_eq!(
        meta(&admin.entities.get(victim.id).await?).latest_instance,
        Some(v1.id)
    );
    let v2 = submit(
        &admin,
        &generators::gen_toolset_instance(&victim, 2, Some(v1.id), &[&tool_ab]),
    )
    .await?;
    assert_eq!(v2.name, "victim#r2".to_owned());
    Ok(())
}

/// A submission checks the toolset's groups as they are once it holds the toolset's lock, so a
/// group change that finishes first is never missed
#[tokio::test]
async fn submission_sees_group_changes() -> Result<(), Error> {
    // a tool in minerva and mars
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let b = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a, &b], &admin).await?;
    // race a submission against adding mars to the toolset, both waiting on its lock
    for attempt in 0..6 {
        let created = toolset(
            &admin,
            &format!("moving-{attempt}"),
            &[&a],
            ToolSetEntityRequest::default(),
        )
        .await?;
        let instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
        test_utilities::hold_instance_lock(created.id).await?;
        let change = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            admin
                .entities
                .update(created.id, EntityUpdate::default().group(&b))
                .await
        };
        let (submitted, changed, released) = tokio::join!(
            submit(&admin, &instance),
            change,
            release_later(created.id, Duration::from_millis(800)),
        );
        released?;
        changed?;
        // the instance is refused for its stale snapshot or follows the toolset into mars
        match submitted {
            Ok(resp) => {
                let stored = admin.entities.get(resp.id).await?;
                assert_eq!(
                    sorted_groups(&stored.groups),
                    sorted_groups(&[a.clone(), b.clone()]),
                    "attempt {attempt}"
                );
            }
            Err(err) => {
                assert_eq!(err.status().map(|status| status.as_u16()), Some(400));
            }
        }
    }
    Ok(())
}

/// Every member field taken from the stored tool instance must match it
#[tokio::test]
async fn member_fields_match() -> Result<(), Error> {
    // a toolset in minerva and two tools
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let (other, _) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "honest", &[&a], ToolSetEntityRequest::default()).await?;
    // a wrong function name, version, or tool is refused
    let mut function_name = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    function_name.members[0].function_name = "delete_everything".to_owned();
    let mut version = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    version.members[0].version = "99.0.0".to_owned();
    let mut wrong_tool = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    wrong_tool.members[0].tool = other.id;
    for (mut instance, expected) in [
        (function_name, "function_name"),
        (version, "version"),
        (wrong_tool, "belongs to tool"),
    ] {
        instance.digest = instance.compute_digest()?;
        let (status, msg) = refusal(submit(&admin, &instance).await);
        assert_eq!(status, 400, "{expected}: {msg}");
        assert!(msg.contains(expected), "{expected}: {msg}");
    }
    // nothing was created
    assert_eq!(
        meta(&admin.entities.get(created.id).await?).latest_instance,
        None
    );
    Ok(())
}

/// A deleted instance's revision is never given to a later instance
#[tokio::test]
async fn revisions_never_reused() -> Result<(), Error> {
    // a toolset with r1 and r2
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "onward", &[&a], ToolSetEntityRequest::default()).await?;
    let r1 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    let r2 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]),
    )
    .await?;
    // after r2 is deleted, revision 2 is a conflict and revision 3 follows r1
    admin.entities.delete(r2.id).await?;
    let again = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]);
    let (status, msg) = refusal(submit(&admin, &again).await);
    assert_eq!(status, 409, "{msg}");
    assert!(msg.contains("revision 3"), "{msg}");
    let r3 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 3, Some(r1.id), &[&tool]),
    )
    .await?;
    assert_eq!(r3.name, "onward#r3".to_owned());
    Ok(())
}

/// A deleted instance's revision stays taken when Redis loses its state, since the highest
/// revision is stored with the toolset
#[tokio::test]
async fn revisions_survive_redis_loss() -> Result<(), Error> {
    // a toolset with r1 and r2
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(&admin, "durable", &[&a], ToolSetEntityRequest::default()).await?;
    let r1 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    let r2 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]),
    )
    .await?;
    // delete r2, then drop the entity state Redis keeps for the toolset
    admin.entities.delete(r2.id).await?;
    test_utilities::drop_entity_redis_keys(created.id).await?;
    // revision 2 is still a conflict and revision 3 follows r1
    let again = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&tool]);
    let (status, msg) = refusal(submit(&admin, &again).await);
    assert_eq!(status, 409, "{msg}");
    assert!(msg.contains("revision 3"), "{msg}");
    let r3 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 3, Some(r1.id), &[&tool]),
    )
    .await?;
    assert_eq!(r3.name, "durable#r3".to_owned());
    // the toolset's row in its one group records revision 3 as its highest
    let rows = test_utilities::entity_rows(created.id).await?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    for (_, data) in rows {
        let value: serde_json::Value = serde_json::from_str(&data)?;
        assert_eq!(value["ToolSet"]["highest_revision"], 3, "{value}");
    }
    Ok(())
}

/// A selector key can't hold `=`, which would make its tag ambiguous
#[tokio::test]
async fn selector_key_without_equals() -> Result<(), Error> {
    // a group for the toolsets
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    // a key holding `=` is refused on create and on update
    let req = ToolSetEntityRequest::default().selector(ToolSetSelector::new("a=b", "c"));
    let (status, msg) = refusal(
        admin
            .entities
            .create(generators::gen_toolset("ambiguous", &[&a], req))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("toolset_selector_key"), "{msg}");
    let created = toolset(&admin, "plain", &[&a], ToolSetEntityRequest::default()).await?;
    let EntityMetadataUpdate::ToolSet {
        read_only,
        clear_selector,
        auto_instance,
        add_tools,
        remove_tools,
        extend_target_groups,
        ..
    } = toolset_update()
    else {
        unreachable!()
    };
    let ambiguous = EntityMetadataUpdate::ToolSet {
        read_only,
        selector: Some(ToolSetSelector::new("a=b", "c")),
        clear_selector,
        auto_instance,
        add_tools,
        remove_tools,
        extend_target_groups,
    };
    let (status, _) = refusal(update_metadata(&admin, created.id, ambiguous).await);
    assert_eq!(status, 400);
    // a value holding `=` is fine
    let req = ToolSetEntityRequest::default().selector(ToolSetSelector::new("a", "b=c"));
    toolset(&admin, "valued", &[&a], req).await?;
    Ok(())
}

/// An image upload is refused on a toolset instance
#[tokio::test]
async fn instance_image_refused() -> Result<(), Error> {
    // a toolset with one instance
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
    let (_, tool) = agentic_tool(&[&a], &admin).await?;
    let created = toolset(
        &io.client,
        "pictured",
        &[&a],
        ToolSetEntityRequest::default(),
    )
    .await?;
    let r1 = submit(
        &io.client,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    // a patch uploading an image is refused
    let png = reqwest::multipart::Part::bytes(b"\x89PNG\r\n\x1a\n".to_vec())
        .file_name("icon.png")
        .mime_str("image/png")?;
    let resp = reqwest::Client::new()
        .patch(format!("{}/api/entities/{}", io.client.host, r1.id))
        .header("authorization", io.auth_header())
        .multipart(Form::new().part("image", png))
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("name or description"), "{body}");
    // the instance still has no image
    assert_eq!(admin.entities.get(r1.id).await?.image, None);
    Ok(())
}

/// The generic route adds `Includes` edges with the same rules, under the toolset's lock
#[tokio::test]
async fn generic_includes_route() -> Result<(), Error> {
    // a toolset, a tool, and an agent in minerva
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
    let (tool, _) = agentic_tool(&[&a], &io.client).await?;
    let created = toolset(
        &io.client,
        "generic",
        &[&a],
        ToolSetEntityRequest::default(),
    )
    .await?;
    let agent = io
        .client
        .entities
        .create(generators::gen_agent(
            "included",
            &[&a],
            agent_req("included", &[]),
        ))
        .await?;
    let agent = io.client.entities.get(agent.id).await?;
    // including an agent is refused
    let (status, _) = refusal(
        io.client
            .associations
            .create(&link(AssociationKind::Includes, &created, &agent))
            .await,
    );
    assert_eq!(status, 400);
    // including the tool waits for the toolset's lock and then shows on get
    let include = link(AssociationKind::Includes, &created, &tool);
    test_utilities::hold_instance_lock(created.id).await?;
    let (included, released) = tokio::join!(
        timed(io.client.associations.create(&include)),
        release_later(created.id, Duration::from_millis(1_500)),
    );
    released?;
    let waited = included?;
    assert!(waited >= Duration::from_millis(1_400), "{waited:?}");
    assert_eq!(
        ids(&meta(&io.client.entities.get(created.id).await?).tools),
        vec![tool.id]
    );
    Ok(())
}

/// An agent's tool removal field doesn't remove a toolset edge
#[tokio::test]
async fn toolset_removal_field() -> Result<(), Error> {
    // an agent calling a toolset in minerva
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
    let created = toolset(&io.client, "called", &[&a], ToolSetEntityRequest::default()).await?;
    let agent = io
        .client
        .entities
        .create(generators::gen_agent(
            "caller",
            &[&a],
            agent_req("caller", &[created.id]),
        ))
        .await?;
    // removing the toolset through the tool removal field is refused and keeps the edge
    let wrong_field = EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: Vec::new(),
        remove_tools: vec![created.id],
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    };
    let (status, msg) = refusal(update_metadata(&io.client, agent.id, wrong_field).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("remove_agent_tool_sets"), "{msg}");
    let stored = io.client.entities.get(agent.id).await?;
    assert_eq!(ids(&agent_meta(&stored).tool_sets), vec![created.id]);
    Ok(())
}

/// Creating a toolset that includes a tool whose lock another request holds waits for the
/// lock, even when the tool sorts before the new toolset
#[tokio::test]
async fn toolset_create_waits_for_target_lock() -> Result<(), Error> {
    // a tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    // the new toolset sorts after the tool
    let name = format!("kit-{suffix}");
    let (_, high) = ordered_ids();
    test_utilities::faults::create_with_id(&name, high);
    // create the toolset while another request holds the tool's lock for 3 seconds
    test_utilities::hold_instance_lock(tool.id).await?;
    let (req, groups) = (
        ToolSetEntityRequest::default().tool(tool.id),
        [minerva.as_str()],
    );
    let (created, released) = tokio::join!(
        timed(toolset(&io.client, &name, &groups, req)),
        release_later(tool.id, Duration::from_secs(3)),
    );
    released?;
    // the create waited for the lock and then succeeded with the tool included
    let waited = created?;
    assert!(waited >= Duration::from_millis(2_900), "{waited:?}");
    let read = io.client.entities.get(high).await?;
    assert_eq!(ids(&meta(&read).tools), vec![tool.id]);
    assert!(!test_utilities::instance_lock_exists(tool.id).await?);
    Ok(())
}

/// Widening an included tool needs the rights a group add on it needs
#[tokio::test]
async fn includes_widening_needs_edit_rights() -> Result<(), Error> {
    // europa owns minerva and submits the tool, callisto monitors minerva and owns vesta, and io
    // edits entities in minerva and vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let io = moon_user(&admin, "io", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .owner(&europa.username)
            .monitor(&callisto.username)
            .user(&io.username),
    )
    .await?;
    let vesta = god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default()
            .owner(&callisto.username)
            .user(&io.username),
    )
    .await?;
    // europa's tool in minerva and callisto's toolset in vesta
    let (tool, _) = agentic_tool(&[&minerva], &europa.client).await?;
    let hoard = toolset(
        &callisto.client,
        "hoard",
        &[&vesta],
        ToolSetEntityRequest::default(),
    )
    .await?;
    let EntityMetadataUpdate::ToolSet {
        read_only,
        selector,
        clear_selector,
        auto_instance,
        remove_tools,
        ..
    } = toolset_update()
    else {
        unreachable!()
    };
    let include = EntityMetadataUpdate::ToolSet {
        read_only,
        selector,
        clear_selector,
        auto_instance,
        add_tools: vec![tool.id],
        remove_tools,
        extend_target_groups: true,
    };
    // callisto can't widen the tool into vesta through the toolset's includes
    let (status, msg) = refusal(update_metadata(&callisto.client, hoard.id, include.clone()).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains(&vesta), "{msg}");
    assert!(msg.contains("can't widen"), "{msg}");
    let read = admin.entities.get(tool.id).await?;
    assert_eq!(read.groups, vec![minerva.clone()]);
    assert!(
        meta(&callisto.client.entities.get(hoard.id).await?)
            .tools
            .is_empty()
    );
    // io edits entities in both groups, so the same include widens the tool into vesta
    update_metadata(&io.client, hoard.id, include).await?;
    let read = admin.entities.get(tool.id).await?;
    assert_eq!(
        sorted_groups(&read.groups),
        sorted_groups(&[minerva.clone(), vesta.clone()])
    );
    Ok(())
}

/// A toolset instance's snapshot groups only name the groups its reader can see, whether it's
/// read on its own or inside an agent that calls it
#[tokio::test]
// one scenario checks every read path against shared fixtures
#[allow(clippy::too_many_lines)]
async fn instance_snapshot_groups_filtered() -> Result<(), Error> {
    // ganymede is only in minerva, and the toolset and its member are in minerva and mars
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
    let created = toolset(
        &admin,
        "snap",
        &[&minerva, &mars],
        ToolSetEntityRequest::default(),
    )
    .await?;
    let r1 = submit(
        &admin,
        &generators::gen_toolset_instance(&created, 1, None, &[&tool]),
    )
    .await?;
    // ganymede's read names only minerva in the snapshot
    let read = ganymede.client.entities.get(r1.id).await?;
    assert_eq!(instance_meta(&read).tool_set_groups, vec![minerva.clone()]);
    assert_eq!(
        instance_meta(&read).members[0].groups,
        vec![minerva.clone()]
    );
    // so does his details listing
    let opts = EntityListOpts::default()
        .groups(vec![minerva.clone()])
        .kind(EntityKinds::ToolSetInstance)
        .limit(100);
    let listed = ganymede.client.entities.list_details(&opts).await?;
    let found = listed
        .data
        .iter()
        .find(|entity| entity.id == r1.id)
        .expect("the instance is listed");
    assert_eq!(instance_meta(found).tool_set_groups, vec![minerva.clone()]);
    assert_eq!(
        instance_meta(found).members[0].groups,
        vec![minerva.clone()]
    );
    // an agent in minerva calls the instance, so ganymede reads the instance inside it too
    let agent = admin
        .entities
        .create(generators::gen_agent(
            "snap-caller",
            &[&minerva],
            agent_req("snap_caller", &[r1.id]),
        ))
        .await?;
    // ganymede's read of the agent names only minerva in the nested snapshot
    let read = ganymede.client.entities.get(agent.id).await?;
    let nested = &agent_meta(&read).tool_sets;
    assert_eq!(ids(nested), vec![r1.id]);
    assert_eq!(
        instance_meta(&nested[0]).tool_set_groups,
        vec![minerva.clone()]
    );
    assert_eq!(
        instance_meta(&nested[0]).members[0].groups,
        vec![minerva.clone()]
    );
    // so does his details listing of agents
    let opts = EntityListOpts::default()
        .groups(vec![minerva.clone()])
        .kind(EntityKinds::Agent)
        .limit(100);
    let listed = ganymede.client.entities.list_details(&opts).await?;
    let found = listed
        .data
        .iter()
        .find(|entity| entity.id == agent.id)
        .expect("the agent is listed");
    let nested = &agent_meta(found).tool_sets;
    assert_eq!(ids(nested), vec![r1.id]);
    assert_eq!(
        instance_meta(&nested[0]).tool_set_groups,
        vec![minerva.clone()]
    );
    assert_eq!(
        instance_meta(&nested[0]).members[0].groups,
        vec![minerva.clone()]
    );
    // the admin sees every group, so the stored snapshot is whole
    let both = sorted_groups(&[minerva.clone(), mars.clone()]);
    let read = admin.entities.get(r1.id).await?;
    assert_eq!(
        sorted_groups(&instance_meta(&read).tool_set_groups),
        both.clone()
    );
    assert_eq!(sorted_groups(&instance_meta(&read).members[0].groups), both);
    // and so is the snapshot nested in the admin's read of the agent
    let read = admin.entities.get(agent.id).await?;
    let nested = &agent_meta(&read).tool_sets;
    assert_eq!(
        sorted_groups(&instance_meta(&nested[0]).tool_set_groups),
        both.clone()
    );
    assert_eq!(
        sorted_groups(&instance_meta(&nested[0]).members[0].groups),
        both
    );
    Ok(())
}

/// A submission is refused at once, without waiting on the toolset's lock, when the submitter
/// can't see or edit the toolset, even when it's sent in groups the submitter can edit
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
    let (_, tool) = agentic_tool(&[&minerva, &mars], &admin).await?;
    let created = toolset(
        &admin,
        "oracle",
        &[&minerva, &mars],
        ToolSetEntityRequest::default(),
    )
    .await?;
    // with another request holding the toolset's lock, ganymede's submission sent only in
    // minerva is a 401 at once
    let mut instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    instance.triggered_by = ganymede.username.clone();
    let mut req = generators::gen_toolset_instance_req(&instance);
    req.groups = vec![minerva.clone()];
    test_utilities::hold_instance_lock(created.id).await?;
    let ((elapsed, status), released) = tokio::join!(
        timed_refusal(ganymede.client.entities.create(req)),
        release_later(created.id, Duration::from_millis(3_000))
    );
    released?;
    assert_eq!(status, 401);
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    // amalthea can't see the toolset, so hers sent in vulcan is a 404 at once
    instance.triggered_by = amalthea.username.clone();
    let mut req = generators::gen_toolset_instance_req(&instance);
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
        None::<Uuid>
    );
    Ok(())
}

/// A member id the submitter can't see answers like a missing one, whatever it is
#[tokio::test]
async fn hidden_member_answers_like_missing() -> Result<(), Error> {
    // europa edits minerva, and an agent and a tool live in mars, which she isn't in
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&europa.username),
    )
    .await?;
    let mars = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let (_, tool) = agentic_tool(&[&minerva], &admin).await?;
    let (_, hidden_tool) = agentic_tool(&[&mars], &admin).await?;
    let hidden_agent = admin
        .entities
        .create(generators::gen_agent(
            "grabber",
            &[&mars],
            AgentEntityRequest::new(generators::gen_agent_config("grabber")),
        ))
        .await?
        .id;
    let created = toolset(
        &europa.client,
        "oracle",
        &[&minerva],
        ToolSetEntityRequest::default(),
    )
    .await?;
    // a missing id, a hidden agent, and a hidden tool instance are each the same 404
    for id in [Uuid::new_v4(), hidden_agent, hidden_tool.id] {
        let mut instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
        instance.members[0].tool_instance = id;
        let (status, msg) = refusal(submit(&europa.client, &instance).await);
        assert_eq!(status, 404, "{msg}");
        assert_eq!(
            msg,
            format!("member tool instance `{id}` doesn't exist"),
            "{msg}"
        );
    }
    // a visible entity that isn't a tool instance is still named as such
    let visible = toolset(
        &europa.client,
        "visible",
        &[&minerva],
        ToolSetEntityRequest::default(),
    )
    .await?;
    let mut instance = generators::gen_toolset_instance(&created, 1, None, &[&tool]);
    instance.members[0].tool_instance = visible.id;
    let (status, msg) = refusal(submit(&europa.client, &instance).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("is not a tool instance"), "{msg}");
    Ok(())
}

/// A submission or tag request can't set a toolset instance's intrinsic tag keys
#[tokio::test]
async fn intrinsic_tag_keys_refused() -> Result<(), Error> {
    // europa edits minerva, where a toolset and two tools live
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
    let created = toolset(&admin, "kit", &[&a], ToolSetEntityRequest::default()).await?;
    let other = toolset(&admin, "other", &[&a], ToolSetEntityRequest::default()).await?;
    let (_, member) = agentic_tool(&[&a], &admin).await?;
    let (unrelated, _) = agentic_tool(&[&a], &admin).await?;
    let instance = generators::gen_toolset_instance(&created, 1, None, &[&member]);
    // a submission naming any intrinsic key is refused
    for (key, value) in [
        ("ToolSetFollowsTool", unrelated.id.to_string()),
        ("ToolSetId", other.id.to_string()),
        ("ToolSetSelector", "toolset=ghidra".to_owned()),
        ("toolsetstatus", "Ready".to_owned()),
    ] {
        let req = generators::gen_toolset_instance_req(&instance).tag(key, value);
        let (status, msg) = refusal(europa.client.entities.create(req).await);
        assert_eq!(status, 400, "{key}: {msg}");
        assert!(msg.contains(key), "{msg}");
    }
    // the clean instance submits, and the tag route refuses the intrinsic keys too
    let r1 = europa
        .client
        .entities
        .create(generators::gen_toolset_instance_req(&instance))
        .await?;
    for (key, value) in [
        ("ToolSetFollowsTool", unrelated.id.to_string()),
        ("ToolSetId", other.id.to_string()),
    ] {
        let status = raw_tags(&europa, r1.id, false, json!({"tags": {key: [value]}})).await?;
        assert_eq!(status, 400, "{key}");
    }
    let stored = admin.entities.get(r1.id).await?;
    assert_eq!(
        tag_values(&stored, "ToolSetId"),
        vec![created.id.to_string()]
    );
    assert!(!tag_values(&stored, "ToolSetFollowsTool").contains(&unrelated.id.to_string()));
    Ok(())
}

/// A toolset gains a group only when every member its existing instances snapshotted is in it
#[tokio::test]
async fn widening_needs_contained_members() -> Result<(), Error> {
    // europa edits minerva and vesta, and callisto is only in vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&europa.username),
    )
    .await?;
    let vesta = god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .user(&callisto.username),
    )
    .await?;
    // a toolset in minerva that includes a minerva-only tool, with an instance of it
    let (secret_root, secret) = agentic_tool(&[&minerva], &admin).await?;
    let req = ToolSetEntityRequest::default().tool(secret_root.id);
    let created = toolset(&europa.client, "kit", &[&minerva], req).await?;
    let r1 = submit(
        &europa.client,
        &generators::gen_toolset_instance(&created, 1, None, &[&secret]),
    )
    .await?;
    // europa drops the include, then widening to vesta is refused naming the old instance
    let mut drop = toolset_update();
    if let EntityMetadataUpdate::ToolSet { remove_tools, .. } = &mut drop {
        remove_tools.push(secret_root.id);
    }
    update_metadata(&europa.client, created.id, drop).await?;
    let (status, msg) = refusal(
        europa
            .client
            .entities
            .update(created.id, EntityUpdate::default().group(&vesta))
            .await,
    );
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("kit#r1"), "{msg}");
    // callisto still can't read the member's snapshot
    assert_eq!(refusal(callisto.client.entities.get(r1.id).await).0, 404);
    assert_eq!(
        admin.entities.get(r1.id).await?.groups,
        vec![minerva.clone()]
    );
    Ok(())
}

/// A submitted toolset instance is tagged with every tool its toolset includes at latest, even
/// one that left no member, so the tool's next publish finds the toolset
#[tokio::test]
async fn explicit_latest_tools_followed() -> Result<(), Error> {
    // a read only toolset in minerva including a tool that declares an effect
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let mut spec = generators::gen_agentic_spec();
    if let thorium::models::ToolSpec::Agentic(tool) = &mut spec {
        tool.capabilities
            .effects
            .insert(thorium::models::ToolEffect::WritesThorium);
    }
    let (writer, _) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    let (member_root, member) = agentic_tool(&[&a], &admin).await?;
    let req = ToolSetEntityRequest::default()
        .read_only(true)
        .tool(writer.id)
        .tool(member_root.id);
    let created = toolset(&admin, "kit", &[&a], req).await?;
    // the instance the resolver would build has only the tool without effects as a member
    let instance = generators::gen_toolset_instance(&created, 1, None, &[&member]);
    let r1 = submit(&admin, &instance).await?;
    // both included tools are followed
    let stored = admin.entities.get(r1.id).await?;
    let mut expected = vec![writer.id.to_string(), member_root.id.to_string()];
    expected.sort();
    assert_eq!(tag_values(&stored, "ToolSetFollowsTool"), expected);
    Ok(())
}

/// A submitted toolset instance is bounded by what a resolution can list, and a ready one can't
/// hold what the resolver would make invalid: too many members, two members of one tool or
/// with one function name, non-agentic members, or members with effects in a read only toolset
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn submitted_instance_content_bounds() -> Result<(), Error> {
    // a toolset in minerva and one more member than a toolset may have
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let a = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let created = toolset(&admin, "kit", &[&a], ToolSetEntityRequest::default()).await?;
    let limits = thorium::models::ToolSetLimits::default();
    let mut members = Vec::new();
    for _ in 0..=limits.max_tools_per_toolset {
        members.push(agentic_tool(&[&a], &admin).await?.1);
    }
    let refs = members.iter().collect::<Vec<&Entity>>();
    // a ready instance with too many members is refused, and an invalid one is stored
    let ready = generators::gen_toolset_instance(&created, 1, None, &refs);
    let (status, msg) = refusal(submit(&admin, &ready).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("max_tools_per_toolset"), "{msg}");
    let mut invalid = ready.clone();
    invalid.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["too many members".to_owned()],
    };
    invalid.digest = invalid.compute_digest()?;
    let r1 = submit(&admin, &invalid).await?;
    // two members sharing a function name are refused in a ready instance
    let mut spec = generators::gen_agentic_spec();
    let shared_name = match &spec {
        thorium::models::ToolSpec::Agentic(tool) => tool.function_name.clone(),
        thorium::models::ToolSpec::Image(_) | thorium::models::ToolSpec::Harness(_) => {
            unreachable!()
        }
    };
    let (_, first) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    if let thorium::models::ToolSpec::Agentic(tool) = &mut spec {
        tool.model_description = "another tool with the same function name".to_owned();
    }
    let (_, second) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    let clash = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&first, &second]);
    let (status, msg) = refusal(submit(&admin, &clash).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains(&shared_name), "{msg}");
    // two instances of one tool are refused in a ready instance
    let (root, v1) = agentic_tool(&[&a], &admin).await?;
    admin
        .entities
        .update(
            root.id,
            generators::gen_tool_publish("1.1.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let latest = match admin.entities.get(root.id).await?.metadata {
        EntityMetadata::Tool(meta) => meta.latest.unwrap(),
        _ => unreachable!(),
    };
    let v2 = admin.entities.get(latest).await?;
    let twice = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&v1, &v2]);
    let (status, msg) = refusal(submit(&admin, &twice).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains(&root.id.to_string()), "{msg}");
    // a non-agentic member is refused whatever the status
    let (_, image) =
        generators::tool(&[&a], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let mut imaged = generators::gen_toolset_instance(&created, 2, Some(r1.id), &[&image]);
    imaged.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["broken".to_owned()],
    };
    imaged.digest = imaged.compute_digest()?;
    let (status, msg) = refusal(submit(&admin, &imaged).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("agentic"), "{msg}");
    // a ready read only instance can't hold a member that declares an effect
    let read_only = toolset(
        &admin,
        "reader",
        &[&a],
        ToolSetEntityRequest::default().read_only(true),
    )
    .await?;
    let mut spec = generators::gen_agentic_spec();
    if let thorium::models::ToolSpec::Agentic(tool) = &mut spec {
        tool.capabilities
            .effects
            .insert(thorium::models::ToolEffect::WritesThorium);
    }
    let (_, writer) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    let effects = generators::gen_toolset_instance(&read_only, 1, None, &[&writer]);
    let (status, msg) = refusal(submit(&admin, &effects).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("read only"), "{msg}");
    // more members than any resolution lists are refused before any is read
    let mut huge = invalid.clone();
    huge.revision = 2;
    huge.previous = Some(r1.id);
    let template = huge.members[0].clone();
    huge.members = (0..=4 * limits.max_candidates())
        .map(|index| {
            let mut member = template.clone();
            member.tool = Uuid::new_v4();
            member.tool_instance = Uuid::new_v4();
            member.function_name = format!("f_{index}");
            member
        })
        .collect();
    huge.digest = huge.compute_digest()?;
    let (status, msg) = refusal(submit(&admin, &huge).await);
    assert_eq!(status, 400, "{msg}");
    assert!(msg.contains("at most"), "{msg}");
    Ok(())
}
