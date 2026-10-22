//! Agent checks, `config_hash`, and the agent's graph edges
//!
//! An agent's config is validated and hashed as one value. Its tools, toolsets, sub-agents, and
//! harness are graph edges that are checked together with the agent, before anything is written,
//! and filled back in from those edges when the agent is read.

use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use tracing::instrument;
use uuid::Uuid;

use super::tools;
use crate::models::backends::associations::{
    self as edges, CheckedEdge, EdgeField, EdgePlan, EdgeRequest,
};
use crate::models::backends::db;
use crate::models::entities::models::parse_json_field;
use crate::models::{
    AgentConfig, AgentEntity, AssociationKind, AssociationListOpts, AssociationTargetColumn,
    Directionality, Entity, EntityForm, EntityKinds, EntityMetadata, EntityMetadataUpdateForm,
    EntityResponse, ListableAssociation, ModelAuthSource, User,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, conflict, internal_err};

/// The field naming the tools an agent calls
const TOOLS_FIELD: EdgeField = EdgeField {
    name: "agent_tools",
    kinds: &[EntityKinds::Tool, EntityKinds::ToolInstance],
    describe: "a tool or tool instance",
};

/// The field naming the toolsets an agent calls
const TOOL_SETS_FIELD: EdgeField = EdgeField {
    name: "agent_tool_sets",
    kinds: &[EntityKinds::ToolSet, EntityKinds::ToolSetInstance],
    describe: "a toolset or toolset instance",
};

/// The field naming the agents an agent delegates to
const SUB_AGENTS_FIELD: EdgeField = EdgeField {
    name: "agent_sub_agents",
    kinds: &[EntityKinds::Agent],
    describe: "an agent",
};

/// The field naming the harness an agent runs in
const HARNESS_FIELD: EdgeField = EdgeField {
    name: "agent_harness",
    kinds: &[EntityKinds::Tool, EntityKinds::ToolInstance],
    describe: "a harness tool or harness tool instance",
};

/// The update field naming the tools to add to an agent
const ADD_TOOLS_FIELD: EdgeField = EdgeField {
    name: "add_agent_tools",
    ..TOOLS_FIELD
};

/// The update field naming the toolsets to add to an agent
const ADD_TOOL_SETS_FIELD: EdgeField = EdgeField {
    name: "add_agent_tool_sets",
    ..TOOL_SETS_FIELD
};

/// The update field naming the agents to add as an agent's sub-agents
const ADD_SUB_AGENTS_FIELD: EdgeField = EdgeField {
    name: "add_agent_sub_agents",
    ..SUB_AGENTS_FIELD
};

/// The fields an agent's edges come from on a create
const CREATE_FIELDS: [EdgeField; 4] = [
    HARNESS_FIELD,
    TOOLS_FIELD,
    TOOL_SETS_FIELD,
    SUB_AGENTS_FIELD,
];

/// The fields an agent's added edges come from on an update
const UPDATE_FIELDS: [EdgeField; 4] = [
    HARNESS_FIELD,
    ADD_TOOLS_FIELD,
    ADD_TOOL_SETS_FIELD,
    ADD_SUB_AGENTS_FIELD,
];

/// The edge kinds an agent's own edges use
const AGENT_EDGES: [AssociationKind; 3] = [
    AssociationKind::Calls,
    AssociationKind::DelegatesTo,
    AssociationKind::RunsIn,
];

/// Parse and check an agent's config field
///
/// # Arguments
///
/// * `raw` - The JSON encoded config
fn parse_config(raw: &str) -> Result<AgentConfig, ApiError> {
    // parse the whole config as one typed value
    let config: AgentConfig = parse_json_field("agent_config", raw)?;
    // check the fields a request can get wrong
    if let Err(msg) = config.validate() {
        return bad!(msg);
    }
    Ok(config)
}

/// Hash an agent's config into its `config_hash`
///
/// # Arguments
///
/// * `config` - The config to hash
fn hash_config(config: &AgentConfig) -> Result<String, ApiError> {
    // the config always digests as an object
    match config.config_hash() {
        Ok(hash) => Ok(hash),
        Err(err) => internal_err!(format!("failed to hash an agent config: {err}")),
    }
}

/// Build the edge requests of an agent's edge fields
///
/// # Arguments
///
/// * `fields` - The harness, tools, toolsets, and sub-agents fields, in that order
/// * `harness` - The harness to run in
/// * `tools` - The tools and tool instances to call
/// * `tool_sets` - The toolsets and toolset instances to call
/// * `sub_agents` - The agents to delegate to
fn edge_requests(
    fields: [EdgeField; 4],
    harness: Option<Uuid>,
    tools: &[Uuid],
    tool_sets: &[Uuid],
    sub_agents: &[Uuid],
) -> Vec<EdgeRequest> {
    // build one request per named target, tagged with its field
    let request = |kind, field, target| EdgeRequest {
        kind,
        target,
        field: Some(field),
    };
    let mut requested = Vec::with_capacity(tools.len() + tool_sets.len() + sub_agents.len() + 1);
    requested.extend(
        harness
            .into_iter()
            .map(|id| request(AssociationKind::RunsIn, fields[0], id)),
    );
    requested.extend(
        tools
            .iter()
            .map(|id| request(AssociationKind::Calls, fields[1], *id)),
    );
    requested.extend(
        tool_sets
            .iter()
            .map(|id| request(AssociationKind::Calls, fields[2], *id)),
    );
    requested.extend(
        sub_agents
            .iter()
            .map(|id| request(AssociationKind::DelegatesTo, fields[3], *id)),
    );
    requested
}

/// Get the root a tool, toolset, or one of their instances belongs to, and whether it's pinned
///
/// # Arguments
///
/// * `entity` - The tool, toolset, or instance
fn edge_root(entity: &Entity) -> Option<(Uuid, bool)> {
    // a root is followed at latest, and an instance pins its root
    match &entity.metadata {
        EntityMetadata::Tool(_) | EntityMetadata::ToolSet(_) => Some((entity.id, false)),
        EntityMetadata::ToolInstance(instance) => Some((instance.tool, true)),
        EntityMetadata::ToolSetInstance(instance) => Some((instance.tool_set, true)),
        _ => None,
    }
}

/// Check that a source never reaches a tool or toolset both at latest and pinned to one of its
/// instances
///
/// # Arguments
///
/// * `source` - The source's kind and name, such as ``agent `triage` ``
/// * `kind` - The kind of edge being checked
/// * `kept` - The targets the source keeps after the request's removals
/// * `added` - The edges being added
pub(crate) fn check_latest_and_pinned(
    source: &str,
    kind: AssociationKind,
    kept: &[&Entity],
    added: &[CheckedEdge],
) -> Result<(), ApiError> {
    // collect every target the source will reach with this kind of edge, and how
    let added_targets = added
        .iter()
        .filter(|edge| edge.kind == kind)
        .map(|edge| &edge.target);
    let mut calls: HashMap<Uuid, (bool, bool, String)> = HashMap::new();
    for target in kept.iter().copied().chain(added_targets) {
        if let Some((root, pinned)) = edge_root(target) {
            // record whether this tool is followed at latest, pinned, or both
            let entry = calls
                .entry(root)
                .or_insert_with(|| (false, false, target.name.clone()));
            if pinned {
                entry.1 = true;
            } else {
                entry.0 = true;
                entry.2.clone_from(&target.name);
            }
        }
    }
    // a target reached both ways is refused
    if let Some((_, _, name)) = calls
        .values()
        .find(|(latest, pinned, _)| *latest && *pinned)
    {
        let verb = if kind == AssociationKind::Includes {
            "include"
        } else {
            "call"
        };
        return bad!(format!(
            "{source} can't {verb} `{name}` at its latest version and pinned to one of its instances at once"
        ));
    }
    Ok(())
}

/// Get an agent's metadata from its entity
///
/// # Arguments
///
/// * `entity` - The agent entity
fn agent_meta(entity: &Entity) -> Result<&AgentEntity, ApiError> {
    // only agents have agent metadata
    match &entity.metadata {
        EntityMetadata::Agent(agent) => Ok(agent),
        _ => internal_err!(format!("{} is not an agent", entity.id)),
    }
}

/// Create an agent and its edges from a submitted form
///
/// The config and every edge are checked before anything is written; the agent's rows are
/// written before its edges, so a failed write only ever leaves an agent without edges behind.
///
/// # Arguments
///
/// * `user` - The user creating the agent
/// * `id` - The id of the new agent
/// * `form` - The submitted form
/// * `shared` - Shared Thorium objects
#[instrument(name = "agents::create", skip_all, fields(id = id.to_string()), err(Debug))]
pub(crate) async fn create(
    user: &User,
    id: Uuid,
    form: EntityForm,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // check the name and groups
    let name = tools::check_name_and_groups(form.name.as_ref(), &form.groups)?;
    // parse, check, and hash the config
    let Some(raw) = form.metadata.agent_config.as_deref() else {
        return bad!("an agent needs an `agent_config`".to_owned());
    };
    let config = parse_config(raw)?;
    let config_hash = hash_config(&config)?;
    // parse the model auth levels, which aren't part of the config
    let model_auth = match form.metadata.agent_model_auth.as_deref() {
        Some(raw) => parse_json_field::<ModelAuthSource>("agent_model_auth", raw)?,
        None => ModelAuthSource::default(),
    };
    // build the agent
    let now = Utc::now();
    let entity = Entity {
        id,
        name: name.clone(),
        kind: EntityKinds::Agent,
        metadata: EntityMetadata::Agent(AgentEntity {
            config,
            config_hash,
            model_auth,
            modified: now,
            ..AgentEntity::default()
        }),
        description: form.description,
        submitter: user.username.clone(),
        groups: form.groups,
        tags: HashMap::default(),
        image: form.image,
        created: now,
    };
    // check every edge before anything is written
    let requested = edge_requests(
        CREATE_FIELDS,
        form.metadata.agent_harness,
        &form.metadata.agent_tools,
        &form.metadata.agent_tool_sets,
        &form.metadata.agent_sub_agents,
    );
    let extend = form.metadata.extend_target_groups == Some(true);
    let mut plan = helpers::heap_future(|| {
        edges::check_graph_edges(user, &entity, false, &requested, extend, shared)
    })
    .await?;
    let checked = check_latest_and_pinned(
        &format!("agent `{name}`"),
        AssociationKind::Calls,
        &[],
        &plan.edges,
    )
    .and_then(|()| edges::check_edge_caps(&entity, &HashSet::new(), &plan, shared));
    plan.release_on_error(checked, shared).await?;
    // write the agent, then its edges
    let inserted = db::entities::insert(user, &entity, form.tags, shared).await;
    plan.release_on_error(inserted, shared).await?;
    helpers::heap_future(|| edges::write_graph_edges(user, &entity, plan, shared)).await?;
    Ok(EntityResponse::new(id, name))
}

/// Apply the config and model auth fields of an update to an agent
///
/// Setting a config replaces the whole config and recomputes `config_hash`; the model auth
/// levels aren't hashed. Every update marks the agent modified.
///
/// # Arguments
///
/// * `agent` - The agent's metadata
/// * `form` - The metadata update form
pub(crate) fn update_meta(
    agent: &mut AgentEntity,
    form: &mut EntityMetadataUpdateForm,
) -> Result<(), ApiError> {
    // replace the whole config and its hash
    if let Some(raw) = form.agent_config.take() {
        let config = parse_config(&raw)?;
        agent.config_hash = hash_config(&config)?;
        agent.config = config;
    }
    // replace the model auth levels
    if let Some(raw) = form.agent_model_auth.take() {
        agent.model_auth = parse_json_field("agent_model_auth", &raw)?;
    }
    // any edit marks the agent modified
    agent.modified = Utc::now();
    Ok(())
}

/// List an agent's outgoing edges of the agent edge kinds
///
/// # Arguments
///
/// * `user` - The user reading the edges
/// * `agent` - The agent
/// * `groups` - The groups to read the edges in
/// * `shared` - Shared Thorium objects
async fn outgoing(
    user: &User,
    agent: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<ListableAssociation>, ApiError> {
    // list the agent's edges of the agent edge kinds
    edges_of(
        user,
        agent,
        groups,
        &AGENT_EDGES,
        Directionality::To,
        shared,
    )
    .await
}

/// List an entity's edges of some kinds in one direction
///
/// # Arguments
///
/// * `user` - The user reading the edges
/// * `entity` - The entity whose edges to list
/// * `groups` - The groups to read the edges in
/// * `kinds` - The kinds of edge to keep
/// * `direction` - The direction of edge to keep, from the entity's side
/// * `shared` - Shared Thorium objects
pub(crate) async fn edges_of(
    user: &User,
    entity: Uuid,
    groups: &[String],
    kinds: &[AssociationKind],
    direction: Directionality,
    shared: &Shared,
) -> Result<Vec<ListableAssociation>, ApiError> {
    // list the entity's associations in these groups
    let source = AssociationTargetColumn::Entity(entity);
    let opts = AssociationListOpts::default()
        .groups(groups.to_vec())
        .limit(10_000);
    let mut cursor = db::associations::list(user, opts, &source, shared).await?;
    // keep the edges of these kinds in this direction from every page
    let mut found = Vec::new();
    loop {
        found.extend(
            cursor
                .data
                .drain(..)
                .filter(|assoc| assoc.direction == direction && kinds.contains(&assoc.kind)),
        );
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        // get the next page of edges
        cursor.next(shared).await?;
    }
    Ok(found)
}

/// Get the entity id at the other end of an edge, if it's an entity
///
/// # Arguments
///
/// * `assoc` - The edge
pub(crate) fn other_entity(assoc: &ListableAssociation) -> Result<Option<Uuid>, ApiError> {
    // only entity ends have an id
    match assoc.deserialize_other()? {
        AssociationTargetColumn::Entity(id) => Ok(Some(id)),
        _ => Ok(None),
    }
}

/// Fill an agent's harness, tools, toolsets, and sub-agents from its outgoing edges
///
/// The targets are read in the agent's visible groups, which they cover.
///
/// # Arguments
///
/// * `user` - The user reading the agent
/// * `entity` - The agent
/// * `shared` - Shared Thorium objects
#[instrument(name = "agents::populate", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn populate(
    user: &User,
    mut entity: Entity,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // collect each edge's kind by its target
    let mut kinds: BTreeMap<Uuid, HashSet<AssociationKind>> = BTreeMap::new();
    for assoc in outgoing(user, entity.id, &entity.groups, shared).await? {
        if let Some(id) = other_entity(&assoc)? {
            kinds.entry(id).or_default().insert(assoc.kind);
        }
    }
    // read every target at once
    let ids = kinds.keys().copied().collect::<Vec<Uuid>>();
    let targets = if ids.is_empty() {
        Vec::new()
    } else {
        db::entities::get_many(&entity.groups, &ids, shared).await?
    };
    // put each target in the field its edge fills
    if let EntityMetadata::Agent(agent) = &mut entity.metadata {
        agent.clear_edges();
        for target in targets {
            let Some(edge_kinds) = kinds.get(&target.id) else {
                continue;
            };
            for kind in edge_kinds {
                match (kind, target.kind) {
                    (AssociationKind::RunsIn, _) => {
                        agent.harness = Some(Box::new(target.clone()));
                    }
                    (AssociationKind::DelegatesTo, _) => agent.sub_agents.push(target.clone()),
                    (AssociationKind::Calls, EntityKinds::Tool) => {
                        agent.latest_tools.push(target.clone());
                    }
                    (AssociationKind::Calls, EntityKinds::ToolInstance) => {
                        agent.pinned_tools.push(target.clone());
                    }
                    (AssociationKind::Calls, _) => agent.tool_sets.push(target.clone()),
                    _ => (),
                }
            }
        }
    }
    Ok(entity)
}

/// Check the edges an update adds and removes, then write them
///
/// Every added edge is checked before any edge is added or removed, so one bad edge rejects
/// the whole update. Removing an edge the agent doesn't have changes nothing, and adding one it
/// already has adds nothing.
///
/// # Arguments
///
/// * `user` - The user updating the agent
/// * `entity` - The agent, with its edges filled in
/// * `form` - The metadata update form
/// * `shared` - Shared Thorium objects
#[instrument(name = "agents::update_edges", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn update_edges(
    user: &User,
    entity: &mut Entity,
    form: &mut EntityMetadataUpdateForm,
    shared: &Shared,
) -> Result<(), ApiError> {
    // collect the edges to remove by their kind and target
    let clear_harness = form.clear_agent_harness == Some(true);
    let removes = removed_edges(form)
        .into_iter()
        .collect::<HashSet<(AssociationKind, Uuid)>>();
    // collect the edges to add
    let requested = edge_requests(
        UPDATE_FIELDS,
        form.agent_harness,
        &form.add_agent_tools,
        &form.add_agent_tool_sets,
        &form.add_agent_sub_agents,
    );
    // nothing changes without edge fields
    if requested.is_empty() && removes.is_empty() && !clear_harness {
        return Ok(());
    }
    // a removal field only removes the kind of edge it's for
    let agent = agent_meta(entity)?;
    check_removal_fields(agent, form)?;
    // an agent's harness is only replaced after clearing it
    if let (Some(current), Some(_), false) = (&agent.harness, form.agent_harness, clear_harness) {
        return bad!(format!(
            "agent `{}` already runs in harness `{}`; clear it in the same update to replace it",
            entity.name, current.name
        ));
    }
    // the tools and toolsets the agent keeps calling after the removals
    let kept = agent
        .latest_tools
        .iter()
        .chain(&agent.pinned_tools)
        .chain(&agent.tool_sets)
        .filter(|target| !removes.contains(&(AssociationKind::Calls, target.id)))
        .collect::<Vec<&Entity>>();
    // the edges the agent has now, so adding one again adds nothing
    let mut existing: HashSet<(AssociationKind, Uuid)> = HashSet::new();
    existing.extend(
        agent
            .latest_tools
            .iter()
            .chain(&agent.pinned_tools)
            .chain(&agent.tool_sets)
            .map(|target| (AssociationKind::Calls, target.id)),
    );
    existing.extend(
        agent
            .sub_agents
            .iter()
            .map(|target| (AssociationKind::DelegatesTo, target.id)),
    );
    // check every added edge before anything is written
    let extend = form.extend_target_groups == Some(true);
    let mut plan = helpers::heap_future(|| {
        edges::check_graph_edges(user, entity, true, &requested, extend, shared)
    })
    .await?;
    let checked = check_latest_and_pinned(
        &format!("agent `{}`", entity.name),
        AssociationKind::Calls,
        &kept,
        &plan.edges,
    );
    plan.release_on_error(checked, shared).await?;
    plan.edges.retain(|edge| {
        !existing.contains(&(edge.kind, edge.target.id))
            || removes.contains(&(edge.kind, edge.target.id))
    });
    // the agent's edges after the update must fit what a resolution reads
    let mut capped = removes.clone();
    if clear_harness && let Some(harness) = &agent.harness {
        capped.insert((AssociationKind::RunsIn, harness.id));
    }
    let checked = edges::check_edge_caps(entity, &capped, &plan, shared);
    plan.release_on_error(checked, shared).await?;
    // remove the edges named for removal, then add the new ones
    let dropped =
        helpers::heap_future(|| remove_edges(user, entity, &removes, clear_harness, shared)).await;
    plan.release_on_error(dropped, shared).await?;
    helpers::heap_future(|| write_plan(user, entity, plan, shared)).await
}

/// Remove an agent's stored edges named for removal
///
/// # Arguments
///
/// * `user` - The user updating the agent
/// * `entity` - The agent
/// * `removes` - The kind and target of each edge to remove
/// * `clear_harness` - Whether the agent's harness edge is removed too
/// * `shared` - Shared Thorium objects
async fn remove_edges(
    user: &User,
    entity: &Entity,
    removes: &HashSet<(AssociationKind, Uuid)>,
    clear_harness: bool,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find the stored rows of the edges to remove
    let rows = outgoing(user, entity.id, &entity.groups, shared)
        .await?
        .into_iter()
        .filter_map(|assoc| match other_entity(&assoc) {
            Ok(Some(id)) => {
                let dropping = removes.contains(&(assoc.kind, id))
                    || (clear_harness && assoc.kind == AssociationKind::RunsIn);
                dropping.then_some(Ok(assoc))
            }
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<Vec<ListableAssociation>, ApiError>>()?;
    // remove the edges and queue a change for each
    if !rows.is_empty() {
        let source = AssociationTargetColumn::Entity(entity.id);
        db::associations::delete_many(&source, &rows, shared).await?;
        edges::queue_removed_edges(user, entity.id, &rows, shared).await?;
    }
    Ok(())
}

/// Make sure each tool or toolset removal field names only targets of its own kind
///
/// Both fields remove `Calls` edges, so naming a toolset in the tool field, or a tool in the
/// toolset field, is refused rather than removing an edge the field isn't for.
///
/// # Arguments
///
/// * `agent` - The agent, with its edges filled in
/// * `form` - The metadata update form
fn check_removal_fields(
    agent: &AgentEntity,
    form: &EntityMetadataUpdateForm,
) -> Result<(), ApiError> {
    // a toolset named in the tool removal field is refused
    if let Some(target) = agent
        .tool_sets
        .iter()
        .find(|target| form.remove_agent_tools.contains(&target.id))
    {
        return bad!(format!(
            "`{}` is a {}; remove it with remove_agent_tool_sets",
            target.name, target.kind
        ));
    }
    // a tool named in the toolset removal field is refused
    if let Some(target) = agent
        .latest_tools
        .iter()
        .chain(&agent.pinned_tools)
        .find(|target| form.remove_agent_tool_sets.contains(&target.id))
    {
        return bad!(format!(
            "`{}` is a {}; remove it with remove_agent_tools",
            target.name, target.kind
        ));
    }
    Ok(())
}

/// The edges an update of an agent removes, by kind and target
///
/// # Arguments
///
/// * `form` - The metadata update form
pub(crate) fn removed_edges(form: &EntityMetadataUpdateForm) -> Vec<(AssociationKind, Uuid)> {
    // tools and toolsets are called, and sub-agents are delegated to
    form.remove_agent_tools
        .iter()
        .chain(&form.remove_agent_tool_sets)
        .map(|id| (AssociationKind::Calls, *id))
        .chain(
            form.remove_agent_sub_agents
                .iter()
                .map(|id| (AssociationKind::DelegatesTo, *id)),
        )
        .collect()
}

/// Write a checked edge plan from an agent
///
/// # Arguments
///
/// * `user` - The user adding the edges
/// * `entity` - The agent
/// * `plan` - The checked plan
/// * `shared` - Shared Thorium objects
async fn write_plan(
    user: &User,
    entity: &Entity,
    mut plan: EdgePlan,
    shared: &Shared,
) -> Result<(), ApiError> {
    // an empty plan writes nothing but still releases its locks
    if plan.is_empty() {
        plan.locks.release(shared).await;
        return Ok(());
    }
    // write the widenings and then the edges
    helpers::heap_future(|| edges::write_graph_edges(user, entity, plan, shared)).await
}

/// Check the tool and toolset edges the generic association route adds to an agent or toolset
///
/// An agent can't call a tool or toolset both at latest and pinned to one of its instances, and
/// a toolset can't include a tool both ways.
///
/// # Arguments
///
/// * `user` - The user adding the edges
/// * `source` - The edges' source
/// * `plan` - The checked edges
/// * `shared` - Shared Thorium objects
pub(crate) async fn check_new_calls(
    user: &User,
    source: &Entity,
    plan: &EdgePlan,
    shared: &Shared,
) -> Result<(), ApiError> {
    // only an agent's calls and a toolset's includes can conflict
    let (kind, label) = match source.kind {
        EntityKinds::Agent => (AssociationKind::Calls, "agent"),
        EntityKinds::ToolSet => (AssociationKind::Includes, "toolset"),
        _ => return Ok(()),
    };
    if plan.edges.iter().all(|edge| edge.kind != kind) {
        return Ok(());
    }
    // read what the source reaches now
    let current =
        helpers::heap_future(|| Entity::get_populated(user, source.clone(), shared)).await?;
    let kept = match &current.metadata {
        EntityMetadata::Agent(agent) => agent
            .latest_tools
            .iter()
            .chain(&agent.pinned_tools)
            .chain(&agent.tool_sets)
            .collect::<Vec<&Entity>>(),
        EntityMetadata::ToolSet(tool_set) => tool_set.tools.iter().collect::<Vec<&Entity>>(),
        _ => Vec::new(),
    };
    check_latest_and_pinned(
        &format!("{label} `{}`", source.name),
        kind,
        &kept,
        &plan.edges,
    )
}

/// Check a group change of an agent
///
/// Every target of the agent's edges must be in each added group, so its edges stay contained,
/// and a group can't be removed while an edge from an entity in that group points at the agent.
/// Edges the same update removes don't need to be contained.
///
/// # Arguments
///
/// * `user` - The user changing the agent's groups
/// * `agent` - The agent, in all of its groups
/// * `add` - The groups being added
/// * `remove` - The groups being removed
/// * `dropped` - The edges the same update removes, by kind and target
/// * `shared` - Shared Thorium objects
#[instrument(name = "agents::check_group_change", skip_all, fields(id = agent.id.to_string()), err(Debug))]
pub(crate) async fn check_group_change(
    user: &User,
    agent: &Entity,
    add: &[String],
    remove: &[String],
    dropped: &[(AssociationKind, Uuid)],
    shared: &Shared,
) -> Result<(), ApiError> {
    // the agent's own edges must stay contained, and edges to it block a removal
    let label = format!("agent `{}`", agent.name);
    let change = GroupChange {
        entity: agent,
        out_kinds: &AGENT_EDGES,
        referenced: &[agent.id],
        label: &label,
        add,
        remove,
        dropped,
    };
    check_contained_group_change(user, &change, shared).await
}

/// A group change of an entity whose graph edges must stay contained
pub(crate) struct GroupChange<'a> {
    /// The entity changing groups, in all of its groups
    pub entity: &'a Entity,
    /// The kinds of the entity's own outgoing edges, whose targets must be in each added group
    pub out_kinds: &'a [AssociationKind],
    /// The entity and its instances, which graph edges from a removed group must not reference
    pub referenced: &'a [Uuid],
    /// The entity's kind and name, for errors
    pub label: &'a str,
    /// The groups being added
    pub add: &'a [String],
    /// The groups being removed
    pub remove: &'a [String],
    /// The entity's own edges the same update removes, by kind and target, which don't need to
    /// be in each added group
    pub dropped: &'a [(AssociationKind, Uuid)],
}

/// Check a group change of an entity whose graph edges must stay contained
///
/// Every target of the entity's own edges must be in each added group, and a group can't be
/// removed while a graph edge from another entity in that group references the entity or one of
/// its instances.
///
/// # Arguments
///
/// * `user` - The user changing the groups
/// * `change` - The group change to check
/// * `shared` - Shared Thorium objects
pub(crate) async fn check_contained_group_change(
    user: &User,
    change: &GroupChange<'_>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the parts of the change
    let (agent, add, remove) = (change.entity, change.add, change.remove);
    // every target of the entity's edges must be in each added group
    if !add.is_empty() {
        let groups = db::entities::all_groups(agent.id, shared).await?;
        let found = edges_of(
            user,
            agent.id,
            &groups,
            change.out_kinds,
            Directionality::To,
            shared,
        )
        .await?;
        for assoc in found {
            let Some(id) = other_entity(&assoc)? else {
                continue;
            };
            // the entity's own edges to itself follow it, and edges being removed don't matter
            if id == agent.id || change.dropped.contains(&(assoc.kind, id)) {
                continue;
            }
            let target_groups = db::entities::all_groups(id, shared).await?;
            let missing = helpers::missing_groups(add, &target_groups);
            if !missing.is_empty() {
                // name only the missing groups this user can see
                let visible = tools::visible_groups(user, &missing)?;
                let name = assoc.extra_other.clone().unwrap_or_else(|| id.to_string());
                return bad!(format!(
                    "`{name}` isn't in groups {visible:?}; add those groups to it first"
                ));
            }
        }
    }
    // a group can't be removed while another entity in it points at the entity
    if !remove.is_empty() {
        let referrers =
            db::associations::referrers_in_groups(user, change.referenced, remove, shared).await?;
        let mut blocked = false;
        let mut visible: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (group, refs) in referrers {
            for referrer in refs {
                // only graph edges from other entities block a removal
                let from_self = match &referrer.source {
                    AssociationTargetColumn::Entity(id) => change.referenced.contains(id),
                    _ => false,
                };
                if !referrer.kind.is_graph_edge() || from_self {
                    continue;
                }
                blocked = true;
                // only name referrers in groups this user can see
                if user.is_admin() || user.groups.contains(&group) {
                    let described =
                        tools::describe_referrer(&referrer.source, &group, shared).await;
                    visible.entry(group.clone()).or_default().insert(described);
                }
            }
        }
        if blocked {
            return conflict!(tools::removal_message(change.label, visible));
        }
    }
    Ok(())
}
