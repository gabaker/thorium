//! Integration tests for agents and their graph edges
//!
//! These cover creating agents with their config and edges, the rules every edge follows (kind
//! pairs, agentic tools only, one of latest or pinned, and containment), validating a whole
//! request before anything is written, editing and deleting agents in every one of their
//! groups, and the generic association route applying the same rules.

use reqwest::multipart::Form;
use serde_json::json;
use std::time::Duration;
use thorium::models::{
    AgentConfig, AgentEntity, AgentEntityRequest, AssociationKind, AssociationRequest,
    AssociationTarget, Directionality, Entity, EntityKinds, EntityListOpts, EntityMetadata,
    EntityMetadataRequest, EntityMetadataUpdate, EntityRequest, EntityUpdate, GroupAllowed,
    ModelAuthLevel, ModelAuthSource, ModelCapabilities, ModelMatcher, ModelRequirements,
    ToolChoice, ToolEntityRequest, UserRole, WorkflowCatalogEntityRequest, WorkflowGraphChange,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser, god_group, moon_user};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

mod common;

use common::{
    agentic_tool, changes, changes_since, edges, harness_tool, ids, ordered_ids, other_id, queued,
    release_later, sorted, timed, timed_refusal, update_metadata,
};

/// Create an agent and get its id
///
/// # Arguments
///
/// * `client` - The client to create the agent with
/// * `name` - The agent's name
/// * `groups` - The groups to create the agent in
/// * `req` - The agent's config and edges
async fn agent(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: AgentEntityRequest,
) -> Result<Uuid, Error> {
    // create the agent and keep its id
    Ok(client
        .entities
        .create(generators::gen_agent(name, groups, req))
        .await?
        .id)
}

/// Build an agent request with a delegation config
///
/// # Arguments
///
/// * `function_name` - The agent's delegation function name
fn agent_req(function_name: &str) -> AgentEntityRequest {
    // a config another agent can delegate to
    AgentEntityRequest::new(generators::gen_agent_config(function_name))
}

/// Get an agent's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The agent entity
fn meta(entity: &Entity) -> &AgentEntity {
    // only agents have agent metadata
    match &entity.metadata {
        EntityMetadata::Agent(agent) => agent,
        other => panic!("expected an agent, got {other:?}"),
    }
}

/// Check that no association of an entity names another entity
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The entity whose associations to read
/// * `other` - The entity that must not be named
async fn no_rows_naming(admin: &Thorium, id: Uuid, other: Uuid) -> Result<(), Error> {
    // list every association of the entity
    let rows = admin.entities.associations(id).await?;
    let naming = rows
        .iter()
        .filter(|assoc| other_id(assoc) == Some(other))
        .count();
    assert_eq!(naming, 0, "{id} still has rows naming {other}: {rows:?}");
    Ok(())
}

/// Check that a graph neighbor has no agent edges at all
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The neighbor whose associations to read
async fn no_agent_edges(admin: &Thorium, id: Uuid) -> Result<(), Error> {
    // list the neighbor's associations and keep the agent edge kinds
    let rows = admin.entities.associations(id).await?;
    let agent_rows = rows
        .iter()
        .filter(|assoc| {
            matches!(
                assoc.kind,
                AssociationKind::Calls | AssociationKind::DelegatesTo
            )
        })
        .count();
    assert_eq!(agent_rows, 0, "{id} has agent edges: {rows:?}");
    Ok(())
}

/// Get a refusal's message
///
/// # Arguments
///
/// * `result` - The refused request
fn refusal<T>(result: Result<T, Error>) -> String {
    // the message of the error
    let msg = match result {
        Ok(_) => panic!("expected a refusal"),
        Err(err) => err.msg().unwrap_or_default(),
    };
    // unwrap the message from the API's error body when it's in one
    serde_json::from_str::<serde_json::Value>(&msg)
        .ok()
        .and_then(|body| body["error"].as_str().map(ToOwned::to_owned))
        .unwrap_or(msg)
}

/// Check that a refused agent create wrote nothing an admin can find
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `group` - A group the agent would have been in
/// * `name` - The agent's name
/// * `tag` - The value of the `role` tag the agent was sent with
async fn not_written(admin: &Thorium, group: &str, name: &str, tag: &str) -> Result<(), Error> {
    // no agent with this name is listed in the group
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .kind(EntityKinds::Agent)
        .limit(1000);
    let listed = admin.entities.list(&opts).await?;
    assert!(
        listed.data.iter().all(|line| line.name != name),
        "{name} was written to {group}"
    );
    // no entity carries the agent's tag
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .tag("role", tag)
        .limit(10);
    let tagged = admin.entities.list(&opts).await?;
    assert!(
        tagged.data.is_empty(),
        "{name}'s tag was written to {group}"
    );
    Ok(())
}

/// Send a raw multipart entity create as a user and get its status and body
///
/// # Arguments
///
/// * `user` - The user to send the create as
/// * `fields` - The form fields to send
async fn raw_create(user: &TestUser, fields: &[(&str, String)]) -> Result<(u16, String), Error> {
    // build the form from the fields
    let form = fields.iter().fold(Form::new(), |form, (name, value)| {
        form.text((*name).to_owned(), value.clone())
    });
    // send it as the user
    let url = format!("{}/api/entities/", user.client.host);
    let resp = reqwest::Client::new()
        .post(url)
        .header("authorization", user.auth_header())
        .multipart(form)
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// Add tags to an entity as a user, returning the status code
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `id` - The entity to tag
/// * `tags` - The tag request body
async fn raw_tags(user: &TestUser, id: Uuid, tags: serde_json::Value) -> Result<u16, Error> {
    // post the tags as the user
    let url = format!("{}/api/entities/tags/{id}", user.client.host);
    let resp = reqwest::Client::new()
        .post(url)
        .header("authorization", user.auth_header())
        .json(&tags)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Build an empty agent metadata update
fn agent_update() -> EntityMetadataUpdate {
    // an update that changes nothing
    EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    }
}

/// Build an agent update that adds and removes edges
///
/// # Arguments
///
/// * `add_tools` - The tools to add
/// * `remove_tools` - The tools to remove
/// * `add_sub_agents` - The sub-agents to add
/// * `remove_sub_agents` - The sub-agents to remove
fn edge_update(
    add_tools: &[Uuid],
    remove_tools: &[Uuid],
    add_sub_agents: &[Uuid],
    remove_sub_agents: &[Uuid],
) -> EntityMetadataUpdate {
    // an update with only edge changes
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
        remove_sub_agents: remove_sub_agents.to_vec(),
        extend_target_groups: false,
    }
}

/// Build an agent update that replaces the config
///
/// # Arguments
///
/// * `config` - The new config
fn config_update(config: AgentConfig) -> EntityMetadataUpdate {
    // an update with only a config
    let EntityMetadataUpdate::Agent {
        model_auth,
        harness,
        clear_harness,
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    EntityMetadataUpdate::Agent {
        config: Some(config),
        model_auth,
        harness,
        clear_harness,
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups,
    }
}

/// Get the groups an entity's rows are stored in
///
/// # Arguments
///
/// * `id` - The entity to inspect
async fn stored_groups(id: Uuid) -> Result<Vec<String>, Error> {
    // read the entity's rows and keep each group once
    let mut groups = test_utilities::entity_rows(id)
        .await?
        .into_iter()
        .map(|(group, _)| group)
        .collect::<Vec<String>>();
    groups.sort();
    groups.dedup();
    Ok(groups)
}

/// Sort owned group names
///
/// # Arguments
///
/// * `groups` - The groups to sort
fn sorted_groups(groups: &[&str]) -> Vec<String> {
    // own and sort the names
    let mut groups = groups
        .iter()
        .map(|group| (*group).to_owned())
        .collect::<Vec<String>>();
    groups.sort();
    groups
}

/// Build an agent update that only sets or clears the agent's harness
///
/// # Arguments
///
/// * `harness` - The harness to run in
/// * `clear` - Whether to clear the current harness first
fn harness_update(harness: Option<Uuid>, clear: bool) -> EntityMetadataUpdate {
    // an update that only changes the harness edge
    let EntityMetadataUpdate::Agent {
        config,
        model_auth,
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        clear_harness: clear.then_some(true),
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups,
    }
}

/// Get the id of an agent's harness
///
/// # Arguments
///
/// * `client` - The client to read the agent with
/// * `id` - The agent
async fn harness_of(client: &Thorium, id: Uuid) -> Result<Option<Uuid>, Error> {
    // read the agent with its edges filled in
    let read = client.entities.get(id).await?;
    Ok(meta(&read).harness.as_ref().map(|harness| harness.id))
}

/// An agent runs in at most one Harness tool or tool instance: other targets and a second harness
/// are refused naming the current one, a harness is only replaced by clearing it in the same
/// update, and every harness edge change queues an edge change
#[tokio::test]
// one scenario walks every harness edge rule against shared fixtures
#[allow(clippy::too_many_lines)]
async fn harness_edge_rules() -> Result<(), Error> {
    // io edits in minerva, which holds two harnesses, an agentic tool, and an image tool
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (first, first_r1) = harness_tool(&[&minerva], &io.client).await?;
    let (second, _) = harness_tool(&[&minerva], &io.client).await?;
    let (agentic, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (image, image_r1) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_image_spec(),
        &io.client,
    )
    .await?;
    // an agent created with a harness followed at latest runs in it
    let lead = agent(
        &io.client,
        "lead",
        &[&minerva],
        agent_req("lead").harness(first.id),
    )
    .await?;
    is!(harness_of(&io.client, lead).await?, Some(first.id));
    // an agent created pinned to a harness instance runs in that instance
    let pinner = agent(
        &io.client,
        "pinner",
        &[&minerva],
        agent_req("pinner").harness(first_r1.id),
    )
    .await?;
    is!(harness_of(&io.client, pinner).await?, Some(first_r1.id));
    // a tool or tool instance that isn't a Harness is refused, writing nothing
    for (name, target, kind) in [
        ("agentic", agentic.id, "Agentic"),
        ("image", image.id, "Image"),
        ("image-pin", image_r1.id, "Image"),
    ] {
        let tag = format!("{name}-{suffix}");
        let req = generators::gen_agent(name, &[&minerva], agent_req("a").harness(target))
            .tag("role", &tag);
        let resp = io.client.entities.create(req).await;
        fail!(resp, 400);
        let msg = refusal(resp);
        assert!(msg.contains("Harness") && msg.contains(kind), "{msg}");
        not_written(&admin, &minerva, name, &tag).await?;
    }
    // a second harness by update is refused naming the current one, and nothing changes
    let seen = changes(&admin, lead).await?.len();
    let resp = update_metadata(&io.client, lead, harness_update(Some(second.id), false)).await;
    fail!(resp, 400);
    let msg = refusal(resp);
    assert!(msg.contains(&first.name), "{msg}");
    is!(harness_of(&io.client, lead).await?, Some(first.id));
    // a second harness through the generic association route is refused the same way
    let target = AssociationTarget::Entity {
        id: second.id,
        name: second.name.clone(),
    };
    let source = AssociationTarget::Entity {
        id: lead,
        name: "lead".to_owned(),
    };
    let link = AssociationRequest::new(AssociationKind::RunsIn, source.clone()).target(target);
    let resp = io.client.associations.create(&link).await;
    fail!(resp, 400);
    let msg = refusal(resp);
    assert!(msg.contains(&first.name), "{msg}");
    is!(harness_of(&io.client, lead).await?, Some(first.id));
    // a non-harness through the generic association route is refused
    let target = AssociationTarget::Entity {
        id: agentic.id,
        name: agentic.name.clone(),
    };
    let link = AssociationRequest::new(AssociationKind::RunsIn, source).target(target);
    let resp = io.client.associations.create(&link).await;
    fail!(resp, 400);
    is!(
        changes_since(&admin, lead, seen).await?,
        Vec::<common::QueuedChange>::new()
    );
    // clearing the harness removes the edge in both directions and queues its removal
    let edge = |target, groups: &[&str]| {
        queued(
            &io.username,
            groups,
            WorkflowGraphChange::EdgeChanged {
                source: lead,
                target,
                kind: AssociationKind::RunsIn,
            },
        )
    };
    update_metadata(&io.client, lead, harness_update(None, true)).await?;
    is!(harness_of(&io.client, lead).await?, None::<Uuid>);
    no_rows_naming(&admin, lead, first.id).await?;
    no_rows_naming(&admin, first.id, lead).await?;
    is!(
        changes_since(&admin, lead, seen).await?,
        vec![edge(first.id, &[&minerva])]
    );
    // setting a harness on an agent without one adds the edge
    let seen = changes(&admin, lead).await?.len();
    update_metadata(&io.client, lead, harness_update(Some(second.id), false)).await?;
    is!(harness_of(&io.client, lead).await?, Some(second.id));
    is!(
        changes_since(&admin, lead, seen).await?,
        vec![edge(second.id, &[&minerva])]
    );
    // clearing and setting in one update replaces the harness, the removal queued first
    let seen = changes(&admin, lead).await?.len();
    update_metadata(&io.client, lead, harness_update(Some(first_r1.id), true)).await?;
    is!(harness_of(&io.client, lead).await?, Some(first_r1.id));
    no_rows_naming(&admin, lead, second.id).await?;
    is!(
        changes_since(&admin, lead, seen).await?,
        vec![edge(second.id, &[&minerva]), edge(first_r1.id, &[&minerva])]
    );
    // a deprecated harness instance can't be newly pinned
    let (third, third_r1) = harness_tool(&[&minerva], &io.client).await?;
    io.client
        .entities
        .update(third_r1.id, generators::gen_tool_deprecate(true))
        .await?;
    let resp = update_metadata(&io.client, lead, harness_update(Some(third_r1.id), true)).await;
    fail!(resp, 400);
    let msg = refusal(resp);
    assert!(msg.contains("deprecated"), "{msg}");
    is!(harness_of(&io.client, lead).await?, Some(first_r1.id));
    // the harness root itself is still a valid target
    update_metadata(&io.client, lead, harness_update(Some(third.id), true)).await?;
    is!(harness_of(&io.client, lead).await?, Some(third.id));
    Ok(())
}

/// Creating an agent stores its config and hash in every group, and its function name follows
/// the agentic function name rule
#[tokio::test]
async fn create_agent() -> Result<(), Error> {
    // io creates an agent in minerva and mars
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let mut config = generators::gen_agent_config("triage_lead");
    config.models = vec![ModelMatcher {
        model_id: "claude-opus".into(),
        ..ModelMatcher::default()
    }];
    let id = agent(
        &io.client,
        "triage-lead",
        &[&minerva, &mars],
        AgentEntityRequest::new(config.clone()),
    )
    .await?;
    // the agent keeps its config and its hash is the config's agent config digest
    let read = io.client.entities.get(id).await?;
    is!(read.kind, EntityKinds::Agent);
    is!(meta(&read).config.clone(), config.clone());
    let hash = meta(&read).config_hash.clone();
    assert!(hash.starts_with("v1:") && hash.len() == 67, "{hash}");
    is!(hash, config.config_hash()?);
    // it's stored in both groups
    is!(stored_groups(id).await?, sorted_groups(&[&minerva, &mars]));
    // one character and 64 allowed characters are accepted function names
    let longest = format!("a{}", "b_-9".repeat(15)) + "xyz";
    for (index, name) in ["a", longest.as_str()].iter().enumerate() {
        let req = agent_req(name);
        agent(&io.client, &format!("named-{index}"), &[&minerva], req).await?;
    }
    // invalid function names are refused naming the field, writing nothing
    let too_long = format!("{longest}c");
    for (index, name) in ["", too_long.as_str(), "has space", "dot.name"]
        .iter()
        .enumerate()
    {
        let agent_name = format!("bad-name-{index}");
        let tag = format!("bad-{index}-{suffix}");
        let req =
            generators::gen_agent(&agent_name, &[&minerva], agent_req(name)).tag("role", &tag);
        let resp = io.client.entities.create(req).await;
        fail!(resp, 400);
        assert!(refusal(resp).contains("function_name"), "{name}");
        not_written(&admin, &minerva, &agent_name, &tag).await?;
    }
    Ok(())
}

/// Config edits recompute the hash and replace the whole config, while the name, tags, and
/// model auth levels leave it alone
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn config_edits() -> Result<(), Error> {
    // an agent with two models in a group of io's
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
    let mut config = generators::gen_agent_config("analyst");
    config.models = vec![
        ModelMatcher {
            model_id: "b-model".into(),
            ..ModelMatcher::default()
        },
        ModelMatcher {
            model_id: "a-model".into(),
            ..ModelMatcher::default()
        },
    ];
    let id = agent(
        &io.client,
        "analyst",
        &[&minerva],
        AgentEntityRequest::new(config.clone()),
    )
    .await?;
    let first = io.client.entities.get(id).await?;
    let first_hash = meta(&first).config_hash.clone();
    let seen = changes(&admin, id).await?.len();
    // the models keep their order
    let model_ids = meta(&first)
        .config
        .models
        .iter()
        .map(|matcher| matcher.model_id.clone())
        .collect::<Vec<String>>();
    is!(model_ids, vec!["b-model".to_owned(), "a-model".to_owned()]);
    // renaming the agent, describing it, and tagging it leave the hash alone
    io.client
        .entities
        .update(
            id,
            EntityUpdate::default()
                .name("renamed-analyst")
                .description("reads reports"),
        )
        .await?;
    let tags = json!({"groups": [&minerva], "tags": {"team": ["blue"]}});
    let status = raw_tags(&io, id, tags).await?;
    is!(status, 204);
    let read = io.client.entities.get(id).await?;
    is!(read.name.clone(), "renamed-analyst".to_owned());
    is!(meta(&read).config_hash.clone(), first_hash.clone());
    // changing only the model auth levels leaves the config and its hash alone
    let levels = ModelAuthSource {
        levels: std::collections::BTreeSet::from([ModelAuthLevel::User]),
    };
    let EntityMetadataUpdate::Agent {
        config: c,
        harness,
        clear_harness,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    let auth_only = EntityMetadataUpdate::Agent {
        config: c,
        model_auth: Some(levels.clone()),
        harness,
        clear_harness,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    };
    update_metadata(&io.client, id, auth_only).await?;
    let read = io.client.entities.get(id).await?;
    is!(meta(&read).model_auth.clone(), levels);
    is!(meta(&read).config.clone(), config.clone());
    is!(meta(&read).config_hash.clone(), first_hash.clone());
    // none of those edits queued a change
    is!(changes_since(&admin, id, seen).await?.len(), 0);
    // changing the models or the requirements changes the hash
    let mut reordered = config.clone();
    reordered.models.reverse();
    update_metadata(&io.client, id, config_update(reordered.clone())).await?;
    let read = io.client.entities.get(id).await?;
    is!(meta(&read).config_hash.clone(), reordered.config_hash()?);
    assert_ne!(meta(&read).config_hash, first_hash);
    let mut required = reordered.clone();
    required.model_requirements = ModelRequirements {
        capabilities: ModelCapabilities {
            forced_tool_choice: true,
            ..ModelCapabilities::default()
        },
        ..ModelRequirements::default()
    };
    update_metadata(&io.client, id, config_update(required.clone())).await?;
    let read = io.client.entities.get(id).await?;
    is!(meta(&read).config_hash.clone(), required.config_hash()?);
    // a config update replaces the whole config, so fields left out return to their defaults
    let partial = AgentConfig {
        prompt: Some("only a prompt".into()),
        ..AgentConfig::default()
    };
    update_metadata(&io.client, id, config_update(partial.clone())).await?;
    let read = io.client.entities.get(id).await?;
    is!(meta(&read).config.clone(), partial.clone());
    is!(meta(&read).config_hash.clone(), partial.config_hash()?);
    is!(meta(&read).model_auth.clone(), levels);
    // each of the three config changes queued one agent update by io in the agent's group
    let updated = queued(
        &io.username,
        &[&minerva],
        WorkflowGraphChange::AgentUpdated { agent: id },
    );
    let expected = vec![updated; 3];
    let found = changes_since(&admin, id, seen).await?;
    is!(found, expected);
    // every stored row holds the new config
    for (_, kind_data) in test_utilities::entity_rows(id).await? {
        assert!(kind_data.contains("only a prompt"), "{kind_data}");
    }
    Ok(())
}

/// The config travels as one JSON form field, which is validated as a whole
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn agent_config_form_field() -> Result<(), Error> {
    // io in minerva
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
    // build the fields of a create from a raw config
    let fields = |name: &str, config: String| {
        vec![
            ("name", name.to_owned()),
            ("kind", "Agent".to_owned()),
            ("groups[]", minerva.clone()),
            ("tags[role][]", format!("{name}-{suffix}")),
            ("metadata[agent_config]", config),
        ]
    };
    // a whole config is stored exactly
    let config = generators::gen_agent_config("form_agent");
    let (status, body) =
        raw_create(&io, &fields("form-agent", serde_json::to_string(&config)?)).await?;
    is!(status, 200);
    let created: serde_json::Value = serde_json::from_str(&body)?;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let read = io.client.entities.get(id).await?;
    is!(meta(&read).config.clone(), config.clone());
    // the model auth field travels on its own and doesn't change the hash
    let mut with_auth = fields("form-auth", serde_json::to_string(&config)?);
    with_auth.push((
        "metadata[agent_model_auth]",
        r#"{"levels":["Group"]}"#.to_owned(),
    ));
    let (status, body) = raw_create(&io, &with_auth).await?;
    is!(status, 200);
    let created: serde_json::Value = serde_json::from_str(&body)?;
    let auth_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let auth_read = io.client.entities.get(auth_id).await?;
    is!(
        meta(&auth_read).config_hash.clone(),
        meta(&read).config_hash.clone()
    );
    is!(meta(&auth_read).model_auth.levels.len(), 1);
    // invalid JSON, a wrong type, and out of range limits are refused naming the field
    for (index, raw) in [
        "{not json".to_owned(),
        r#"{"max_turns": "ten"}"#.to_owned(),
        r#"{"max_tools": 4294967296}"#.to_owned(),
        r#"{"max_swarm_size": -1}"#.to_owned(),
        r#"{"tool_choice": {"Function": ""}}"#.to_owned(),
        r#"{"tool_choice": {"Function": "1scan"}}"#.to_owned(),
        r#"{"function_name": "1agent"}"#.to_owned(),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("form-bad-{index}");
        let (status, body) = raw_create(&io, &fields(&name, raw.clone())).await?;
        is!(status, 400);
        assert!(body.contains("agent_config"), "{raw}: {body}");
        not_written(&admin, &minerva, &name, &format!("{name}-{suffix}")).await?;
    }
    // the largest limit is accepted
    let (status, _) = raw_create(
        &io,
        &fields("form-max", r#"{"max_turns": 4294967295}"#.to_owned()),
    )
    .await?;
    is!(status, 200);
    // each tool call control round trips and gives its own hash
    let mut hashes = vec![meta(&read).config_hash.clone()];
    for (index, controlled) in [
        AgentConfig {
            tool_choice: ToolChoice::Required,
            ..config.clone()
        },
        AgentConfig {
            tool_choice: ToolChoice::None,
            ..config.clone()
        },
        AgentConfig {
            tool_choice: ToolChoice::Function("yara_scan".into()),
            ..config.clone()
        },
        AgentConfig {
            parallel_tool_calls: Some(false),
            ..config.clone()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let id = agent(
            &io.client,
            &format!("controls-{index}"),
            &[&minerva],
            AgentEntityRequest::new(controlled.clone()),
        )
        .await?;
        let read = io.client.entities.get(id).await?;
        is!(meta(&read).config.clone(), controlled);
        assert!(
            !hashes.contains(&meta(&read).config_hash),
            "control {index} reused a hash"
        );
        hashes.push(meta(&read).config_hash.clone());
    }
    Ok(())
}

/// Tool edges show latest tools and pinned instances, sub-agent edges show sub-agents, and one
/// create applies every edge request
#[tokio::test]
async fn create_with_edges() -> Result<(), Error> {
    // two agentic tools and a sub-agent in minerva
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
    let (latest, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (other, pinned) = agentic_tool(&[&minerva], &io.client).await?;
    let sub = agent(
        &io.client,
        "researcher",
        &[&minerva],
        agent_req("researcher"),
    )
    .await?;
    // an agent following one tool, pinned to another tool's instance, and delegating
    let req = agent_req("triage_lead")
        .tool(latest.id)
        .tool(pinned.id)
        .sub_agent(sub);
    let id = agent(&io.client, "triage-lead", &[&minerva], req).await?;
    // get shows each edge in its field
    let read = io.client.entities.get(id).await?;
    is!(ids(&meta(&read).latest_tools), vec![latest.id]);
    is!(ids(&meta(&read).pinned_tools), vec![pinned.id]);
    is!(ids(&meta(&read).sub_agents), vec![sub]);
    assert!(meta(&read).tool_sets.is_empty() && meta(&read).harness.is_none());
    // the admin association route shows two calls and one delegation from the agent
    is!(
        edges(&admin, id, AssociationKind::Calls, Directionality::To).await?,
        sorted(&[latest.id, pinned.id])
    );
    is!(
        edges(&admin, id, AssociationKind::DelegatesTo, Directionality::To).await?,
        vec![sub]
    );
    // each target sees the edge coming in
    is!(
        edges(
            &admin,
            latest.id,
            AssociationKind::Calls,
            Directionality::From
        )
        .await?,
        vec![id]
    );
    is!(
        edges(
            &admin,
            sub,
            AssociationKind::DelegatesTo,
            Directionality::From
        )
        .await?,
        vec![id]
    );
    // the other tool isn't linked, and the sub-agent lists no sub-agents of its own
    no_rows_naming(&admin, other.id, id).await?;
    let sub_read = io.client.entities.get(sub).await?;
    assert!(meta(&sub_read).sub_agents.is_empty());
    Ok(())
}

/// A create with any invalid edge is refused and writes no entity, tag, census entry, or edge
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn refused_edges() -> Result<(), Error> {
    // tools and agents in minerva, a tool only in minerva, and mars
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (valid, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let (narrow, narrow_instance) = agentic_tool(&[&minerva], &io.client).await?;
    let (image, image_instance) = generators::tool(
        &[&minerva, &mars],
        "1.0.0",
        &generators::gen_image_spec(),
        &io.client,
    )
    .await?;
    let other = agent(&io.client, "other", &[&minerva, &mars], agent_req("other")).await?;
    // each request names the valid tool plus one bad edge
    let cases: Vec<(&str, AgentEntityRequest, u16)> = vec![
        (
            "image-tool",
            agent_req("a").tool(valid.id).tool(image.id),
            400,
        ),
        (
            "image-instance",
            agent_req("a").tool(valid.id).tool(image_instance.id),
            400,
        ),
        (
            "calls-agent",
            agent_req("a").tool(valid.id).tool(other),
            400,
        ),
        (
            "delegates-tool",
            agent_req("a").tool(valid.id).sub_agent(valid.id),
            400,
        ),
        (
            "missing-target",
            agent_req("a").tool(valid.id).tool(Uuid::new_v4()),
            400,
        ),
        (
            "toolset-tool",
            agent_req("a").tool(valid.id).tool_set(valid.id),
            400,
        ),
        (
            "latest-and-pinned",
            agent_req("a").tool(narrow.id).tool(narrow_instance.id),
            400,
        ),
        (
            "harness-tool",
            agent_req("a").tool(valid.id).harness(valid.id),
            400,
        ),
    ];
    for (name, req, code) in cases {
        let tag = format!("{name}-{suffix}");
        let resp = io
            .client
            .entities
            .create(generators::gen_agent(name, &[&minerva], req).tag("role", &tag))
            .await;
        fail!(resp, code);
        not_written(&admin, &minerva, name, &tag).await?;
    }
    // a target missing one of the agent's groups is refused listing the group for that target
    let isolated = god_group(&admin, "vesta", &suffix, &roles).await?;
    let tag = format!("narrow-{suffix}");
    let req = generators::gen_agent(
        "narrow",
        &[&minerva, &isolated],
        agent_req("a").tool(narrow.id),
    )
    .tag("role", &tag);
    let resp = io.client.entities.create(req).await;
    fail!(resp, 400);
    let msg = refusal(resp);
    assert!(
        msg.contains(&narrow.name) && msg.contains(&isolated),
        "{msg}"
    );
    not_written(&admin, &minerva, "narrow", &tag).await?;
    not_written(&admin, &isolated, "narrow", &tag).await?;
    // the group no other entity is in got no census entry
    is!(
        test_utilities::entity_census_exists(&isolated).await?,
        false
    );
    // a group that disallows associations refuses the edges with a 401
    let closed = generators::named_group_with_roles(
        &generators::readable_name("janus", &suffix),
        &roles,
        GroupAllowed::default().disable_associations(),
        &admin,
    )
    .await?;
    let (closed_tool, _) = agentic_tool(&[&closed], &io.client).await?;
    let tag = format!("closed-{suffix}");
    let req = generators::gen_agent("closed", &[&closed], agent_req("a").tool(closed_tool.id))
        .tag("role", &tag);
    let resp = io.client.entities.create(req).await;
    fail!(resp, 401);
    not_written(&admin, &closed, "closed", &tag).await?;
    // no target has an agent edge
    for id in [
        valid.id,
        narrow.id,
        narrow_instance.id,
        image.id,
        other,
        closed_tool.id,
    ] {
        no_agent_edges(&admin, id).await?;
    }
    Ok(())
}

/// The generic association route applies the same kind pairs, tool kinds, and containment
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn generic_route_rules() -> Result<(), Error> {
    // two agents, an agentic tool, and an image tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let lead = agent(&io.client, "lead", &[&minerva, &mars], agent_req("lead")).await?;
    let helper = agent(
        &io.client,
        "helper",
        &[&minerva, &mars],
        agent_req("helper"),
    )
    .await?;
    let (tool, instance) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let (narrow, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (image, _) = generators::tool(
        &[&minerva, &mars],
        "1.0.0",
        &generators::gen_image_spec(),
        &io.client,
    )
    .await?;
    let entity = |id: Uuid, name: &str| AssociationTarget::Entity {
        id,
        name: name.to_owned(),
    };
    let link = |kind, target: AssociationTarget| {
        AssociationRequest::new(kind, entity(lead, "lead")).target(target)
    };
    // invalid kind pairs, an image tool, and a target missing a group are refused
    for (case, req) in [
        (
            "calls agent",
            link(AssociationKind::Calls, entity(helper, "helper")),
        ),
        (
            "delegates tool",
            link(AssociationKind::DelegatesTo, entity(tool.id, &tool.name)),
        ),
        (
            "orchestrates agent",
            link(AssociationKind::Orchestrates, entity(helper, "helper")),
        ),
        (
            "built from agent",
            link(AssociationKind::BuiltFrom, entity(helper, "helper")),
        ),
        (
            "runs in agent",
            link(AssociationKind::RunsIn, entity(helper, "helper")),
        ),
        (
            "includes tool",
            link(AssociationKind::Includes, entity(tool.id, &tool.name)),
        ),
        (
            "calls image",
            link(AssociationKind::Calls, entity(image.id, &image.name)),
        ),
        (
            "narrow",
            link(AssociationKind::Calls, entity(narrow.id, &narrow.name)),
        ),
    ] {
        let resp = io.client.associations.create(&req).await;
        assert!(resp.is_err(), "{case} was accepted");
        fail!(resp, 400);
    }
    no_rows_naming(&admin, helper, lead).await?;
    no_rows_naming(&admin, tool.id, lead).await?;
    no_agent_edges(&admin, image.id).await?;
    no_agent_edges(&admin, narrow.id).await?;
    // the refused edges queued nothing
    is!(changes(&admin, lead).await?.len(), 0);
    // a valid call and delegation are created and shown on get
    io.client
        .associations
        .create(&link(AssociationKind::Calls, entity(tool.id, &tool.name)))
        .await?;
    io.client
        .associations
        .create(&link(
            AssociationKind::DelegatesTo,
            entity(helper, "helper"),
        ))
        .await?;
    let read = io.client.entities.get(lead).await?;
    is!(ids(&meta(&read).latest_tools), vec![tool.id]);
    is!(ids(&meta(&read).sub_agents), vec![helper]);
    // each queued one edge change by io in every group of the agent
    let edge = |target, kind| {
        queued(
            &io.username,
            &[&minerva, &mars],
            WorkflowGraphChange::EdgeChanged {
                source: lead,
                target,
                kind,
            },
        )
    };
    is!(
        changes(&admin, lead).await?,
        vec![
            edge(tool.id, AssociationKind::Calls),
            edge(helper, AssociationKind::DelegatesTo)
        ]
    );
    // an association that isn't a graph edge queues nothing
    io.client
        .associations
        .create(&link(
            AssociationKind::AssociatedWith,
            entity(helper, "helper"),
        ))
        .await?;
    is!(changes(&admin, lead).await?.len(), 2);
    // the tool's instance can't be pinned next to its latest edge
    let resp = io
        .client
        .associations
        .create(&link(
            AssociationKind::Calls,
            entity(instance.id, &instance.name),
        ))
        .await;
    fail!(resp, 400);
    is!(changes(&admin, lead).await?.len(), 2);
    Ok(())
}

/// Each edge update field adds or removes exactly its edge, and an update with any invalid edge
/// changes nothing
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn update_edge_fields() -> Result<(), Error> {
    // an agent with one tool and one sub-agent
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
    let (first, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (second, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (third, _) = agentic_tool(&[&minerva], &io.client).await?;
    let sub = agent(&io.client, "sub", &[&minerva], agent_req("sub")).await?;
    let other_sub = agent(&io.client, "other-sub", &[&minerva], agent_req("other_sub")).await?;
    let id = agent(
        &io.client,
        "lead",
        &[&minerva],
        agent_req("lead").tool(first.id).sub_agent(sub),
    )
    .await?;
    let before = io.client.entities.get(id).await?;
    let seen = changes(&admin, id).await?.len();
    let edge = |target, kind| {
        queued(
            &io.username,
            &[&minerva],
            WorkflowGraphChange::EdgeChanged {
                source: id,
                target,
                kind,
            },
        )
    };
    // adding a tool adds exactly it and advances modified
    update_metadata(&io.client, id, edge_update(&[second.id], &[], &[], &[])).await?;
    let read = io.client.entities.get(id).await?;
    is!(
        ids(&meta(&read).latest_tools),
        sorted(&[first.id, second.id])
    );
    assert!(meta(&read).modified > meta(&before).modified);
    // removing a tool removes exactly it, in both directions
    update_metadata(&io.client, id, edge_update(&[], &[first.id], &[], &[])).await?;
    let read = io.client.entities.get(id).await?;
    is!(ids(&meta(&read).latest_tools), vec![second.id]);
    no_rows_naming(&admin, id, first.id).await?;
    no_rows_naming(&admin, first.id, id).await?;
    // one request adds one tool and removes another
    update_metadata(
        &io.client,
        id,
        edge_update(&[third.id], &[second.id], &[], &[]),
    )
    .await?;
    let read = io.client.entities.get(id).await?;
    is!(ids(&meta(&read).latest_tools), vec![third.id]);
    no_rows_naming(&admin, second.id, id).await?;
    // sub-agents are added and removed the same way
    update_metadata(&io.client, id, edge_update(&[], &[], &[other_sub], &[sub])).await?;
    let read = io.client.entities.get(id).await?;
    is!(ids(&meta(&read).sub_agents), vec![other_sub]);
    no_rows_naming(&admin, sub, id).await?;
    // each changed edge queued one edge change, an update's removals before its additions
    let calls = AssociationKind::Calls;
    let delegates = AssociationKind::DelegatesTo;
    is!(
        changes_since(&admin, id, seen).await?,
        vec![
            edge(second.id, calls),
            edge(first.id, calls),
            edge(second.id, calls),
            edge(third.id, calls),
            edge(sub, delegates),
            edge(other_sub, delegates),
        ]
    );
    let seen = changes(&admin, id).await?.len();
    // removing edges that don't exist changes nothing
    let EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    let absent = EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        clear_harness: Some(true),
        add_tools: Vec::new(),
        remove_tools: vec![first.id],
        add_tool_sets: Vec::new(),
        remove_tool_sets: vec![Uuid::new_v4()],
        add_sub_agents: Vec::new(),
        remove_sub_agents: vec![sub],
        extend_target_groups: false,
    };
    update_metadata(&io.client, id, absent).await?;
    let unchanged = io.client.entities.get(id).await?;
    is!(ids(&meta(&unchanged).latest_tools), vec![third.id]);
    is!(ids(&meta(&unchanged).sub_agents), vec![other_sub]);
    // an update adding one valid and one invalid edge changes nothing
    let mut edited = meta(&unchanged).config.clone();
    edited.prompt = Some("never stored".into());
    let EntityMetadataUpdate::Agent {
        model_auth,
        harness,
        clear_harness,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    let mixed = EntityMetadataUpdate::Agent {
        config: Some(edited),
        model_auth,
        harness,
        clear_harness,
        add_tools: vec![first.id, Uuid::new_v4()],
        remove_tools: vec![third.id],
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    };
    let resp = update_metadata(&io.client, id, mixed).await;
    fail!(resp, 400);
    let after = io.client.entities.get(id).await?;
    is!(meta(&after).config.clone(), meta(&unchanged).config.clone());
    is!(
        meta(&after).config_hash.clone(),
        meta(&unchanged).config_hash.clone()
    );
    is!(meta(&after).modified, meta(&unchanged).modified);
    is!(after.tags.clone(), unchanged.tags.clone());
    is!(ids(&meta(&after).latest_tools), vec![third.id]);
    no_rows_naming(&admin, first.id, id).await?;
    // neither the update removing absent edges nor the refused update queued anything
    is!(changes_since(&admin, id, seen).await?.len(), 0);
    Ok(())
}

/// Delegation cycles and self-delegation are allowed, and each sub-agent is listed once
#[tokio::test]
async fn delegation_cycles() -> Result<(), Error> {
    // two agents that delegate to each other
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
    let b = agent(&io.client, "b", &[&minerva], agent_req("agent_b")).await?;
    let a = agent(
        &io.client,
        "a",
        &[&minerva],
        agent_req("agent_a").sub_agent(b),
    )
    .await?;
    update_metadata(&io.client, b, edge_update(&[], &[], &[a], &[])).await?;
    // a also delegates to itself
    update_metadata(&io.client, a, edge_update(&[], &[], &[a], &[])).await?;
    // a lists itself and b once each, and b lists a
    let read = io.client.entities.get(a).await?;
    let subs = meta(&read)
        .sub_agents
        .iter()
        .map(|sub| sub.id)
        .collect::<Vec<Uuid>>();
    is!(subs.len(), 2);
    is!(sorted(&subs), sorted(&[a, b]));
    let read = io.client.entities.get(b).await?;
    is!(ids(&meta(&read).sub_agents), vec![a]);
    Ok(())
}

/// An agent that another agent delegates to or a workflow orchestrates can't be deleted until
/// they are, and deleting it then removes every one of its own edges
#[tokio::test]
async fn delete_removes_edges() -> Result<(), Error> {
    // an agent with two tools and a sub-agent that another agent delegates to
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (first, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let (_, pinned) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let sub = agent(&io.client, "sub", &[&minerva, &mars], agent_req("sub")).await?;
    let doomed = agent(
        &io.client,
        "doomed",
        &[&minerva, &mars],
        agent_req("doomed")
            .tool(first.id)
            .tool(pinned.id)
            .sub_agent(sub),
    )
    .await?;
    let parent = agent(
        &io.client,
        "parent",
        &[&minerva, &mars],
        agent_req("parent").sub_agent(doomed),
    )
    .await?;
    let req = WorkflowCatalogEntityRequest::default().agent(doomed);
    let workflow = io
        .client
        .entities
        .create(generators::gen_workflow("lead", &[&minerva, &mars], req))
        .await?
        .id;
    // deleting it is refused naming the parent and the workflow, and nothing is removed
    fail!(io.client.entities.delete(doomed).await, 409, "parent");
    fail!(io.client.entities.delete(doomed).await, 409, "lead");
    io.client.entities.get(doomed).await?;
    // once the workflow and the parent are gone it can be deleted
    io.client.entities.delete(workflow).await?;
    io.client.entities.delete(parent).await?;
    let seen = changes(&admin, doomed).await?.len();
    io.client.entities.delete(doomed).await?;
    fail!(io.client.entities.get(doomed).await, 404);
    // no former neighbor has a row naming it
    for neighbor in [first.id, pinned.id, sub] {
        no_rows_naming(&admin, neighbor, doomed).await?;
    }
    // each of the three removed edges queued one edge change in both groups
    let edge = |source, target, kind| {
        queued(
            &io.username,
            &[&minerva, &mars],
            WorkflowGraphChange::EdgeChanged {
                source,
                target,
                kind,
            },
        )
    };
    let mut removed = changes_since(&admin, doomed, seen).await?;
    let mut expected = vec![
        edge(doomed, first.id, AssociationKind::Calls),
        edge(doomed, pinned.id, AssociationKind::Calls),
        edge(doomed, sub, AssociationKind::DelegatesTo),
    ];
    let order = |change: &common::QueuedChange| format!("{:?}", change.change);
    removed.sort_by_key(order);
    expected.sort_by_key(order);
    is!(removed, expected);
    Ok(())
}

/// Edges need targets that cover the agent's groups, unless the caller widens the targets
#[tokio::test]
async fn containment_and_widening() -> Result<(), Error> {
    // io edits entities in minerva and mars
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let both = GroupRoles::default()
        .user(&io.username)
        .user(&europa.username);
    let minerva = god_group(&admin, "minerva", &suffix, &both).await?;
    let mars = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default()
            .user(&io.username)
            .monitor(&europa.username),
    )
    .await?;
    // a sub-agent and tools only in minerva
    let sub = agent(&io.client, "sub", &[&minerva], agent_req("sub")).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (pinned_tool, pinned) = agentic_tool(&[&minerva], &io.client).await?;
    io.client
        .entities
        .update(
            pinned_tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    // an agent in minerva and mars can't delegate to it, and the refusal names mars
    for (name, req) in [
        ("delegating", agent_req("a").sub_agent(sub)),
        ("calling", agent_req("a").tool(tool.id)),
        ("pinning", agent_req("a").tool(pinned.id)),
    ] {
        let resp = io
            .client
            .entities
            .create(generators::gen_agent(name, &[&minerva, &mars], req))
            .await;
        fail!(resp, 400);
        assert!(refusal(resp).contains(&mars));
    }
    // an agent only in minerva may use targets in more groups
    let (wide, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    agent(
        &io.client,
        "narrow",
        &[&minerva],
        agent_req("narrow").tool(wide.id),
    )
    .await?;
    // europa can't edit entities in mars, so she can't create an agent there to widen from
    let req = agent_req("a").sub_agent(sub).extend_target_groups();
    let resp = europa
        .client
        .entities
        .create(generators::gen_agent(
            "europa-agent",
            &[&minerva, &mars],
            req,
        ))
        .await;
    fail!(resp, 401);
    is!(stored_groups(sub).await?, sorted_groups(&[&minerva]));
    // io widens the sub-agent, the tool, and the pinned instance's tool and every instance
    let req = agent_req("a")
        .sub_agent(sub)
        .tool(tool.id)
        .tool(pinned.id)
        .extend_target_groups();
    let id = agent(&io.client, "widened", &[&minerva, &mars], req).await?;
    let both_groups = sorted_groups(&[&minerva, &mars]);
    is!(stored_groups(sub).await?, both_groups.clone());
    is!(stored_groups(tool.id).await?, both_groups.clone());
    is!(stored_groups(pinned_tool.id).await?, both_groups.clone());
    for summary in admin.entities.tool_instances(pinned_tool.id).await? {
        is!(stored_groups(summary.id).await?, both_groups.clone());
    }
    let read = io.client.entities.get(id).await?;
    is!(ids(&meta(&read).sub_agents), vec![sub]);
    is!(ids(&meta(&read).pinned_tools), vec![pinned.id]);
    Ok(())
}

/// A widening that fails for one target widens no target, and a tool create with an invalid
/// spec writes nothing
#[tokio::test]
async fn widening_all_or_nothing() -> Result<(), Error> {
    // a tool and a sub-agent in minerva, the sub-agent calling a tool only in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    let (inner, _) = agentic_tool(&[&minerva], &io.client).await?;
    let sub = agent(
        &io.client,
        "sub",
        &[&minerva],
        agent_req("sub").tool(inner.id),
    )
    .await?;
    // widening the sub-agent would break its own edge to the inner tool
    let req = agent_req("a")
        .tool(tool.id)
        .sub_agent(sub)
        .extend_target_groups();
    let resp = io
        .client
        .entities
        .create(generators::gen_agent(
            "all-or-nothing",
            &[&minerva, &mars],
            req,
        ))
        .await;
    fail!(resp, 400);
    // neither target was widened
    is!(stored_groups(tool.id).await?, sorted_groups(&[&minerva]));
    is!(stored_groups(sub).await?, sorted_groups(&[&minerva]));
    // a tool create with an invalid spec writes no tool, instance, tag, or census entry
    let fresh = god_group(&admin, "ceres", &suffix, &roles).await?;
    let mut spec = generators::gen_agentic_spec();
    if let thorium::models::ToolSpec::Agentic(agentic) = &mut spec {
        agentic.function_name = "1bad".into();
    }
    let meta = ToolEntityRequest::new("1.0.0", &spec)?;
    let req = EntityRequest::new(
        "bad-tool",
        EntityMetadataRequest::Tool(meta),
        vec![fresh.clone()],
    )
    .tag("role", format!("bad-tool-{suffix}"));
    let resp = io.client.entities.create(req).await;
    fail!(resp, 400);
    let listed = admin
        .entities
        .list(
            &EntityListOpts::default()
                .groups(vec![fresh.clone()])
                .limit(10),
        )
        .await?;
    assert!(
        listed.data.is_empty(),
        "the tool create wrote {:?}",
        listed.data
    );
    is!(test_utilities::entity_census_exists(&fresh).await?, false);
    Ok(())
}

/// Build an agent update that adds tools and widens them when they miss some of its groups
///
/// # Arguments
///
/// * `tools` - The tools or tool instances to add
fn widening_update(tools: &[Uuid]) -> EntityMetadataUpdate {
    // an update that only adds tools, with widening on
    let EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        clear_harness,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        clear_harness,
        add_tools: tools.to_vec(),
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups: true,
    }
}

/// Check that a widening was refused as one the caller may not make
///
/// # Arguments
///
/// * `resp` - The refused request
/// * `group` - The group the target would have gained
fn refused_widening<T>(resp: Result<T, Error>, group: &str) {
    // the refusal is a 400 naming the missing group and saying the target can't be widened
    let status = resp
        .as_ref()
        .err()
        .and_then(Error::status)
        .map(|status| status.as_u16());
    let msg = refusal(resp);
    assert_eq!(status, Some(400), "{msg}");
    assert!(msg.contains(group), "{msg}");
    assert!(msg.contains("can't widen"), "{msg}");
}

/// Widening a target needs the rights a group add on it needs: edit rights in every one of the
/// target's groups and in each added group, on every route that widens
#[tokio::test]
// one scenario walks every widening route against shared fixtures
#[allow(clippy::too_many_lines)]
async fn widening_needs_edit_rights_on_target() -> Result<(), Error> {
    // europa owns minerva and juno and submits the tools, callisto monitors minerva, edits juno,
    // and owns vesta, and io edits entities in minerva and vesta
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
    let juno = god_group(
        &admin,
        "juno",
        &suffix,
        &GroupRoles::default()
            .owner(&europa.username)
            .user(&callisto.username),
    )
    .await?;
    let mars = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
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
    // europa's tool in minerva with two instances, and a tool in juno and mars
    let (tool, first) = agentic_tool(&[&minerva], &europa.client).await?;
    europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let (split, _) = agentic_tool(&[&juno, &mars], &europa.client).await?;
    // callisto's agent in vesta
    let grabber = agent(&callisto.client, "grabber", &[&vesta], agent_req("grabber")).await?;
    // callisto can't widen the tool through an agent's latest or pinned edge
    let resp = update_metadata(&callisto.client, grabber, widening_update(&[tool.id])).await;
    refused_widening(resp, &vesta);
    let resp = update_metadata(&callisto.client, grabber, widening_update(&[first.id])).await;
    refused_widening(resp, &vesta);
    // nor through the generic association route
    let generic = AssociationRequest::new(
        AssociationKind::Calls,
        AssociationTarget::Entity {
            id: grabber,
            name: "grabber".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: tool.id,
        name: tool.name.clone(),
    })
    .extend_target_groups();
    let resp = callisto.client.associations.create(&generic).await;
    refused_widening(resp, &vesta);
    // editing a tool in only some of its groups isn't enough either
    let resp = update_metadata(&callisto.client, grabber, widening_update(&[split.id])).await;
    refused_widening(resp, &vesta);
    is!(
        stored_groups(split.id).await?,
        sorted_groups(&[&juno, &mars])
    );
    // the tool and both of its instances are still only in minerva
    let instances = admin.entities.tool_instances(tool.id).await?;
    assert_eq!(instances.len(), 2, "{instances:?}");
    is!(stored_groups(tool.id).await?, sorted_groups(&[&minerva]));
    for summary in &instances {
        is!(stored_groups(summary.id).await?, sorted_groups(&[&minerva]));
    }
    // no refused edge was written
    let read = callisto.client.entities.get(grabber).await?;
    assert!(meta(&read).latest_tools.is_empty());
    assert!(meta(&read).pinned_tools.is_empty());
    for target in [tool.id, first.id, split.id] {
        no_rows_naming(&admin, target, grabber).await?;
    }
    // europa still publishes and edits her tool
    europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("3.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    europa
        .client
        .entities
        .update(
            tool.id,
            EntityUpdate::default().description("still europa's"),
        )
        .await?;
    let read = europa.client.entities.get(tool.id).await?;
    is!(read.description, Some("still europa's".to_owned()));
    // io edits entities in minerva and vesta, so the same link widens the tool and every
    // instance into vesta
    update_metadata(&io.client, grabber, widening_update(&[tool.id])).await?;
    let both = sorted_groups(&[&minerva, &vesta]);
    is!(stored_groups(tool.id).await?, both.clone());
    let instances = admin.entities.tool_instances(tool.id).await?;
    assert_eq!(instances.len(), 3, "{instances:?}");
    for summary in &instances {
        is!(stored_groups(summary.id).await?, both.clone());
    }
    let read = io.client.entities.get(grabber).await?;
    is!(ids(&meta(&read).latest_tools), vec![tool.id]);
    Ok(())
}

/// The generic route refuses a source the caller can't see or edit without waiting on its lock
#[tokio::test]
async fn generic_route_authorizes_before_locking() -> Result<(), Error> {
    // io edits minerva, ganymede only monitors it, and amalthea is only in mars
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .user(&io.username)
            .monitor(&ganymede.username),
    )
    .await?;
    god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default().user(&amalthea.username),
    )
    .await?;
    // io's agent and tool in minerva
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    let lead = agent(&io.client, "lead", &[&minerva], agent_req("lead")).await?;
    let calls = AssociationRequest::new(
        AssociationKind::Calls,
        AssociationTarget::Entity {
            id: lead,
            name: "lead".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: tool.id,
        name: tool.name.clone(),
    });
    // amalthea can't see the agent, so she gets a 404 at once while another request holds its
    // lock
    test_utilities::hold_instance_lock(lead).await?;
    let ((elapsed, status), released) = tokio::join!(
        timed_refusal(amalthea.client.associations.create(&calls)),
        release_later(lead, Duration::from_millis(3_000))
    );
    released?;
    assert_eq!(status, 404);
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    // ganymede sees the agent but can't edit it, so he gets a 401 at once
    test_utilities::hold_instance_lock(lead).await?;
    let ((elapsed, status), released) = tokio::join!(
        timed_refusal(ganymede.client.associations.create(&calls)),
        release_later(lead, Duration::from_millis(3_000))
    );
    released?;
    assert_eq!(status, 401);
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    // neither refusal wrote an edge, and io's own request still works
    no_rows_naming(&admin, tool.id, lead).await?;
    io.client.associations.create(&calls).await?;
    let read = io.client.entities.get(lead).await?;
    is!(ids(&meta(&read).latest_tools), vec![tool.id]);
    Ok(())
}

/// Edits and deletes need rights in every group, monitors can't create, analysts can, and a
/// sub-agent can't lose a group an agent delegating to it is in
#[tokio::test]
async fn full_view_rights() -> Result<(), Error> {
    // io and europa edit in minerva, only io edits in mars, callisto monitors both, ganymede
    // manages both, and himalia is an analyst member of minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let himalia = generators::named_user_with_role(
        &admin,
        &generators::readable_name("himalia", &suffix),
        UserRole::Analyst,
    )
    .await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .user(&io.username)
            .user(&europa.username)
            .user(&himalia.username)
            .monitor(&callisto.username)
            .manager(&ganymede.username),
    )
    .await?;
    let mars = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default()
            .user(&io.username)
            .monitor(&europa.username)
            .monitor(&callisto.username)
            .manager(&ganymede.username),
    )
    .await?;
    let (tool, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let sub = agent(&io.client, "sub", &[&minerva, &mars], agent_req("sub")).await?;
    let id = agent(
        &io.client,
        "shared",
        &[&minerva, &mars],
        agent_req("shared").sub_agent(sub),
    )
    .await?;
    let before = io.client.entities.get(id).await?;
    let rows_before = test_utilities::entity_rows(id).await?;
    // europa can't edit the config, add an edge, or delete it
    let resp = update_metadata(&europa.client, id, config_update(AgentConfig::default())).await;
    fail!(resp, 401);
    assert!(refusal(resp).contains("you don't have permission to edit `shared`"));
    let resp = update_metadata(&europa.client, id, edge_update(&[tool.id], &[], &[], &[])).await;
    fail!(resp, 401);
    assert!(refusal(resp).contains("you don't have permission to edit `shared`"));
    let resp = europa.client.entities.delete(id).await;
    fail!(resp, 401);
    assert!(refusal(resp).contains("you don't have permission to delete `shared`"));
    // the agent, its edges, and its rows are unchanged
    let after = io.client.entities.get(id).await?;
    is!(
        meta(&after).config_hash.clone(),
        meta(&before).config_hash.clone()
    );
    assert!(meta(&after).latest_tools.is_empty());
    is!(ids(&meta(&after).sub_agents), vec![sub]);
    is!(test_utilities::entity_rows(id).await?, rows_before);
    // callisto, a monitor of both groups, can't create one
    let resp = callisto
        .client
        .entities
        .create(generators::gen_agent(
            "monitored",
            &[&minerva, &mars],
            agent_req("m"),
        ))
        .await;
    fail!(resp, 401);
    // himalia, an analyst member, can
    agent(
        &himalia.client,
        "analyzed",
        &[&minerva],
        agent_req("analyzed"),
    )
    .await?;
    // mars can't be removed from the sub-agent while the shared agent in mars delegates to it
    let remove = EntityUpdate::default().remove_group(&mars);
    let resp = io.client.entities.update(sub, remove).await;
    fail!(resp, 409);
    assert!(refusal(resp).contains("shared"));
    is!(stored_groups(sub).await?, sorted_groups(&[&minerva, &mars]));
    // ganymede, a manager of both groups, edits and deletes it
    update_metadata(
        &ganymede.client,
        id,
        config_update(generators::gen_agent_config("managed")),
    )
    .await?;
    ganymede.client.entities.delete(id).await?;
    fail!(io.client.entities.get(id).await, 404);
    Ok(())
}

/// Concurrent edge adds both persist, and concurrent config edits leave every row agreeing
#[tokio::test]
async fn concurrent_writes() -> Result<(), Error> {
    // an agent in minerva and mars and two tools
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (first, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let (second, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let id = agent(&io.client, "busy", &[&minerva, &mars], agent_req("busy")).await?;
    let seen = changes(&admin, id).await?.len();
    // add both tools at once
    let (left, right) = tokio::join!(
        update_metadata(&io.client, id, edge_update(&[first.id], &[], &[], &[])),
        update_metadata(&io.client, id, edge_update(&[second.id], &[], &[], &[])),
    );
    left?;
    right?;
    let read = io.client.entities.get(id).await?;
    is!(
        ids(&meta(&read).latest_tools),
        sorted(&[first.id, second.id])
    );
    // each add queued its own edge change
    let mut added = changes_since(&admin, id, seen)
        .await?
        .into_iter()
        .map(|queued| queued.change)
        .collect::<Vec<WorkflowGraphChange>>();
    added.sort_by_key(|change| format!("{change:?}"));
    let mut expected = [first.id, second.id]
        .into_iter()
        .map(|target| WorkflowGraphChange::EdgeChanged {
            source: id,
            target,
            kind: AssociationKind::Calls,
        })
        .collect::<Vec<WorkflowGraphChange>>();
    expected.sort_by_key(|change| format!("{change:?}"));
    is!(added, expected);
    let seen = changes(&admin, id).await?.len();
    // edit the config twice at once
    let one = generators::gen_agent_config("first_edit");
    let two = generators::gen_agent_config("second_edit");
    let (left, right) = tokio::join!(
        update_metadata(&io.client, id, config_update(one)),
        update_metadata(&io.client, id, config_update(two)),
    );
    left?;
    right?;
    // both rows hold the same config, and its hash matches it
    let rows = test_utilities::entity_rows(id).await?;
    is!(rows.len(), 2);
    let configs = rows
        .iter()
        .map(|(_, data)| {
            let value: serde_json::Value = serde_json::from_str(data).unwrap();
            value["Agent"].clone()
        })
        .collect::<Vec<serde_json::Value>>();
    is!(configs[0]["config"].clone(), configs[1]["config"].clone());
    let config: AgentConfig = serde_json::from_value(configs[0]["config"].clone())?;
    is!(
        configs[0]["config_hash"].as_str().unwrap().to_owned(),
        config.config_hash()?
    );
    // the tools are still linked
    let read = io.client.entities.get(id).await?;
    is!(
        ids(&meta(&read).latest_tools),
        sorted(&[first.id, second.id])
    );
    // each config edit queued its own agent update
    let updated = queued(
        &io.username,
        &[&minerva, &mars],
        WorkflowGraphChange::AgentUpdated { agent: id },
    );
    let expected = vec![updated; 2];
    let found = changes_since(&admin, id, seen).await?;
    is!(found, expected);
    Ok(())
}

/// Users outside an agent's groups can't see it, and members can
#[tokio::test]
async fn visibility() -> Result<(), Error> {
    // an agent in minerva and amalthea in mercury
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let mercury = god_group(
        &admin,
        "mercury",
        &suffix,
        &GroupRoles::default().user(&amalthea.username),
    )
    .await?;
    let id = agent(
        &io.client,
        "hidden",
        &[&minerva],
        agent_req("hidden").tool(agentic_tool(&[&minerva], &io.client).await?.0.id),
    )
    .await?;
    // amalthea gets a 404 and doesn't see it in her lists or tag listings
    fail!(amalthea.client.entities.get(id).await, 404);
    let listed = amalthea
        .client
        .entities
        .list(
            &EntityListOpts::default()
                .kind(EntityKinds::Agent)
                .limit(100),
        )
        .await?;
    assert!(listed.data.iter().all(|line| line.id != id));
    let tagged = amalthea
        .client
        .entities
        .list(
            &EntityListOpts::default()
                .groups(vec![mercury])
                .tag("submitter", &io.username)
                .limit(100),
        )
        .await?;
    assert!(tagged.data.iter().all(|line| line.id != id));
    // io sees it
    is!(io.client.entities.get(id).await?.id, id);
    Ok(())
}

/// Creating and deleting an agent's call edge to a tool never creates a tool instance
#[tokio::test]
async fn calls_edges_create_no_instances() -> Result<(), Error> {
    // a tool with one instance
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
    let (tool, instance) = agentic_tool(&[&minerva], &io.client).await?;
    let listed = |client: Thorium| async move {
        Ok::<Vec<Uuid>, Error>(
            client
                .entities
                .tool_instances(tool.id)
                .await?
                .into_iter()
                .map(|summary| summary.id)
                .collect(),
        )
    };
    let before = listed(admin.clone()).await?;
    is!(before.clone(), vec![instance.id]);
    // create an agent calling the tool, remove the edge, and add it back
    let id = agent(
        &io.client,
        "caller",
        &[&minerva],
        agent_req("caller").tool(tool.id),
    )
    .await?;
    is!(listed(admin.clone()).await?, before.clone());
    update_metadata(&io.client, id, edge_update(&[], &[tool.id], &[], &[])).await?;
    is!(listed(admin.clone()).await?, before.clone());
    update_metadata(&io.client, id, edge_update(&[tool.id], &[], &[], &[])).await?;
    io.client.entities.delete(id).await?;
    is!(listed(admin.clone()).await?, before);
    // the edges only queued edge changes, never a tool update
    let queued = changes(&admin, tool.id).await?;
    let updates = queued
        .iter()
        .filter(|queued| matches!(queued.change, WorkflowGraphChange::ToolUpdated { .. }))
        .count();
    is!(updates, 1);
    is!(queued.len(), 5);
    Ok(())
}

/// Every kind with a reference refuses a target that doesn't cover its groups, naming the
/// missing groups
#[tokio::test]
async fn reference_containment() -> Result<(), Error> {
    // minerva and mars, with an image tool instance, an endpoint, and a credential in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().owner(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (_, image) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_image_spec(),
        &io.client,
    )
    .await?;
    // a container backing pinning the image instance from both groups names mars
    let mut spec = generators::gen_agentic_spec();
    if let thorium::models::ToolSpec::Agentic(agentic) = &mut spec {
        agentic.backing =
            thorium::models::AgenticToolBacking::Container(thorium::models::ContainerToolBacking {
                tool_instance: image.id,
                ..thorium::models::ContainerToolBacking::default()
            });
        agentic.timeout = Some(60);
    }
    let resp = io
        .client
        .entities
        .create(generators::gen_tool(&[&minerva, &mars], "1.0.0", &spec))
        .await;
    fail!(resp, 400);
    assert!(refusal(resp).contains(&mars));
    // a profile serving through an endpoint in minerva alone names mars
    let mut entry = thorium::models::ModelCatalogEntryRequest::new("claude-c");
    entry.context_window = Some(200_000);
    let endpoint_req =
        thorium::models::ModelEndpointEntityRequest::new(generators::gen_anthropic_provider())
            .model(entry);
    let endpoint = generators::model_endpoint(&[&minerva], endpoint_req.clone(), &admin).await?;
    let profile = thorium::models::ModelProfileEntityRequest::new(
        endpoint.id,
        "claude-c",
        thorium::models::ModelApi::AnthropicMessages,
    );
    let resp = generators::model_profile(&[&minerva, &mars], profile, &admin).await;
    fail!(resp, 400);
    assert!(refusal(resp).contains(&mars));
    // an endpoint authenticating with a credential in minerva alone names mars
    let credential = generators::model_credential(
        &[&minerva],
        thorium::models::CredentialKind::ApiKey,
        Some("sk-agent-test-secret-value"),
        &admin,
    )
    .await?;
    let mut with_credential = endpoint_req;
    with_credential.credential = Some(credential.id);
    let resp = generators::model_endpoint(&[&minerva, &mars], with_credential, &admin).await;
    fail!(resp, 400);
    assert!(refusal(resp).contains(&mars));
    // a group change on the agent side is covered by the agent tests; the generic route refuses
    // owned kinds whatever the groups
    let target = AssociationTarget::Entity {
        id: endpoint.id,
        name: endpoint.name.clone(),
    };
    let req = AssociationRequest::new(AssociationKind::Provides, target.clone()).target(target);
    let resp = io.client.associations.create(&req).await;
    fail!(resp, 400);
    Ok(())
}

/// Removing a group from a sub-agent or tool an agent uses is blocked, and adding a group to an
/// agent needs its targets in that group
#[tokio::test]
async fn agent_group_changes() -> Result<(), Error> {
    // an agent in minerva calling a tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    let id = agent(
        &io.client,
        "grower",
        &[&minerva],
        agent_req("grower").tool(tool.id),
    )
    .await?;
    // the agent can't gain mars while its tool isn't in mars
    let resp = io
        .client
        .entities
        .update(id, EntityUpdate::default().group(&mars))
        .await;
    fail!(resp, 400);
    let msg = refusal(resp);
    assert!(msg.contains(&tool.name) && msg.contains(&mars), "{msg}");
    is!(stored_groups(id).await?, sorted_groups(&[&minerva]));
    // once the tool is in mars the agent gains mars and its edge follows
    io.client
        .entities
        .update(tool.id, EntityUpdate::default().group(&mars))
        .await?;
    io.client
        .entities
        .update(id, EntityUpdate::default().group(&mars))
        .await?;
    is!(stored_groups(id).await?, sorted_groups(&[&minerva, &mars]));
    // the tool can't lose mars while the agent in mars calls it
    let resp = io
        .client
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&mars))
        .await;
    fail!(resp, 409);
    assert!(refusal(resp).contains("grower"));
    Ok(())
}

/// Analysts who can edit a group add every agent edge kind through the form and the generic
/// route
#[tokio::test]
async fn analysts_add_edges() -> Result<(), Error> {
    // himalia is an analyst member of minerva with a tool, a sub-agent, and an agent there
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let himalia = generators::named_user_with_role(
        &admin,
        &generators::readable_name("himalia", &suffix),
        UserRole::Analyst,
    )
    .await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&himalia.username),
    )
    .await?;
    let (first, _) = agentic_tool(&[&minerva], &himalia.client).await?;
    let (second, _) = agentic_tool(&[&minerva], &himalia.client).await?;
    let sub = agent(&himalia.client, "sub", &[&minerva], agent_req("sub")).await?;
    let other = agent(&himalia.client, "other", &[&minerva], agent_req("other")).await?;
    // the form adds a call and a delegation
    let id = agent(
        &himalia.client,
        "analyzed",
        &[&minerva],
        agent_req("analyzed").tool(first.id).sub_agent(sub),
    )
    .await?;
    // the generic route adds another call and delegation
    let source = AssociationTarget::Entity {
        id,
        name: "analyzed".to_owned(),
    };
    for (kind, target, name) in [
        (AssociationKind::Calls, second.id, second.name.clone()),
        (AssociationKind::DelegatesTo, other, "other".to_owned()),
    ] {
        let req = AssociationRequest::new(kind, source.clone())
            .target(AssociationTarget::Entity { id: target, name });
        himalia.client.associations.create(&req).await?;
    }
    let read = himalia.client.entities.get(id).await?;
    is!(
        ids(&meta(&read).latest_tools),
        sorted(&[first.id, second.id])
    );
    is!(ids(&meta(&read).sub_agents), sorted(&[sub, other]));
    Ok(())
}

/// Adding an edge waits for its target's lock, widening a target waits for the target's lock,
/// and the generic route waits for its source agent's lock
#[tokio::test]
async fn edge_writes_wait_for_locks() -> Result<(), Error> {
    // two agents and a tool in minerva and mars, and a tool in minerva only
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let lead = agent(&io.client, "lead", &[&minerva, &mars], agent_req("lead")).await?;
    let helper = agent(
        &io.client,
        "helper",
        &[&minerva, &mars],
        agent_req("helper"),
    )
    .await?;
    let (wide, _) = agentic_tool(&[&minerva, &mars], &io.client).await?;
    let (narrow, _) = agentic_tool(&[&minerva], &io.client).await?;
    // an edge to a tool whose lock another request holds waits for it
    test_utilities::hold_instance_lock(wide.id).await?;
    let (added, released) = tokio::join!(
        timed(update_metadata(
            &io.client,
            lead,
            edge_update(&[wide.id], &[], &[], &[])
        )),
        release_later(wide.id, Duration::from_millis(1_000)),
    );
    released?;
    let waited = added?;
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    // widening a tool whose lock another request holds waits for it
    let widen = EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: vec![narrow.id],
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: true,
    };
    test_utilities::hold_instance_lock(narrow.id).await?;
    let (widened, released) = tokio::join!(
        timed(update_metadata(&io.client, lead, widen)),
        release_later(narrow.id, Duration::from_millis(1_000))
    );
    released?;
    let waited = widened?;
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    is!(
        stored_groups(narrow.id).await?,
        sorted_groups(&[&minerva, &mars])
    );
    // the generic route waits for the lock of the agent it adds an edge from
    let delegate = AssociationRequest::new(
        AssociationKind::DelegatesTo,
        AssociationTarget::Entity {
            id: lead,
            name: "lead".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: helper,
        name: "helper".to_owned(),
    });
    test_utilities::hold_instance_lock(lead).await?;
    let (delegated, released) = tokio::join!(
        timed(io.client.associations.create(&delegate)),
        release_later(lead, Duration::from_millis(1_500))
    );
    released?;
    let waited = delegated?;
    assert!(waited >= Duration::from_millis(1_400), "{waited:?}");
    let read = io.client.entities.get(lead).await?;
    is!(
        ids(&meta(&read).latest_tools),
        sorted(&[wide.id, narrow.id])
    );
    is!(ids(&meta(&read).sub_agents), vec![helper]);
    Ok(())
}

/// One update can add a group and drop the edge whose target isn't in it, and a removal field
/// only removes the kind of edge it is for
#[tokio::test]
async fn edge_removals_in_group_changes() -> Result<(), Error> {
    // an agent in minerva calling a tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    let id = agent(
        &io.client,
        "mover",
        &[&minerva],
        agent_req("mover").tool(tool.id),
    )
    .await?;
    // removing the tool through the toolset removal field is refused and keeps the edge
    let EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        clear_harness,
        add_tools,
        remove_tools,
        add_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups,
        ..
    } = agent_update()
    else {
        unreachable!()
    };
    let wrong_field = EntityMetadataUpdate::Agent {
        config,
        model_auth,
        harness,
        clear_harness,
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets: vec![tool.id],
        add_sub_agents,
        remove_sub_agents,
        extend_target_groups,
    };
    let resp = update_metadata(&io.client, id, wrong_field).await;
    fail!(resp, 400);
    assert!(refusal(resp).contains("remove_agent_tools"));
    is!(
        ids(&meta(&io.client.entities.get(id).await?).latest_tools),
        vec![tool.id]
    );
    // adding mars while removing the tool, which isn't in mars, is one valid update
    let moved =
        EntityUpdate::default()
            .group(&mars)
            .metadata(edge_update(&[], &[tool.id], &[], &[]));
    io.client.entities.update(id, moved).await?;
    let read = io.client.entities.get(id).await?;
    assert!(meta(&read).latest_tools.is_empty());
    is!(stored_groups(id).await?, sorted_groups(&[&minerva, &mars]));
    no_rows_naming(&admin, tool.id, id).await?;
    Ok(())
}

/// Run a request that may be refused and measure how long it took to finish
///
/// # Arguments
///
/// * `request` - The request to run
async fn timed_result<T>(
    request: impl std::future::Future<Output = Result<T, Error>>,
) -> (Result<T, Error>, Duration) {
    // time the request from when it's first polled
    let start = std::time::Instant::now();
    let result = request.await;
    (result, start.elapsed())
}

/// Get the status code of a refused request
///
/// # Arguments
///
/// * `result` - The request's result
fn refused_status<T>(result: &Result<T, Error>) -> Option<u16> {
    // a refusal carries its status code
    match result {
        Ok(_) => None,
        Err(err) => err.status().map(|status| status.as_u16()),
    }
}

/// Two agents delegating to each other at once finish quickly, with at most one waiting out
/// the other and every write that succeeds kept
#[tokio::test]
async fn cross_delegation_finishes_quickly() -> Result<(), Error> {
    // two agents in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let a = agent(&io.client, "a", &[&minerva], agent_req("agent_a")).await?;
    let b = agent(&io.client, "b", &[&minerva], agent_req("agent_b")).await?;
    // a delegates to b while b delegates to a
    let ((left, left_took), (right, right_took)) = tokio::join!(
        timed_result(update_metadata(
            &io.client,
            a,
            edge_update(&[], &[], &[b], &[])
        )),
        timed_result(update_metadata(
            &io.client,
            b,
            edge_update(&[], &[], &[a], &[])
        )),
    );
    // both finish well before a lock could expire
    assert!(left_took < Duration::from_secs(5), "{left_took:?}");
    assert!(right_took < Duration::from_secs(5), "{right_took:?}");
    // at least one succeeds and any refusal is a conflict
    assert!(left.is_ok() || right.is_ok(), "{left:?} {right:?}");
    for result in [&left, &right] {
        if result.is_err() {
            is!(refused_status(result), Some(409));
        }
    }
    // exactly the edges that were accepted are stored
    let read_a = io.client.entities.get(a).await?;
    let read_b = io.client.entities.get(b).await?;
    let expect_a = if left.is_ok() { vec![b] } else { Vec::new() };
    let expect_b = if right.is_ok() { vec![a] } else { Vec::new() };
    is!(ids(&meta(&read_a).sub_agents), expect_a);
    is!(ids(&meta(&read_b).sub_agents), expect_b);
    // neither agent's lock is left behind
    assert!(!test_utilities::instance_lock_exists(a).await?);
    assert!(!test_utilities::instance_lock_exists(b).await?);
    Ok(())
}

/// Add an edge from a lead agent to an agent whose lock another request holds for almost as
/// long as a lock lasts, and a second later edit the lead's config
///
/// Checks that the edge write gives up with a conflict, the config edit waits for it and then
/// succeeds, and no edge is stored. Returns how long the edge write took.
///
/// # Arguments
///
/// * `io` - The user writing the agents
/// * `group` - The group to create the agents in
/// * `label` - The label naming this pair of agents
/// * `lead_id` - The id to give the lead agent
/// * `held_id` - The id to give the agent whose lock is held
async fn edge_to_held_target(
    io: &TestUser,
    group: &str,
    label: &str,
    lead_id: Uuid,
    held_id: Uuid,
) -> Result<Duration, Error> {
    // the lead and held agents with the chosen ids
    let (lead_name, held_name) = (format!("{label}-lead"), format!("{label}-held"));
    test_utilities::faults::create_with_id(&lead_name, lead_id);
    test_utilities::faults::create_with_id(&held_name, held_id);
    let lead = agent(&io.client, &lead_name, &[group], agent_req("lead")).await?;
    let held = agent(&io.client, &held_name, &[group], agent_req("held")).await?;
    is!((lead, held), (lead_id, held_id));
    // another request holds the target's lock for almost as long as a lock lasts
    test_utilities::hold_instance_lock_for(held, "long-holder", 29_000).await?;
    // add an edge to the held target, and a second later edit the lead's config
    let edge = async {
        let (result, took) = timed_result(update_metadata(
            &io.client,
            lead,
            edge_update(&[], &[], &[held], &[]),
        ))
        .await;
        (result, took, std::time::Instant::now())
    };
    let config = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let edit = config_update(generators::gen_agent_config("lead_edit"));
        let result = update_metadata(&io.client, lead, edit).await;
        (result, std::time::Instant::now())
    };
    let ((edge_result, edge_took, edge_done), (config_result, config_done)) =
        tokio::join!(edge, config);
    // the edge write gives up with a conflict
    is!(refused_status(&edge_result), Some(409));
    // the config edit waited for the edge write to release the lead's lock, and then succeeded
    config_result?;
    assert!(config_done >= edge_done);
    // the lead has its new config and no edge to the held target
    let read = io.client.entities.get(lead).await?;
    is!(
        meta(&read).config.function_name.clone(),
        Some("lead_edit".to_owned())
    );
    assert!(meta(&read).sub_agents.is_empty());
    test_utilities::release_instance_lock(held).await?;
    Ok(edge_took)
}

/// An edge write waiting on a target lock that stays held gives up long before its own source
/// lock could expire: it waits out its whole budget for a target sorting after its source, and
/// gives way quickly for one sorting before it, while another write to the source waits for it
#[tokio::test]
async fn edge_lock_waits_stay_under_expiry() -> Result<(), Error> {
    // a user and a group for both pairs of agents
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    // one pair whose held target sorts after the lead, and one whose held target sorts before it
    let (after_low, after_high) = ordered_ids();
    let (before_low, before_high) = ordered_ids();
    let (after_label, before_label) = (format!("after-{suffix}"), format!("before-{suffix}"));
    let (after, before) = tokio::join!(
        edge_to_held_target(&io, &minerva, &after_label, after_low, after_high),
        edge_to_held_target(&io, &minerva, &before_label, before_high, before_low),
    );
    // a target sorting after the source is waited on for the whole budget, well under expiry
    let after = after?;
    assert!(after >= Duration::from_secs(9), "{after:?}");
    assert!(after < Duration::from_secs(15), "{after:?}");
    // a target sorting before the source is given way to quickly
    let before = before?;
    assert!(before < Duration::from_secs(4), "{before:?}");
    Ok(())
}

/// Creating an agent that calls a tool whose lock another request holds waits for the lock,
/// even when the tool sorts before the new agent
#[tokio::test]
async fn create_waits_for_target_lock() -> Result<(), Error> {
    // a tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    // the new agent sorts after the tool
    let name = format!("caller-{suffix}");
    let (_, high) = ordered_ids();
    test_utilities::faults::create_with_id(&name, high);
    // create the agent while another request holds the tool's lock for 3 seconds
    test_utilities::hold_instance_lock(tool.id).await?;
    let req = generators::gen_agent(&name, &[&minerva], agent_req("caller").tool(tool.id));
    let ((created, waited), released) = tokio::join!(
        timed_result(io.client.entities.create(req)),
        release_later(tool.id, Duration::from_secs(3)),
    );
    released?;
    // the create waited for the lock and then succeeded
    let created = created?;
    is!(created.id, high);
    assert!(waited >= Duration::from_millis(2_900), "{waited:?}");
    // the agent calls the tool and the tool's lock is free
    let read = io.client.entities.get(high).await?;
    is!(ids(&meta(&read).latest_tools), vec![tool.id]);
    assert!(!test_utilities::instance_lock_exists(tool.id).await?);
    Ok(())
}

/// A failed release of a source's or a target's lock after an edge is written answers as done
/// and keeps the edge
#[tokio::test]
async fn failed_unlock_keeps_written_edges() -> Result<(), Error> {
    // two agents and a tool in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let lead = agent(&io.client, "lead", &[&minerva], agent_req("lead")).await?;
    let helper = agent(&io.client, "helper", &[&minerva], agent_req("helper")).await?;
    let (tool, _) = agentic_tool(&[&minerva], &io.client).await?;
    // the release of the target tool's lock after the edge is written fails once
    test_utilities::faults::fail_after_unlock(tool.id);
    update_metadata(&io.client, lead, edge_update(&[tool.id], &[], &[], &[])).await?;
    // the release of the source agent's lock after the generic route writes its edge fails once
    test_utilities::faults::fail_after_unlock(lead);
    let delegate = AssociationRequest::new(
        AssociationKind::DelegatesTo,
        AssociationTarget::Entity {
            id: lead,
            name: "lead".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: helper,
        name: "helper".to_owned(),
    });
    io.client.associations.create(&delegate).await?;
    // both edges were kept
    let read = io.client.entities.get(lead).await?;
    is!(ids(&meta(&read).latest_tools), vec![tool.id]);
    is!(ids(&meta(&read).sub_agents), vec![helper]);
    Ok(())
}
