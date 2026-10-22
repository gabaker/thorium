//! Workflow catalogs, their `Orchestrates` edges, and submitted workflow instances
//!
//! A workflow catalog and the agents it orchestrates are checked together before anything is
//! written, and the agents are filled back in from its edges when it's read. A workflow
//! instance is never built here: a submitted one is validated (permissions, references, stored
//! hashes, snapshot groups, digest, and revision) and stored as it was sent, under its
//! workflow's lock so two submissions of one revision can't both succeed. The snapshot group
//! checks are shared with submitted toolset instances.

use chrono::{SubsecRound, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;
use tracing::instrument;
use uuid::Uuid;

use super::agents::{self, GroupChange};
use super::tools;
use crate::models::backends::associations::{self as edges, EdgeField, EdgeRequest};
use crate::models::backends::db;
use crate::models::entities::models::parse_json_field;
use crate::models::entities::workflows::{
    FOLLOWS_TOOL_SET_TAG, FOLLOWS_TOOL_TAG, INSTANCE_AGENT_TAG, WORKFLOW_ID_TAG,
};
use crate::models::{
    AssociationKind, AssociationTarget, AssociationTargetColumn, BindingCheck, CrawlLimits,
    Directionality, Entity, EntityForm, EntityKinds, EntityMetadata, EntityMetadataUpdateForm,
    EntityResponse, EntityUpdateForm, Event, Group, GroupAllowAction, InstancedAgentToolset,
    InstancedToolSpec, ListableAssociation, TagMap, ToolSpec, User, WorkflowCatalogEntity,
    WorkflowCatalogEntityRequest, WorkflowGraphChange, WorkflowInstanceEntity,
    WorkflowInstanceEntityRequest,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, conflict, internal_err, not_found, tag, update};

/// Add a workflow instance's intrinsic tags
///
/// The workflow, revision, digest, status, and each agent come from the instance itself. The
/// tool and toolset roots its agents follow at latest are read from their edges when the
/// instance is submitted and stored as tags, so the ones it already carries are kept.
///
/// # Arguments
///
/// * `instance` - The workflow instance
/// * `stored` - The tags the instance already carries
/// * `tags` - The tags to add to
// tag maps use the default hasher everywhere entities are tagged
#[allow(clippy::implicit_hasher)]
pub(crate) fn intrinsic_tags(
    instance: &WorkflowInstanceEntity,
    stored: &TagMap,
    tags: &mut HashMap<String, HashSet<String>>,
) {
    // tag the workflow, revision, digest, and status
    tag!(tags, WORKFLOW_ID_TAG, instance.workflow.to_string());
    tag!(tags, "WorkflowRevision", instance.revision.to_string());
    tag!(tags, "WorkflowDigest", instance.digest.clone());
    tag!(tags, "WorkflowStatus", instance.status.as_str().to_owned());
    // tag every agent, so an edit to any of them finds the workflow
    for agent in &instance.agents {
        tag!(tags, INSTANCE_AGENT_TAG, agent.agent.to_string());
    }
    // keep the followed roots the instance was tagged with when it was submitted
    for key in [FOLLOWS_TOOL_TAG, FOLLOWS_TOOL_SET_TAG] {
        if let Some(values) = stored.get(key) {
            for value in values.keys() {
                tag!(tags, key, value.clone());
            }
        }
    }
}

/// The tag keys Thorium sets on every workflow instance, which its users can't set themselves
pub(crate) const INTRINSIC_TAG_KEYS: [&str; 7] = [
    "WorkflowId",
    "WorkflowRevision",
    "WorkflowDigest",
    "WorkflowStatus",
    "InstanceAgent",
    "InstanceFollowsTool",
    "InstanceFollowsToolSet",
];

/// How long a workflow instance submission waits for another write to its workflow to finish
const INSTANCE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The field naming the agents a new workflow orchestrates
const AGENTS_FIELD: EdgeField = EdgeField {
    name: "workflow_agents",
    kinds: &[EntityKinds::Agent],
    describe: "an agent",
};

/// The update field naming the agents to add to a workflow
const ADD_AGENTS_FIELD: EdgeField = EdgeField {
    name: "add_workflow_agents",
    ..AGENTS_FIELD
};

/// The edge kinds a workflow's own edges use
const WORKFLOW_EDGES: [AssociationKind; 1] = [AssociationKind::Orchestrates];

/// The edge kinds through which an agent follows a tool or toolset at latest
const FOLLOW_EDGES: [AssociationKind; 2] = [AssociationKind::Calls, AssociationKind::RunsIn];

/// Build the `Orchestrates` edge requests of a workflow's agent field
///
/// # Arguments
///
/// * `field` - The field the agents came from
/// * `agents` - The agents to orchestrate
fn orchestrate_requests(field: EdgeField, agents: &[Uuid]) -> Vec<EdgeRequest> {
    // one request per named agent, tagged with its field
    agents
        .iter()
        .map(|id| EdgeRequest {
            kind: AssociationKind::Orchestrates,
            target: *id,
            field: Some(field),
        })
        .collect()
}

/// Get a workflow's metadata from its entity
///
/// # Arguments
///
/// * `entity` - The workflow entity
fn workflow_meta(entity: &Entity) -> Result<&WorkflowCatalogEntity, ApiError> {
    // only workflow catalogs have workflow metadata
    match &entity.metadata {
        EntityMetadata::WorkflowCatalog(workflow) => Ok(workflow),
        _ => internal_err!(format!("{} is not a workflow catalog", entity.id)),
    }
}

/// Refuse an entry agent that isn't one of a workflow's agents
///
/// # Arguments
///
/// * `entry` - The entry agent
/// * `workflow` - The name of the workflow
fn entry_refusal<T>(entry: Uuid, workflow: &str) -> Result<T, ApiError> {
    // name the agent and the workflow
    bad!(format!(
        "entry agent {entry} is not one of workflow {workflow}'s agents"
    ))
}

/// Create a workflow and its `Orchestrates` edges from a submitted form
///
/// The settings, the entry agent, and every edge are checked before anything is written; the
/// workflow's rows are written before its edges, so a failed write only ever leaves a workflow
/// without edges behind.
///
/// # Arguments
///
/// * `user` - The user creating the workflow
/// * `id` - The id of the new workflow
/// * `form` - The submitted form
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::create", skip_all, fields(id = id.to_string()), err(Debug))]
pub(crate) async fn create(
    user: &User,
    id: Uuid,
    mut form: EntityForm,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // check the name and groups
    let name = tools::check_name_and_groups(form.name.as_ref(), &form.groups)?;
    // a new name can't be the word for an entity kind
    crate::utils::bounder::entity_name(&name)?;
    // take the workflow's fields from the form
    let req = WorkflowCatalogEntityRequest::from_form(&mut form.metadata)?;
    // the entry agent must be one of the workflow's agents
    if let Some(entry) = req.entry_agent
        && !req.agents.contains(&entry)
    {
        return entry_refusal(entry, &name);
    }
    // build the workflow, which is first modified when it's created, at the millisecond
    // precision its creation time is stored with
    let now = Utc::now().trunc_subsecs(3);
    let entity = Entity {
        id,
        name: name.clone(),
        kind: EntityKinds::WorkflowCatalog,
        metadata: EntityMetadata::WorkflowCatalog(WorkflowCatalogEntity {
            entry_agent: req.entry_agent,
            auto_instance: req.auto_instance.unwrap_or(true),
            limits: req.limits.unwrap_or_default(),
            default_models: req.default_models,
            model_auth: req.model_auth.unwrap_or_default(),
            agents: Vec::new(),
            modified: now,
            latest_instance: None,
            newest_instance: None,
            highest_revision: 0,
        }),
        description: form.description,
        submitter: user.username.clone(),
        groups: form.groups,
        tags: HashMap::default(),
        image: form.image,
        created: now,
    };
    // check every edge before anything is written
    let requested = orchestrate_requests(AGENTS_FIELD, &req.agents);
    let mut plan = helpers::heap_future(|| {
        edges::check_graph_edges(
            user,
            &entity,
            false,
            &requested,
            req.extend_target_groups,
            shared,
        )
    })
    .await?;
    // the workflow's agents must fit what a resolution reads
    let checked = edges::check_edge_caps(&entity, &HashSet::new(), &plan, shared);
    plan.release_on_error(checked, shared).await?;
    // write the workflow, then its edges, whose changes the workflow's own change covers
    let inserted = db::entities::insert(user, &entity, form.tags, shared).await;
    plan.release_on_error(inserted, shared).await?;
    plan.skip_edge_events = true;
    let written =
        helpers::heap_future(|| edges::write_graph_edges(user, &entity, plan, shared)).await;
    // queue the new workflow's change even when an edge failed, since it covers the edges that
    // were written
    let change = WorkflowGraphChange::WorkflowUpdated { workflow: id };
    let queued = Event::queue_workflow_graph(user, &entity.groups, change, shared).await;
    // a failed edge write is the first failure, so it's returned over a failure to queue
    written.and(queued)?;
    Ok(EntityResponse::new(id, name))
}

/// Queue a user's request for a new instance of a workflow
///
/// Only a user who can edit the workflow in every one of its groups may ask for one. Nothing
/// is built here; the request is only queued.
///
/// # Arguments
///
/// * `user` - The user asking for the instance
/// * `id` - The id of the workflow
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::request_instance", skip(user, shared), err(Debug))]
pub(crate) async fn request_instance(
    user: &User,
    id: Uuid,
    shared: &Shared,
) -> Result<(), ApiError> {
    // the workflow must exist and be visible
    let workflow = Entity::get(user, id, shared).await?;
    // anything but a workflow catalog isn't a workflow to this route
    if workflow.kind != EntityKinds::WorkflowCatalog {
        return not_found!(format!("Workflow {id} not found"));
    }
    // the user must be able to edit the workflow in every one of its groups
    let groups = db::entities::all_groups(workflow.id, shared).await?;
    Group::authorize_graph(
        user,
        &groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "instance",
        &workflow.name,
        shared,
    )
    .await?;
    // queue the request in every group of the workflow
    let change = WorkflowGraphChange::ManualInstance { workflow: id };
    Event::queue_workflow_graph(user, &groups, change, shared).await
}

/// Apply the settings fields of an update to a workflow
///
/// An entry agent that won't be one of the workflow's agents once the update's agent changes
/// are applied is refused, and an entry agent is either set or cleared. Default models replace
/// the whole list, and every update marks the workflow modified.
///
/// # Arguments
///
/// * `name` - The name of the workflow
/// * `workflow` - The workflow's metadata, with its agents filled in
/// * `form` - The metadata update form
pub(crate) fn update_meta(
    name: &str,
    workflow: &mut WorkflowCatalogEntity,
    form: &mut EntityMetadataUpdateForm,
) -> Result<(), ApiError> {
    // an entry agent is either replaced or cleared
    let entry = form.workflow_entry_agent.take();
    let clear = form.clear_workflow_entry_agent == Some(true);
    if clear && entry.is_some() {
        return bad!("set a new workflow entry agent or clear it, not both".to_owned());
    }
    // a new entry agent must be one of the agents the workflow has after this update
    if let Some(entry) = entry {
        let kept = workflow.agents.iter().any(|agent| agent.id == entry)
            && !form.remove_workflow_agents.contains(&entry);
        if !kept && !form.add_workflow_agents.contains(&entry) {
            return entry_refusal(entry, name);
        }
        workflow.entry_agent = Some(entry);
    }
    if clear {
        workflow.entry_agent = None;
    }
    // replace the settings that were sent
    update!(workflow.auto_instance, form.workflow_auto_instance);
    if let Some(raw) = form.workflow_limits.take() {
        workflow.limits = parse_json_field("workflow_limits", &raw)?;
    }
    if let Some(raw) = form.workflow_default_models.take() {
        workflow.default_models = parse_json_field("workflow_default_models", &raw)?;
    }
    if let Some(raw) = form.workflow_model_auth.take() {
        workflow.model_auth = parse_json_field("workflow_model_auth", &raw)?;
    }
    // any edit marks the workflow modified
    workflow.modified = Utc::now();
    Ok(())
}

/// Check the `Orchestrates` edges an update adds and removes, then write them
///
/// Every added edge is checked before any edge is added or removed, so one bad edge rejects
/// the whole update. Removing an agent the workflow doesn't have changes nothing, and adding one
/// it already has adds nothing.
///
/// # Arguments
///
/// * `user` - The user updating the workflow
/// * `entity` - The workflow, with its agents filled in
/// * `form` - The metadata update form
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::update_edges", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn update_edges(
    user: &User,
    entity: &mut Entity,
    form: &mut EntityMetadataUpdateForm,
    shared: &Shared,
) -> Result<(), ApiError> {
    // collect the agents to remove and the edges to add
    let removes = form
        .remove_workflow_agents
        .iter()
        .copied()
        .collect::<HashSet<Uuid>>();
    let requested = orchestrate_requests(ADD_AGENTS_FIELD, &form.add_workflow_agents);
    // nothing changes without edge fields
    if requested.is_empty() && removes.is_empty() {
        return Ok(());
    }
    // the agents the workflow already orchestrates
    let existing = workflow_meta(entity)?
        .agents
        .iter()
        .map(|agent| agent.id)
        .collect::<HashSet<Uuid>>();
    // check every added edge before anything is written
    let extend = form.extend_target_groups == Some(true);
    let mut plan = helpers::heap_future(|| {
        edges::check_graph_edges(user, entity, true, &requested, extend, shared)
    })
    .await?;
    plan.edges
        .retain(|edge| !existing.contains(&edge.target.id) || removes.contains(&edge.target.id));
    // the workflow's agents after the update must fit what a resolution reads
    let capped = removes
        .iter()
        .map(|id| (AssociationKind::Orchestrates, *id))
        .collect::<HashSet<(AssociationKind, Uuid)>>();
    let checked = edges::check_edge_caps(entity, &capped, &plan, shared);
    plan.release_on_error(checked, shared).await?;
    // remove the edges named for removal, then add the new ones
    let dropped = remove_orchestrates(user, entity, &removes, shared).await;
    plan.release_on_error(dropped, shared).await?;
    // an empty plan writes nothing but still releases its locks
    if plan.is_empty() {
        plan.locks.release(shared).await;
        return Ok(());
    }
    helpers::heap_future(|| edges::write_graph_edges(user, entity, plan, shared)).await
}

/// Remove a workflow's stored `Orchestrates` edges to some agents
///
/// # Arguments
///
/// * `user` - The user updating the workflow
/// * `entity` - The workflow
/// * `removes` - The agents whose edges to remove
/// * `shared` - Shared Thorium objects
async fn remove_orchestrates(
    user: &User,
    entity: &Entity,
    removes: &HashSet<Uuid>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // nothing is read without removals
    if removes.is_empty() {
        return Ok(());
    }
    // find the stored rows of the edges to remove
    let rows = orchestrates(user, entity.id, &entity.groups, shared)
        .await?
        .into_iter()
        .filter_map(|assoc| match agents::other_entity(&assoc) {
            Ok(Some(id)) => removes.contains(&id).then_some(Ok(assoc)),
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

/// List a workflow's outgoing `Orchestrates` edges
///
/// # Arguments
///
/// * `user` - The user reading the edges
/// * `workflow` - The workflow
/// * `groups` - The groups to read the edges in
/// * `shared` - Shared Thorium objects
async fn orchestrates(
    user: &User,
    workflow: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<ListableAssociation>, ApiError> {
    // list the workflow's outgoing orchestrates edges
    agents::edges_of(
        user,
        workflow,
        groups,
        &WORKFLOW_EDGES,
        Directionality::To,
        shared,
    )
    .await
}

/// Get every instance of a workflow in some groups, from their `BuiltFrom` edges
///
/// Only workflow instances of this workflow are kept, whatever the edges point at.
///
/// # Arguments
///
/// * `user` - The user reading the instances
/// * `workflow` - The id of the workflow
/// * `groups` - The groups to read the instances in
/// * `shared` - Shared Thorium objects
pub(crate) async fn instances(
    user: &User,
    workflow: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // find the instances that point at this workflow
    let built = agents::edges_of(
        user,
        workflow,
        groups,
        &[AssociationKind::BuiltFrom],
        Directionality::From,
        shared,
    )
    .await?;
    let mut ids = Vec::with_capacity(built.len());
    for assoc in &built {
        if let Some(id) = agents::other_entity(assoc)?
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    // there is nothing to read without instances
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // read the instances, keeping only instances of this workflow
    let mut found = db::entities::get_many(groups, &ids, shared).await?;
    found.retain(|entity| {
        matches!(&entity.metadata, EntityMetadata::WorkflowInstance(instance) if instance.workflow == workflow)
    });
    Ok(found)
}

/// Find a workflow's newest instance and its newest ready instance
///
/// # Arguments
///
/// * `found` - The workflow's instances
fn newest(found: &[Entity]) -> (Option<Uuid>, Option<Uuid>) {
    // the newest instance of any status has the highest revision
    let mut newest: Option<(Uuid, u64)> = None;
    let mut ready: Option<(Uuid, u64)> = None;
    for entity in found {
        let EntityMetadata::WorkflowInstance(instance) = &entity.metadata else {
            continue;
        };
        let rev = instance.revision;
        if newest.is_none_or(|(_, best)| rev > best) {
            newest = Some((entity.id, rev));
        }
        // the newest ready instance is the workflow's latest
        if instance.is_ready() && ready.is_none_or(|(_, best)| rev > best) {
            ready = Some((entity.id, rev));
        }
    }
    (newest.map(|(id, _)| id), ready.map(|(id, _)| id))
}

/// Fill a workflow's agents
///
/// The agents are read in the workflow's visible groups, which they cover. Its newest and
/// latest instances are stored on it, so no instance is read.
///
/// # Arguments
///
/// * `user` - The user reading the workflow
/// * `entity` - The workflow
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::populate", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn populate(
    user: &User,
    mut entity: Entity,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // collect the orchestrated agents
    let mut ids = Vec::new();
    for assoc in orchestrates(user, entity.id, &entity.groups, shared).await? {
        if let Some(id) = agents::other_entity(&assoc)?
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    let agents = if ids.is_empty() {
        Vec::new()
    } else {
        db::entities::get_many(&entity.groups, &ids, shared).await?
    };
    // fill the workflow's populated agents
    if let EntityMetadata::WorkflowCatalog(workflow) = &mut entity.metadata {
        workflow.agents = agents;
    }
    Ok(entity)
}

/// Move a workflow's newest and latest instance off an instance that was deleted
///
/// Only a deleted instance the workflow names as either one makes the workflow's instances be
/// read again. The caller holds the workflow's lock.
///
/// # Arguments
///
/// * `user` - The user who deleted the instance
/// * `workflow` - The id of the workflow
/// * `deleted` - The id of the deleted instance
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::refresh_instances", skip(user, shared), err(Debug))]
pub(crate) async fn refresh_instances(
    user: &User,
    workflow: Uuid,
    deleted: Uuid,
    shared: &Shared,
) -> Result<(), ApiError> {
    // a workflow that was deleted has nothing to move
    let groups = match db::entities::all_groups(workflow, shared).await {
        Ok(groups) if !groups.is_empty() => groups,
        Ok(_) => return Ok(()),
        Err(err) if err.status() == axum::http::StatusCode::NOT_FOUND => return Ok(()),
        Err(err) => return Err(err),
    };
    // only a deleted newest or latest instance moves anything
    let mut stored = db::entities::get_untagged(&groups, workflow, shared).await?;
    stored.groups.clone_from(&groups);
    let meta = workflow_meta(&stored)?;
    if meta.newest_instance != Some(deleted) && meta.latest_instance != Some(deleted) {
        return Ok(());
    }
    // find the newest and newest ready instances left
    let found = instances(user, workflow, &groups, shared).await?;
    let (newest, latest) = newest(&found);
    if let EntityMetadata::WorkflowCatalog(meta) = &mut stored.metadata {
        meta.newest_instance = newest;
        meta.latest_instance = latest;
    }
    // rewrite the workflow's rows with them
    let highest = workflow_meta(&stored)?.highest_revision;
    db::entities::set_highest_revision(stored, highest, shared).await
}

/// One entity an existing instance snapshotted, checked when its root gains groups
pub(crate) struct Snapshotted<'a> {
    /// The name of the instance that snapshotted it
    pub instance: &'a str,
    /// The id of the snapshotted entity
    pub id: Uuid,
    /// What the snapshotted entity is called in errors
    pub name: String,
}

/// Refuse a root's group add while one of its instances snapshotted an entity that isn't in
/// every added group
///
/// A root's instances gain its added groups with their snapshots, so a snapshot of an entity
/// the root no longer uses can't reach a group that entity isn't in.
///
/// # Arguments
///
/// * `label` - What the root is called in errors
/// * `snapshotted` - Every entity the root's instances snapshotted
/// * `add` - The groups being added
/// * `shared` - Shared Thorium objects
pub(crate) async fn check_snapshots_cover(
    label: &str,
    snapshotted: &[Snapshotted<'_>],
    add: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // nothing is read without an add or snapshots
    if add.is_empty() || snapshotted.is_empty() {
        return Ok(());
    }
    // read the groups of every snapshotted entity at once
    let mut ids = snapshotted
        .iter()
        .map(|snap| snap.id)
        .collect::<Vec<Uuid>>();
    ids.sort_unstable();
    ids.dedup();
    let mut groups: HashMap<Uuid, Vec<String>> = HashMap::default();
    for row in db::entities::groups_and_kinds(&ids, shared).await? {
        groups.entry(row.id).or_default().push(row.group);
    }
    // every snapshotted entity must already be in each added group
    for snap in snapshotted {
        let stored = groups.get(&snap.id).map_or(&[][..], Vec::as_slice);
        let missing = helpers::missing_groups(add, stored);
        if !missing.is_empty() {
            return bad!(format!(
                "{label} can't gain groups {missing:?}: its instance `{}` snapshotted `{}`, which isn't in them; add them to `{}` or delete that instance first",
                snap.instance, snap.name, snap.name
            ));
        }
    }
    Ok(())
}

/// List every entity a workflow's instances snapshotted
///
/// # Arguments
///
/// * `instances` - The workflow's instances
fn snapshotted(instances: &[Entity]) -> Vec<Snapshotted<'_>> {
    // each instance's agents, tools, and toolset instances
    let mut found = Vec::new();
    for entity in instances {
        let EntityMetadata::WorkflowInstance(instance) = &entity.metadata else {
            continue;
        };
        let snap = |id: Uuid, name: String| Snapshotted {
            instance: &entity.name,
            id,
            name,
        };
        found.extend(
            instance
                .agents
                .iter()
                .map(|agent| snap(agent.agent, agent.name.clone())),
        );
        found.extend(
            instance
                .tools
                .iter()
                .map(|tool| snap(tool.tool_instance, tool.name.clone())),
        );
        found.extend(
            instance
                .agents
                .iter()
                .flat_map(|agent| &agent.tool_sets)
                .map(|id| snap(*id, format!("toolset instance {id}"))),
        );
    }
    found
}

/// Check a group change of a workflow
///
/// Every agent the workflow orchestrates must be in each added group, except those the same
/// update removes, and so must everything its existing instances snapshotted; a group can't be
/// removed while a graph edge from another entity in that group references the workflow.
///
/// # Arguments
///
/// * `user` - The user changing the workflow's groups
/// * `workflow` - The workflow, in all of its groups
/// * `instances` - The workflow's instances
/// * `add` - The groups being added
/// * `remove` - The groups being removed
/// * `dropped` - The agents whose `Orchestrates` edges the same update removes
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::check_group_change", skip_all, fields(id = workflow.id.to_string()), err(Debug))]
pub(crate) async fn check_group_change(
    user: &User,
    workflow: &Entity,
    instances: &[Entity],
    add: &[String],
    remove: &[String],
    dropped: &[Uuid],
    shared: &Shared,
) -> Result<(), ApiError> {
    // check the workflow's agents and its referrers, except the agents being removed
    let dropped = dropped
        .iter()
        .map(|id| (AssociationKind::Orchestrates, *id))
        .collect::<Vec<(AssociationKind, Uuid)>>();
    let label = format!("workflow `{}`", workflow.name);
    let change = GroupChange {
        entity: workflow,
        out_kinds: &WORKFLOW_EDGES,
        referenced: &[workflow.id],
        label: &label,
        add,
        remove,
        dropped: &dropped,
    };
    agents::check_contained_group_change(user, &change, shared).await?;
    // the instances' snapshots must be in each added group too
    check_snapshots_cover(&label, &snapshotted(instances), add, shared).await
}

/// Refuse the changes a workflow instance never takes
///
/// A workflow instance takes only a new name or description; its metadata never changes, and
/// its groups follow its workflow.
///
/// # Arguments
///
/// * `form` - The update form
/// * `image_uploaded` - Whether the update uploads an image
pub(crate) fn check_instance_update(
    form: &EntityUpdateForm,
    image_uploaded: bool,
) -> Result<(), ApiError> {
    // no metadata field can change
    if let Some(field) = form.metadata.sent.first() {
        return bad!(format!(
            "a workflow instance's metadata can't change, so `{field}` was refused; only its name and description can"
        ));
    }
    // its image can't change either
    if form.clear_image.is_some() || image_uploaded {
        return bad!("a workflow instance only takes a new name or description".to_owned());
    }
    Ok(())
}

/// Drop the groups a reader can't see from a workflow instance's snapshot groups
///
/// The stored snapshot keeps every group, since submissions are checked against all of them;
/// a reader is only shown the groups they can see, as with an entity's own groups.
///
/// # Arguments
///
/// * `entity` - The entity being read
/// * `user` - The user reading it
pub(crate) fn hide_snapshot_groups(entity: &mut Entity, user: &User) {
    // only workflow instances record these snapshot groups
    if let EntityMetadata::WorkflowInstance(instance) = &mut entity.metadata {
        hide_instance_groups(instance, user);
    }
}

/// Drop the groups a reader can't see from every snapshot in a workflow instance
///
/// # Arguments
///
/// * `instance` - The workflow instance being read
/// * `user` - The user reading it
fn hide_instance_groups(instance: &mut WorkflowInstanceEntity, user: &User) {
    // admins see every group
    if user.is_admin() {
        return;
    }
    // keep the workflow's, each agent's, and each tool's groups this user is in
    instance
        .workflow_groups
        .retain(|group| user.groups.contains(group));
    for agent in &mut instance.agents {
        agent.groups.retain(|group| user.groups.contains(group));
    }
    for tool in &mut instance.tools {
        tool.groups.retain(|group| user.groups.contains(group));
    }
}

/// What kind of entity a submitted instance references, for reading and refusing references
#[derive(Debug, Clone, Copy)]
struct ReferenceKind {
    /// The kind every referenced entity must be
    kind: EntityKinds,
    /// What a reference of this kind is called in errors
    label: &'static str,
    /// What a reference of this kind is called after an article, in errors
    described: &'static str,
}

/// The agents a submitted workflow instance references
const AGENT_REFERENCES: ReferenceKind = ReferenceKind {
    kind: EntityKinds::Agent,
    label: "agent",
    described: "an agent",
};

/// The tool instances a submitted workflow instance references
const TOOL_REFERENCES: ReferenceKind = ReferenceKind {
    kind: EntityKinds::ToolInstance,
    label: "tool instance",
    described: "a tool instance",
};

/// The toolset instances a submitted workflow instance references
const TOOL_SET_REFERENCES: ReferenceKind = ReferenceKind {
    kind: EntityKinds::ToolSetInstance,
    label: "toolset instance",
    described: "a toolset instance",
};

/// The entities a submitted instance references, as they're stored
#[derive(Debug, Default)]
struct References {
    /// Every group each referenced entity is stored in
    groups: BTreeMap<Uuid, Vec<String>>,
    /// Each referenced entity, read in every one of its groups
    entities: HashMap<Uuid, Entity>,
}

impl References {
    /// Get a referenced entity's stored groups
    ///
    /// # Arguments
    ///
    /// * `id` - The referenced entity
    fn groups_of(&self, id: Uuid) -> &[String] {
        // a reference that was read always has groups
        self.groups.get(&id).map_or(&[], Vec::as_slice)
    }
}

/// Read the entities of one kind a submitted instance references
///
/// A reference that doesn't exist or that the submitter can't see is the same `404`, whatever
/// it is, and a visible reference of another kind is a `400`.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `ids` - The referenced ids
/// * `refs` - The kind of entity the ids must reference
/// * `shared` - Shared Thorium objects
async fn read_references(
    user: &User,
    ids: &[Uuid],
    refs: ReferenceKind,
    shared: &Shared,
) -> Result<References, ApiError> {
    // nothing is read without references
    let mut found = References::default();
    if ids.is_empty() {
        return Ok(found);
    }
    // read the group and kind of every reference's rows at once
    let mut kinds: HashMap<Uuid, EntityKinds> = HashMap::default();
    for row in db::entities::groups_and_kinds(ids, shared).await? {
        kinds.insert(row.id, row.kind);
        found.groups.entry(row.id).or_default().push(row.group);
    }
    // every reference must exist, be visible to the submitter, and be of the right kind
    for id in ids {
        // a reference the submitter can't see answers like a missing one
        let visible = found.groups.get(id).is_some_and(|stored| {
            user.is_admin() || stored.iter().any(|group| user.groups.contains(group))
        });
        if !visible {
            return not_found!(format!("{} `{id}` doesn't exist", refs.label));
        }
        if kinds.get(id) != Some(&refs.kind) {
            return bad!(format!("`{id}` is not {}", refs.described));
        }
    }
    // read every reference in all of the groups they're stored in
    let all = found
        .groups
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<String>>()
        .into_iter()
        .collect::<Vec<String>>();
    let ids = found.groups.keys().copied().collect::<Vec<Uuid>>();
    for entity in db::entities::get_many(&all, &ids, shared).await? {
        found.entities.insert(entity.id, entity);
    }
    Ok(found)
}

/// Refuse a submitted instance larger than any instance a workflow can resolve to
///
/// Every check after this one does work in proportion to the instance's size while the
/// workflow's lock is held, so the size is bounded first, by the same bounds on what a
/// resolution under the crawl limits can list, so every resolved instance, ready or invalid,
/// can be submitted.
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `limits` - The configured crawl limits
fn check_size(instance: &WorkflowInstanceEntity, limits: &CrawlLimits) -> Result<(), ApiError> {
    // the whole instance lists a bounded number of agents and tools
    let max_agents = limits.max_agents_per_workflow;
    if instance.agents.len() > max_agents {
        return bad!(format!(
            "a workflow instance can list at most max_agents_per_workflow ({max_agents}) agents, not {}",
            instance.agents.len()
        ));
    }
    let max_tools = limits.max_instance_tools();
    if instance.tools.len() > max_tools {
        return bad!(format!(
            "a workflow instance can list at most {max_tools} tools, not {}",
            instance.tools.len()
        ));
    }
    // each agent lists a bounded number of tools, toolset instances, and sub-agents
    let listed = limits.max_listed_tools_per_agent();
    let max_agent_tools = limits.max_tools_per_agent;
    for agent in &instance.agents {
        if agent.tools.len() > listed {
            return bad!(format!(
                "agent `{}` can list at most {listed} tools (twice max_tools_per_agent ({max_agent_tools})), not {}",
                agent.name,
                agent.tools.len()
            ));
        }
        if agent.tool_sets.len() > listed || agent.sub_agents.len() > max_agents {
            return bad!(format!(
                "agent `{}` can list at most {listed} toolset instances and {max_agents} sub-agents",
                agent.name
            ));
        }
    }
    Ok(())
}

/// Refuse a list of ids that names one id twice
///
/// # Arguments
///
/// * `label` - What the ids are called in errors
/// * `ids` - The ids to check
fn check_unique(label: &str, ids: &[Uuid]) -> Result<(), ApiError> {
    // every id must be new to the set
    let mut seen = HashSet::with_capacity(ids.len());
    if let Some(repeat) = ids.iter().find(|id| !seen.insert(**id)) {
        return bad!(format!("{label} `{repeat}` is listed more than once"));
    }
    Ok(())
}

/// Check that each tool of a submitted instance matches the stored tool instance
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `stored` - The stored tool instances
fn check_tools(instance: &WorkflowInstanceEntity, stored: &References) -> Result<(), ApiError> {
    // each tool's root, hash, version, function name, and kind come from the stored instance
    for tool in &instance.tools {
        let Some(EntityMetadata::ToolInstance(found)) = stored
            .entities
            .get(&tool.tool_instance)
            .map(|entity| &entity.metadata)
        else {
            return not_found!(format!(
                "tool instance `{}` doesn't exist",
                tool.tool_instance
            ));
        };
        let (name, id) = (&tool.name, tool.tool_instance);
        if found.tool != tool.tool {
            return bad!(format!(
                "tool `{name}` names tool `{}`, but tool instance `{id}` belongs to tool `{}`",
                tool.tool, found.tool
            ));
        }
        if found.spec_hash != tool.spec_hash {
            return bad!(format!(
                "tool `{name}` has spec_hash `{}`, but tool instance `{id}` has `{}`",
                tool.spec_hash, found.spec_hash
            ));
        }
        let version = found.version.to_string();
        if version != tool.version {
            return bad!(format!(
                "tool `{name}` has version `{}`, but tool instance `{id}` has `{version}`",
                tool.version
            ));
        }
        let function_name = found.spec.function_name();
        if function_name != tool.function_name.as_deref() {
            return bad!(format!(
                "tool `{name}` has function_name {:?}, but tool instance `{id}` has {function_name:?}",
                tool.function_name
            ));
        }
        if found.spec.kind() != tool.kind {
            return bad!(format!(
                "tool `{name}` is recorded as a {} tool, but tool instance `{id}` is a {} tool",
                tool.kind,
                found.spec.kind()
            ));
        }
    }
    Ok(())
}

/// Check that every tool a submitted instance lists is one its agents use
///
/// A tool is listed because an agent is given it or runs in it, or because a listed tool's
/// container backing pins it as a dependency; any other tool would skip the group checks the
/// agents' and the workflow's edges make.
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `stored` - The stored tool instances
fn check_tool_uses(instance: &WorkflowInstanceEntity, stored: &References) -> Result<(), ApiError> {
    // the tools the agents are given or run in
    let used = instance
        .agents
        .iter()
        .flat_map(|agent| agent.tools.iter().chain(&agent.harness))
        .collect::<HashSet<&Uuid>>();
    // the instances the listed tools' container backings pin
    let backing = instance
        .tools
        .iter()
        .filter_map(|tool| match stored.entities.get(&tool.tool_instance) {
            Some(Entity {
                metadata: EntityMetadata::ToolInstance(found),
                ..
            }) => found.spec.pinned_instance(),
            _ => None,
        })
        .collect::<HashSet<Uuid>>();
    // every tool is used directly or backs another listed tool
    for tool in &instance.tools {
        if tool.dependency {
            if !backing.contains(&tool.tool_instance) {
                return bad!(format!(
                    "tool `{}` is listed as a dependency, but it backs none of the instance's tools",
                    tool.name
                ));
            }
        } else if !used.contains(&tool.tool_instance) {
            return bad!(format!(
                "tool `{}` isn't given to any of the instance's agents or listed as a dependency",
                tool.name
            ));
        }
    }
    Ok(())
}

/// Check that each agent of a submitted instance matches the stored agent
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `stored` - The stored agents
fn check_agents(instance: &WorkflowInstanceEntity, stored: &References) -> Result<(), ApiError> {
    // each agent's config hash comes from the stored agent, and its config hashes to it
    for agent in &instance.agents {
        let Some(EntityMetadata::Agent(found)) = stored
            .entities
            .get(&agent.agent)
            .map(|entity| &entity.metadata)
        else {
            return not_found!(format!("agent `{}` doesn't exist", agent.agent));
        };
        if found.config_hash != agent.config_hash {
            return bad!(format!(
                "agent `{}` has config_hash `{}`, but the stored agent has `{}`",
                agent.name, agent.config_hash, found.config_hash
            ));
        }
        let computed = match agent.config.config_hash() {
            Ok(hash) => hash,
            Err(err) => return internal_err!(format!("failed to hash an agent config: {err}")),
        };
        if computed != agent.config_hash {
            return bad!(format!(
                "agent `{}`'s config hashes to `{computed}`, not its config_hash `{}`",
                agent.name, agent.config_hash
            ));
        }
    }
    Ok(())
}

/// Check that a submitted instance's agents only reference its own tools and agents
///
/// A ready instance must name one of its agents as its entry agent.
///
/// # Arguments
///
/// * `instance` - The submitted instance
fn check_references(instance: &WorkflowInstanceEntity) -> Result<(), ApiError> {
    // index the instance's tools and agents by id
    let tools = instance
        .tools
        .iter()
        .map(|tool| tool.tool_instance)
        .collect::<HashSet<Uuid>>();
    let agents = instance
        .agents
        .iter()
        .map(|agent| agent.agent)
        .collect::<HashSet<Uuid>>();
    // check each agent's tools, harness, and sub-agents
    for agent in &instance.agents {
        let name = &agent.name;
        if let Some(missing) = agent.tools.iter().find(|id| !tools.contains(id)) {
            return bad!(format!(
                "agent `{name}` names tool instance `{missing}`, which isn't in the instance's tools"
            ));
        }
        if let Some(harness) = agent.harness
            && !tools.contains(&harness)
        {
            return bad!(format!(
                "agent `{name}` runs in harness `{harness}`, which isn't in the instance's tools"
            ));
        }
        if let Some(missing) = agent.sub_agents.iter().find(|id| !agents.contains(id)) {
            return bad!(format!(
                "agent `{name}` delegates to `{missing}`, which isn't one of the instance's agents"
            ));
        }
    }
    // a ready instance has an entry agent that is one of its agents
    if instance.is_ready() {
        match instance.entry_agent {
            Some(entry) if agents.contains(&entry) => (),
            Some(entry) => {
                return bad!(format!(
                    "a Ready instance's entry agent `{entry}` must be one of its agents"
                ));
            }
            None => return bad!("a Ready instance needs an entry agent".to_owned()),
        }
    }
    Ok(())
}

/// Check that each agent's tool bindings name its own tools and mechanisms the entry agent's
/// harness instance can carry them out with
///
/// A Ready instance must also run its entry agent in a Harness instance and bind every tool of
/// each agent.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `instance` - The submitted instance
/// * `stored` - The stored tool instances
/// * `shared` - Shared Thorium objects
async fn check_bindings(
    user: &User,
    instance: &WorkflowInstanceEntity,
    stored: &References,
    shared: &Shared,
) -> Result<(), ApiError> {
    // an Invalid instance without bindings has nothing to check
    let ready = instance.is_ready();
    if !ready
        && instance
            .agents
            .iter()
            .all(|agent| agent.bindings.is_empty())
    {
        return Ok(());
    }
    // bindings are made against the entry agent's harness instance
    let harness = instance
        .entry_agent
        .and_then(|entry| instance.agent(entry))
        .and_then(|entry| entry.harness)
        .and_then(|id| stored.entities.get(&id));
    let Some((harness_entity, harness_spec)) = harness.and_then(|entity| match &entity.metadata {
        EntityMetadata::ToolInstance(meta) => match &meta.spec {
            ToolSpec::Harness(spec) => Some((entity, spec)),
            _ => None,
        },
        _ => None,
    }) else {
        return bad!(
            "the instance's entry agent has no Harness instance to bind its tools against"
                .to_owned()
        );
    };
    // read the native tools the harness instance provides
    let provided = tools::provided_ids(user, harness_entity, shared)
        .await?
        .into_iter()
        .collect::<HashSet<Uuid>>();
    // each binding names one of its agent's tools once and a mechanism that tool can be bound to
    for agent in &instance.agents {
        let mut bound = HashSet::with_capacity(agent.bindings.len());
        for binding in &agent.bindings {
            let id = binding.tool_instance;
            if !agent.tools.contains(&id) {
                return bad!(format!(
                    "agent `{}` binds tool instance `{id}`, which isn't one of its tools",
                    agent.name
                ));
            }
            if !bound.insert(id) {
                return bad!(format!(
                    "agent `{}` binds tool instance `{id}` more than once",
                    agent.name
                ));
            }
            let Some(Entity {
                name,
                metadata: EntityMetadata::ToolInstance(tool),
                ..
            }) = stored.entities.get(&id)
            else {
                return not_found!(format!("tool instance `{id}` doesn't exist"));
            };
            let check = BindingCheck {
                tool_name: name,
                tool: &tool.spec,
                harness_name: &harness_entity.name,
                harness: harness_spec,
                provided: provided.contains(&id),
            };
            if let Err(msg) = check.check(binding.mechanism) {
                return bad!(format!("agent `{}`: {msg}", agent.name));
            }
        }
        // a Ready instance binds every one of the agent's tools
        if ready && let Some(id) = agent.tools.iter().find(|id| !bound.contains(*id)) {
            return bad!(format!(
                "agent `{}` doesn't bind tool instance `{id}`",
                agent.name
            ));
        }
    }
    Ok(())
}

/// Check that each agent's toolset digest matches its tools
///
/// # Arguments
///
/// * `instance` - The submitted instance
fn check_toolset_digests(instance: &WorkflowInstanceEntity) -> Result<(), ApiError> {
    // the toolset digest must cover exactly each agent's tools
    for agent in &instance.agents {
        let computed = match agent.compute_toolset_digest(&instance.tools) {
            Ok(digest) => digest,
            Err(err) => return internal_err!(format!("failed to hash an agent's tools: {err}")),
        };
        if computed != agent.toolset_digest {
            return bad!(format!(
                "agent `{}` has toolset_digest `{}`, but its tools hash to `{computed}`",
                agent.name, agent.toolset_digest
            ));
        }
    }
    Ok(())
}

/// Check the toolset instances a submitted instance's agents were given
///
/// Each must be ready and every one of its members must be among the agent's tools. Returns
/// each toolset instance's digest.
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `stored` - The stored toolset instances
fn check_tool_sets(
    instance: &WorkflowInstanceEntity,
    stored: &References,
) -> Result<BTreeMap<Uuid, String>, ApiError> {
    // check each agent's toolset instances
    let mut digests = BTreeMap::new();
    for agent in &instance.agents {
        for id in &agent.tool_sets {
            let Some(EntityMetadata::ToolSetInstance(found)) =
                stored.entities.get(id).map(|entity| &entity.metadata)
            else {
                return not_found!(format!("toolset instance `{id}` doesn't exist"));
            };
            // only a ready toolset instance can be given to an agent
            if !found.is_ready() {
                return bad!(format!(
                    "toolset instance `{id}` is {}; only a Ready instance can be given to agent `{}`",
                    found.status.as_str(),
                    agent.name
                ));
            }
            // every member must be one of the agent's tools
            let given = agent.tools.iter().collect::<HashSet<&Uuid>>();
            if let Some(member) = found
                .members
                .iter()
                .find(|member| !given.contains(&member.tool_instance))
            {
                return bad!(format!(
                    "member `{}` of toolset instance `{id}` isn't among agent `{}`'s tools",
                    member.name, agent.name
                ));
            }
            digests.insert(*id, found.digest.clone());
        }
    }
    Ok(digests)
}

/// The stored groups of everything a submitted workflow instance references
struct StoredGroups<'a> {
    /// The workflow, in all of its groups
    workflow: &'a Entity,
    /// The stored agents
    agents: &'a References,
    /// The stored tool instances
    tools: &'a References,
    /// The stored toolset instances
    tool_sets: &'a References,
}

/// Check a submitted workflow instance's snapshot groups against the stored groups
///
/// The workflow, each agent, and each tool record their stored groups. Every agent covers the
/// workflow's groups, each of an agent's tools, its harness, its sub-agents, and its toolset
/// instances cover the agent's, and every dependency covers the workflow's.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `instance` - The submitted instance
/// * `stored` - The stored groups of everything it references
fn check_snapshot(
    user: &User,
    instance: &WorkflowInstanceEntity,
    stored: &StoredGroups,
) -> Result<(), ApiError> {
    // the workflow, its agents, and its tools record their groups
    let workflow = stored.workflow;
    let mut nodes = vec![SnapshotNode {
        id: workflow.id,
        name: &workflow.name,
        recorded: &instance.workflow_groups,
        stored: &workflow.groups,
    }];
    nodes.extend(instance.agents.iter().map(|agent| SnapshotNode {
        id: agent.agent,
        name: &agent.name,
        recorded: &agent.groups,
        stored: stored.agents.groups_of(agent.agent),
    }));
    nodes.extend(instance.tools.iter().map(|tool| SnapshotNode {
        id: tool.tool_instance,
        name: &tool.name,
        recorded: &tool.groups,
        stored: stored.tools.groups_of(tool.tool_instance),
    }));
    // toolset instances aren't snapshotted, so their stored groups stand in for their record
    nodes.extend(
        stored
            .tool_sets
            .entities
            .values()
            .map(|entity| SnapshotNode {
                id: entity.id,
                name: &entity.name,
                recorded: stored.tool_sets.groups_of(entity.id),
                stored: stored.tool_sets.groups_of(entity.id),
            }),
    );
    // every edge of the snapshot must stay contained
    validate_snapshot_groups(user, &nodes, &snapshot_edges(workflow.id, instance))
}

/// Get the edges of a workflow instance's snapshot that must stay contained
///
/// The workflow reaches every agent, each agent its tools, harness, sub-agents, and toolset
/// instances, and the workflow every dependency.
///
/// # Arguments
///
/// * `workflow` - The id of the workflow
/// * `instance` - The workflow instance
fn snapshot_edges(workflow: Uuid, instance: &WorkflowInstanceEntity) -> Vec<SnapshotEdge> {
    // the workflow's edge to each agent and each agent's own edges
    let mut edges = Vec::new();
    for agent in &instance.agents {
        edges.push(SnapshotEdge {
            source: workflow,
            target: agent.agent,
        });
        let targets = agent
            .tools
            .iter()
            .chain(&agent.harness)
            .chain(&agent.sub_agents)
            .chain(&agent.tool_sets);
        edges.extend(targets.map(|target| SnapshotEdge {
            source: agent.agent,
            target: *target,
        }));
    }
    // the workflow's edge to each dependency
    edges.extend(
        instance
            .tools
            .iter()
            .filter(|tool| tool.dependency)
            .map(|tool| SnapshotEdge {
                source: workflow,
                target: tool.tool_instance,
            }),
    );
    edges
}

/// Check that a submitted workflow instance's digest matches its content
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `tool_sets` - The digest of each toolset instance its agents were given
fn check_digest(
    instance: &WorkflowInstanceEntity,
    tool_sets: &BTreeMap<Uuid, String>,
) -> Result<(), ApiError> {
    // recompute the digest from the instance's typed content
    let computed = match instance.compute_digest(tool_sets) {
        Ok(digest) => digest,
        Err(err) => return internal_err!(format!("failed to hash a workflow instance: {err}")),
    };
    // a digest that doesn't match the content is refused
    if computed != instance.digest {
        return bad!(format!(
            "digest `{}` doesn't match the workflow instance's content, which hashes to `{computed}`",
            instance.digest
        ));
    }
    Ok(())
}

/// Find the tool and toolset roots a submitted instance's agents follow at latest
///
/// They're read from each agent's own `Calls` and `RunsIn` edges, so a root an agent follows is
/// tagged even when it resolved to nothing and the instance is invalid; tools that come from a
/// toolset are followed through the toolset.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `instance` - The submitted instance
/// * `agents` - The stored agents
/// * `shared` - Shared Thorium objects
async fn followed_roots(
    user: &User,
    instance: &WorkflowInstanceEntity,
    agents: &References,
    shared: &Shared,
) -> Result<HashMap<String, HashSet<String>>, ApiError> {
    // collect the targets of every agent's own call and harness edges
    let mut targets = Vec::new();
    for agent in &instance.agents {
        let found = agents::edges_of(
            user,
            agent.agent,
            agents.groups_of(agent.agent),
            &FOLLOW_EDGES,
            Directionality::To,
            shared,
        )
        .await?;
        for assoc in found {
            if let Some(id) = agents::other_entity(&assoc)?
                && !targets.contains(&id)
            {
                targets.push(id);
            }
        }
    }
    // a target that is a tool or toolset root is followed at latest
    let mut tags: HashMap<String, HashSet<String>> = HashMap::default();
    if targets.is_empty() {
        return Ok(tags);
    }
    for row in db::entities::groups_and_kinds(&targets, shared).await? {
        let key = match row.kind {
            EntityKinds::Tool => FOLLOWS_TOOL_TAG,
            EntityKinds::ToolSet => FOLLOWS_TOOL_SET_TAG,
            _ => continue,
        };
        tag!(tags, key, row.id.to_string());
    }
    Ok(tags)
}

/// Read a workflow for an instance submission
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `id` - The id of the workflow
/// * `shared` - Shared Thorium objects
async fn read_workflow(user: &User, id: Uuid, shared: &Shared) -> Result<Entity, ApiError> {
    // the workflow must exist and be visible
    let workflow = Entity::get(user, id, shared).await?;
    // it must actually be a workflow catalog
    if workflow.kind != EntityKinds::WorkflowCatalog {
        return bad!(format!(
            "`{}` is a {}, not a workflow catalog",
            workflow.name, workflow.kind
        ));
    }
    Ok(workflow)
}

/// Create a submitted workflow instance from a form
///
/// The instance is validated but never rebuilt: the submitter must be able to edit the
/// workflow in all of its groups, a non-admin may only name themselves in `triggered_by`, every
/// agent, tool instance, and toolset instance must exist, be visible, and match what's stored,
/// the snapshot groups must match the stored ones and stay contained, and the digest must match
/// the content. A submitter who can't edit the workflow is refused before waiting on its lock.
/// Everything after finding the workflow is checked again, and the instance written, under the
/// workflow's lock, so a group change or delete of the workflow that finishes first is always
/// seen.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `id` - The id of the new instance
/// * `form` - The submitted form
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::create_instance", skip_all, fields(id = id.to_string()), err(Debug))]
pub(crate) async fn create_instance(
    user: &User,
    id: Uuid,
    mut form: EntityForm,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // parse the submitted instance, whose own tags can't use the keys Thorium sets on it
    let instance = WorkflowInstanceEntityRequest::parse_form(&mut form.metadata)?;
    Entity::refuse_intrinsic_tag_keys(EntityKinds::WorkflowInstance, form.tags.keys())?;
    // find the workflow
    let mut workflow = read_workflow(user, instance.workflow, shared).await?;
    // refuse a submitter who can't edit the workflow in every group before waiting on its lock
    workflow.groups = db::entities::all_groups(workflow.id, shared).await?;
    Group::authorize_graph(
        user,
        &workflow.groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "edit",
        &workflow.name,
        shared,
    )
    .await?;
    // check and write the instance while holding the workflow's lock
    let token = lock_for_instance(&workflow, shared).await?;
    let result = helpers::heap_future(|| {
        locked_create_instance(user, id, workflow.clone(), instance, form, shared)
    })
    .await;
    // release the lock whether or not the write worked
    tools::unlock_after_write(workflow.id, &token, shared).await;
    result
}

/// Take a workflow's lock for an instance submission, waiting briefly for another write
///
/// # Arguments
///
/// * `workflow` - The workflow
/// * `shared` - Shared Thorium objects
async fn lock_for_instance(workflow: &Entity, shared: &Shared) -> Result<String, ApiError> {
    // wait a short time for another write to this workflow
    match db::entities::lock_instances(workflow.id, &workflow.name, INSTANCE_LOCK_WAIT, shared)
        .await
    {
        Ok(token) => Ok(token),
        Err(err) if err.status() == axum::http::StatusCode::CONFLICT => conflict!(format!(
            "another instance of `{}` is being created; retry",
            workflow.name
        )),
        Err(err) => Err(err),
    }
}

/// Check a submitted workflow instance against its workflow and what it references, while
/// holding the workflow's lock
///
/// Returns the tags naming the roots the instance's agents follow at latest.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `workflow` - The workflow, in all of its groups
/// * `instance` - The submitted instance
/// * `shared` - Shared Thorium objects
async fn check_submitted(
    user: &User,
    workflow: &Entity,
    instance: &mut WorkflowInstanceEntity,
    shared: &Shared,
) -> Result<HashMap<String, HashSet<String>>, ApiError> {
    // the submitter must be able to edit the workflow in every one of its groups
    Group::authorize_graph(
        user,
        &workflow.groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "edit",
        &workflow.name,
        shared,
    )
    .await?;
    // a non-admin may only name themselves as the user who caused the instance
    if instance.triggered_by.is_empty() {
        instance.triggered_by.clone_from(&user.username);
    } else if !user.is_admin() && instance.triggered_by != user.username {
        return bad!(format!(
            "`{}` can only name themselves in triggered_by, not `{}`",
            user.username, instance.triggered_by
        ));
    }
    // the instance is no larger than any instance a workflow resolves to
    check_size(instance, &CrawlLimits::from(&shared.config.thorium.events))?;
    // each agent and tool is listed once
    let agent_ids = instance
        .agents
        .iter()
        .map(|agent| agent.agent)
        .collect::<Vec<Uuid>>();
    let tool_ids = instance
        .tools
        .iter()
        .map(|tool| tool.tool_instance)
        .collect::<Vec<Uuid>>();
    check_unique("agent", &agent_ids)?;
    check_unique("tool instance", &tool_ids)?;
    // every agent, tool, and toolset instance must exist, be visible, and match what's stored
    let agents = read_references(user, &agent_ids, AGENT_REFERENCES, shared).await?;
    check_agents(instance, &agents)?;
    let tools = read_references(user, &tool_ids, TOOL_REFERENCES, shared).await?;
    check_tools(instance, &tools)?;
    check_tool_uses(instance, &tools)?;
    check_references(instance)?;
    check_bindings(user, instance, &tools, shared).await?;
    check_toolset_digests(instance)?;
    let mut set_ids = instance
        .agents
        .iter()
        .flat_map(|agent| agent.tool_sets.iter().copied())
        .collect::<Vec<Uuid>>();
    set_ids.sort();
    set_ids.dedup();
    let tool_sets = read_references(user, &set_ids, TOOL_SET_REFERENCES, shared).await?;
    let digests = check_tool_sets(instance, &tool_sets)?;
    // the snapshot groups must match the stored groups and stay contained
    let stored = StoredGroups {
        workflow,
        agents: &agents,
        tools: &tools,
        tool_sets: &tool_sets,
    };
    check_snapshot(user, instance, &stored)?;
    // the digest must match the content
    check_digest(instance, &digests)?;
    // store the lists in their canonical order
    instance.sort_lists();
    // find the roots the agents follow at latest, which the instance is tagged with
    followed_roots(user, instance, &agents, shared).await
}

/// Check a submitted instance and its revision and write it, while holding its workflow's lock
///
/// A revision is never reused: the next one follows the highest revision any instance of the
/// workflow was created at, even when that instance was deleted since. The revision is recorded
/// as taken on the workflow's row before the instance is written, so a write that fails after
/// that burns the revision. The workflow's newest and latest instances are written last, so a
/// failed write leaves it naming only instances that exist.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `id` - The id of the new instance
/// * `workflow` - The workflow
/// * `instance` - The submitted instance
/// * `form` - The submitted form, for the description and user tags
/// * `shared` - Shared Thorium objects
async fn locked_create_instance(
    user: &User,
    id: Uuid,
    mut workflow: Entity,
    mut instance: WorkflowInstanceEntity,
    mut form: EntityForm,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // read the workflow's groups as the writes that finished before the lock left them
    workflow.groups = db::entities::all_groups(workflow.id, shared).await?;
    if workflow.groups.is_empty() {
        return not_found!(format!("workflow `{}` doesn't exist", workflow.name));
    }
    // check the instance against the workflow as it is now
    let followed =
        helpers::heap_future(|| check_submitted(user, &workflow, &mut instance, shared)).await?;
    // read the highest revision the workflow ever gave out and its newest instance left, as
    // stored now
    let mut stored = db::entities::get_untagged(&workflow.groups, workflow.id, shared).await?;
    stored.groups.clone_from(&workflow.groups);
    let highest = workflow_meta(&stored)?.highest_revision;
    // find the newest and latest instances again when the stored ones are gone
    if !instances_exist(&stored, shared).await? {
        let found = instances(user, workflow.id, &workflow.groups, shared).await?;
        let (newest, latest) = newest(&found);
        if let EntityMetadata::WorkflowCatalog(meta) = &mut stored.metadata {
            meta.newest_instance = newest;
            meta.latest_instance = latest;
        }
    }
    let previous = workflow_meta(&stored)?.newest_instance;
    // the revision must follow the highest one, and the instance supersedes the newest one left
    let expected = highest + 1;
    if instance.revision != expected {
        return conflict!(format!(
            "the next instance of workflow `{}` is revision {expected}, not {}",
            workflow.name, instance.revision
        ));
    }
    // the instance must supersede the newest instance
    if instance.previous != previous {
        return bad!(format!(
            "revision {expected} of workflow `{}` must name {} as its previous instance",
            workflow.name,
            previous.map_or_else(|| "no instance".to_owned(), |id| format!("`{id}`"))
        ));
    }
    // a ready instance becomes the workflow's latest once it's written
    let ready = instance.is_ready();
    // build the instance in every group of its workflow
    let name = WorkflowInstanceEntity::name_for(&workflow.name, instance.revision);
    let entity = Entity {
        id,
        name: name.clone(),
        kind: EntityKinds::WorkflowInstance,
        metadata: EntityMetadata::WorkflowInstance(instance),
        description: form.description.take(),
        submitter: user.username.clone(),
        groups: workflow.groups.clone(),
        tags: HashMap::default(),
        image: None,
        created: Utc::now(),
    };
    // tag the instance with the roots its agents follow, beside the submitter's own tags
    let mut tags = std::mem::take(&mut form.tags);
    for (key, values) in followed {
        tags.entry(key).or_default().extend(values);
    }
    // record the revision as taken on the workflow before anything else is written, so a write
    // that fails later burns the revision instead of leaving it free for a retry
    db::entities::set_highest_revision(stored.clone(), expected, shared).await?;
    // write the instance and link it to its workflow in every group
    #[cfg(feature = "test-utilities")]
    if crate::test_utilities::faults::take_entity_insert_failure(&name) {
        return internal_err!(format!("injected failure before writing instance `{name}`"));
    }
    db::entities::insert(user, &entity, tags, shared).await?;
    #[cfg(feature = "test-utilities")]
    if crate::test_utilities::faults::take_entity_inserted_failure(&name) {
        return internal_err!(format!("injected failure after writing instance `{name}`"));
    }
    let source = AssociationTarget::Entity {
        id,
        name: name.clone(),
    };
    let target = AssociationTarget::Entity {
        id: workflow.id,
        name: workflow.name.clone(),
    };
    db::associations::create(
        user,
        workflow.groups.len(),
        AssociationKind::BuiltFrom,
        source,
        &vec![(target, workflow.groups.clone())],
        None,
        Directionality::To,
        shared,
    )
    .await?;
    // name the written instance as the workflow's newest, and its latest when it's ready, so
    // the workflow never names an instance that wasn't written
    if let EntityMetadata::WorkflowCatalog(meta) = &mut stored.metadata {
        meta.newest_instance = Some(id);
        if ready {
            meta.latest_instance = Some(id);
        }
    }
    db::entities::set_highest_revision(stored, expected, shared).await?;
    Ok(EntityResponse::new(id, name))
}

/// Check that the newest and latest instances a workflow names, if any, still exist
///
/// A workflow that names no newest instance after giving out revisions counts as missing it,
/// so it's found again among the workflow's instances.
///
/// # Arguments
///
/// * `stored` - The workflow as stored
/// * `shared` - Shared Thorium objects
async fn instances_exist(stored: &Entity, shared: &Shared) -> Result<bool, ApiError> {
    // collect the instances the workflow names
    let meta = workflow_meta(stored)?;
    let named = meta
        .newest_instance
        .into_iter()
        .chain(meta.latest_instance)
        .collect::<BTreeSet<Uuid>>();
    // a workflow that gave out revisions must name its newest instance
    if named.is_empty() {
        return Ok(meta.highest_revision == 0);
    }
    // each named instance must still be a stored workflow instance
    let ids = named.iter().copied().collect::<Vec<Uuid>>();
    let found = db::entities::groups_and_kinds(&ids, shared)
        .await?
        .into_iter()
        .filter(|row| row.kind == EntityKinds::WorkflowInstance)
        .map(|row| row.id)
        .collect::<BTreeSet<Uuid>>();
    Ok(found == named)
}

/// Get one agent of a workflow instance and the stored spec of each tool it was given
///
/// The snapshot groups are filtered to the ones the reader can see.
///
/// # Arguments
///
/// * `user` - The user reading the toolset
/// * `id` - The id of the workflow instance
/// * `agent` - The id of the agent in that instance
/// * `shared` - Shared Thorium objects
#[instrument(name = "workflows::agent_toolset", skip(user, shared), err(Debug))]
pub(crate) async fn agent_toolset(
    user: &User,
    id: Uuid,
    agent: Uuid,
    shared: &Shared,
) -> Result<InstancedAgentToolset, ApiError> {
    // the instance must exist, be visible, and be a workflow instance
    let entity = Entity::get(user, id, shared).await?;
    let EntityMetadata::WorkflowInstance(mut instance) = entity.metadata else {
        return not_found!(format!("workflow instance {id} not found"));
    };
    // only show the snapshot groups this user can see
    hide_instance_groups(&mut instance, user);
    // the agent must be one of the instance's agents
    let Some(instanced) = instance.agent(agent).cloned() else {
        return not_found!(format!(
            "agent {agent} is not in workflow instance `{}`",
            entity.name
        ));
    };
    // read the stored spec of each of the agent's tools, which cover the instance's groups
    let stored = if instanced.tools.is_empty() {
        Vec::new()
    } else {
        db::entities::get_many(&entity.groups, &instanced.tools, shared).await?
    };
    let mut tools = Vec::with_capacity(instanced.tools.len());
    for tool_instance in &instanced.tools {
        // the instance lists every tool its agents use
        let Some(tool) = instance.tool(*tool_instance) else {
            return internal_err!(format!(
                "tool instance {tool_instance} of agent {agent} isn't in instance {id}"
            ));
        };
        let Some(EntityMetadata::ToolInstance(found)) = stored
            .iter()
            .find(|entity| entity.id == *tool_instance)
            .map(|entity| &entity.metadata)
        else {
            return not_found!(format!("tool instance {tool_instance} no longer exists"));
        };
        tools.push(InstancedToolSpec {
            tool: tool.clone(),
            spec: found.spec.clone(),
        });
    }
    Ok(InstancedAgentToolset {
        agent: instanced,
        tools,
    })
}

/// One entity recorded in an instance snapshot and the groups it's stored in now
#[derive(Debug, Clone, Copy)]
pub struct SnapshotNode<'a> {
    /// The id of the snapshotted entity
    pub id: Uuid,
    /// The name of the snapshotted entity, used in error messages
    pub name: &'a str,
    /// The groups the snapshot recorded for this entity
    pub recorded: &'a [String],
    /// The groups this entity is stored in now
    pub stored: &'a [String],
}

/// An edge between two snapshotted entities that must stay contained
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotEdge {
    /// The entity the edge starts at
    pub source: Uuid,
    /// The entity the edge points to, whose groups must cover the source's
    pub target: Uuid,
}

/// Get the groups in a list that a user can see
///
/// # Arguments
///
/// * `user` - The user the groups are shown to
/// * `groups` - The groups to filter
fn visible(user: &User, groups: &[String]) -> Vec<String> {
    // admins see every group and other users see the groups they're in
    groups
        .iter()
        .filter(|group| user.is_admin() || user.groups.contains(*group))
        .cloned()
        .collect()
}

/// Check a submitted instance's snapshot groups
///
/// Every node's recorded groups must equal its stored groups (in any order), and every edge's
/// target must record all of its source's groups. Refusals name only the groups the submitter
/// can see, and name none when the problem is only in groups they can't see.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `nodes` - The snapshotted entities and their stored groups
/// * `edges` - The edges between snapshotted entities
pub fn validate_snapshot_groups(
    user: &User,
    nodes: &[SnapshotNode],
    edges: &[SnapshotEdge],
) -> Result<(), ApiError> {
    // make sure each snapshot recorded exactly the stored groups
    for node in nodes {
        // a group list is equal when each side covers the other
        if !helpers::is_superset(node.recorded, node.stored)
            || !helpers::is_superset(node.stored, node.recorded)
        {
            // name only the groups the submitter can see on either side
            let (recorded, stored) = (visible(user, node.recorded), visible(user, node.stored));
            // a mismatch only in hidden groups names no group at all
            if helpers::is_superset(&recorded, &stored) && helpers::is_superset(&stored, &recorded)
            {
                return bad!(format!(
                    "the snapshot of `{}` doesn't record the groups it is in",
                    node.name
                ));
            }
            return bad!(format!(
                "the snapshot of `{}` records groups {recorded:?} but it is in {stored:?}",
                node.name
            ));
        }
    }
    // index the nodes by id, so each edge finds its ends without a scan
    let index = nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<HashMap<Uuid, &SnapshotNode>>();
    // make sure every edge is contained
    for edge in edges {
        // find both ends of this edge
        let (Some(source), Some(target)) = (index.get(&edge.source), index.get(&edge.target))
        else {
            return bad!(format!(
                "the snapshot edge {} -> {} names an entity that isn't in the snapshot",
                edge.source, edge.target
            ));
        };
        // get the source groups the target is missing
        let missing = helpers::missing_groups(source.recorded, target.recorded);
        // a target missing any of them breaks containment
        if !missing.is_empty() {
            // name only the missing groups the submitter can see
            let shown = visible(user, &missing);
            if shown.is_empty() {
                return bad!(format!(
                    "`{}` is missing groups of `{}`, which references it",
                    target.name, source.name
                ));
            }
            return bad!(format!(
                "`{}` is missing groups {shown:?} of `{}`, which references it",
                target.name, source.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use chrono::Utc;
    use std::collections::{HashMap, HashSet};
    use uuid::Uuid;

    use super::{
        SnapshotEdge, SnapshotNode, check_instance_update, check_references, check_size,
        intrinsic_tags, snapshot_edges, update_meta, validate_snapshot_groups,
    };
    use crate::models::backends::groups::tests::user;
    use crate::models::entities::workflows::resolve::test_graph::{Graph, MINERVA, agentic};
    use crate::models::{
        AgentConfig, AssociationKind, CrawlLimits, Entity, EntityKinds, EntityMetadata,
        EntityMetadataUpdateForm, EntityUpdateForm, InstancedAgent, InstancedTool, TagMap,
        ToolKind, ToolSetInclusion, User, UserRole, WorkflowCatalogEntity, WorkflowInstanceEntity,
        WorkflowInstanceStatus, WorkflowInstanceTrigger, WorkflowLimits,
    };

    /// An admin, who sees every group
    fn admin() -> User {
        user("europa", UserRole::Admin, &[])
    }

    /// Build an entity of a kind with a name and default metadata
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity
    /// * `name` - The entity's name
    fn entity(kind: EntityKinds, name: &str) -> Entity {
        Entity {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            kind,
            metadata: EntityMetadata::Other,
            description: None,
            submitter: "io".to_owned(),
            groups: vec!["minerva".to_owned()],
            tags: TagMap::default(),
            image: None,
            created: Utc::now(),
        }
    }

    /// An invalid workflow instance with two agents and no tools
    fn instance() -> WorkflowInstanceEntity {
        // two agents with default configs
        let agent = |name: &str| InstancedAgent {
            agent: Uuid::new_v4(),
            name: name.to_owned(),
            groups: vec!["minerva".to_owned()],
            config: AgentConfig::default(),
            config_hash: "v1:aa".to_owned(),
            harness: None,
            tools: Vec::new(),
            tool_sets: Vec::new(),
            bindings: Vec::new(),
            sub_agents: Vec::new(),
            toolset_digest: "v1:bb".to_owned(),
        };
        WorkflowInstanceEntity {
            workflow: Uuid::new_v4(),
            workflow_groups: vec!["minerva".to_owned()],
            revision: 3,
            previous: None,
            status: WorkflowInstanceStatus::Invalid {
                errors: vec!["no harness".to_owned()],
            },
            warnings: Vec::new(),
            trigger: WorkflowInstanceTrigger::Manual,
            triggered_by: "io".to_owned(),
            entry_agent: None,
            limits: WorkflowLimits::default(),
            default_models: Vec::new(),
            agents: vec![agent("triage"), agent("reverser")],
            tools: Vec::new(),
            digest: "v1:cc".to_owned(),
        }
    }

    /// A workflow instance is tagged with its workflow, revision, digest, status, and agents,
    /// and keeps the followed roots it already carries
    #[test]
    fn instance_intrinsic_tags() {
        // an instance that was tagged with one followed tool and one followed toolset
        let instance = instance();
        let (tool, tool_set) = (Uuid::new_v4(), Uuid::new_v4());
        let mut stored = TagMap::default();
        for (key, value) in [
            ("InstanceFollowsTool", tool),
            ("InstanceFollowsToolSet", tool_set),
        ] {
            stored
                .entry(key.to_owned())
                .or_default()
                .insert(value.to_string(), HashSet::from(["minerva".to_owned()]));
        }
        stored
            .entry("Owner".to_owned())
            .or_default()
            .insert("io".to_owned(), HashSet::from(["minerva".to_owned()]));
        let mut tags: HashMap<String, HashSet<String>> = HashMap::default();
        intrinsic_tags(&instance, &stored, &mut tags);
        // every intrinsic tag is set, and a user tag isn't
        let one = |value: String| HashSet::from([value]);
        assert_eq!(tags["WorkflowId"], one(instance.workflow.to_string()));
        assert_eq!(tags["WorkflowRevision"], one("3".to_owned()));
        assert_eq!(tags["WorkflowDigest"], one("v1:cc".to_owned()));
        assert_eq!(tags["WorkflowStatus"], one("Invalid".to_owned()));
        assert_eq!(
            tags["InstanceAgent"],
            instance
                .agents
                .iter()
                .map(|agent| agent.agent.to_string())
                .collect::<HashSet<String>>()
        );
        assert_eq!(tags["InstanceFollowsTool"], one(tool.to_string()));
        assert_eq!(tags["InstanceFollowsToolSet"], one(tool_set.to_string()));
        assert!(!tags.contains_key("Owner"));
    }

    /// An update sets or clears the entry agent only among the agents it leaves the workflow
    /// with, replaces the settings it sends, and marks the workflow modified
    #[test]
    fn workflow_update_meta() {
        // a workflow with one agent
        let first = entity(EntityKinds::Agent, "first");
        let second = Uuid::new_v4();
        let mut workflow = WorkflowCatalogEntity {
            entry_agent: Some(first.id),
            agents: vec![first.clone()],
            modified: Utc::now() - chrono::Duration::seconds(5),
            ..WorkflowCatalogEntity::default()
        };
        let before = workflow.modified;
        // an entry agent outside the workflow is refused, and changes nothing
        let mut form = EntityMetadataUpdateForm {
            workflow_entry_agent: Some(second),
            ..EntityMetadataUpdateForm::default()
        };
        let err = update_meta("triage", &mut workflow, &mut form).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            err.msg().unwrap_or_default(),
            format!("entry agent {second} is not one of workflow triage's agents")
        );
        assert_eq!(workflow.entry_agent, Some(first.id));
        // one the same update adds is accepted, with the settings it sends
        let mut form = EntityMetadataUpdateForm {
            workflow_entry_agent: Some(second),
            add_workflow_agents: vec![second],
            workflow_limits: Some(r#"{"max_alive_sub_agents": 3}"#.to_owned()),
            workflow_default_models: Some(r#"[{"model_id": "claude"}]"#.to_owned()),
            workflow_auto_instance: Some(false),
            ..EntityMetadataUpdateForm::default()
        };
        update_meta("triage", &mut workflow, &mut form).unwrap();
        assert_eq!(workflow.entry_agent, Some(second));
        assert_eq!(workflow.limits.max_alive_sub_agents, Some(3));
        assert_eq!(workflow.default_models[0].model_id, "claude");
        assert!(!workflow.auto_instance);
        assert!(workflow.modified > before);
        // an agent the same update removes can't become the entry agent
        let mut form = EntityMetadataUpdateForm {
            workflow_entry_agent: Some(first.id),
            remove_workflow_agents: vec![first.id],
            ..EntityMetadataUpdateForm::default()
        };
        assert!(update_meta("triage", &mut workflow, &mut form).is_err());
        // an empty list clears the default models, and clearing the entry agent unsets it
        let mut form = EntityMetadataUpdateForm {
            workflow_default_models: Some("[]".to_owned()),
            clear_workflow_entry_agent: Some(true),
            ..EntityMetadataUpdateForm::default()
        };
        update_meta("triage", &mut workflow, &mut form).unwrap();
        assert_eq!(workflow.default_models.len(), 0);
        assert_eq!(workflow.entry_agent, None);
        // an update without the list leaves it alone
        workflow.default_models = vec![crate::models::ModelMatcher::default()];
        let mut form = EntityMetadataUpdateForm::default();
        update_meta("triage", &mut workflow, &mut form).unwrap();
        assert_eq!(workflow.default_models.len(), 1);
        // a list that isn't JSON is refused
        let mut form = EntityMetadataUpdateForm {
            workflow_default_models: Some("nope".to_owned()),
            ..EntityMetadataUpdateForm::default()
        };
        assert!(update_meta("triage", &mut workflow, &mut form).is_err());
    }

    /// An instance larger than the crawl limits allow is refused before anything is read, and
    /// lower configured limits apply
    #[test]
    fn submitted_size_limits() {
        // an instance within every default limit passes
        let limits = CrawlLimits::default();
        let base = instance();
        assert!(check_size(&base, &limits).is_ok());
        // one agent past the agent limit is refused, naming the limit
        let mut agents = base.clone();
        agents.agents = (0..=limits.max_agents_per_workflow)
            .map(|_| {
                let mut agent = base.agents[0].clone();
                agent.agent = Uuid::new_v4();
                agent
            })
            .collect();
        let err = check_size(&agents, &limits).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(
            err.msg()
                .unwrap_or_default()
                .contains("max_agents_per_workflow (32)")
        );
        // one tool past the most a resolved instance can list is refused
        let mut tools = base.clone();
        tools.tools = (0..=limits.max_instance_tools())
            .map(|index| InstancedTool {
                tool: Uuid::new_v4(),
                tool_instance: Uuid::new_v4(),
                name: format!("tool-{index}"),
                groups: Vec::new(),
                kind: ToolKind::Agentic,
                version: "1.0.0".to_owned(),
                spec_hash: String::new(),
                function_name: None,
                pinned: false,
                dependency: false,
            })
            .collect();
        assert_eq!(
            check_size(&tools, &limits).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        // an agent listing as many tools as an over-limit agent resolves to passes, so its
        // invalid instance can be stored, and one more is refused
        let listed = 2 * limits.max_tools_per_agent;
        let mut agent_tools = base.clone();
        agent_tools.agents[0].tools = (0..listed).map(|_| Uuid::new_v4()).collect();
        assert!(check_size(&agent_tools, &limits).is_ok());
        agent_tools.agents[0].tools.push(Uuid::new_v4());
        let err = check_size(&agent_tools, &limits).unwrap_err();
        assert!(
            err.msg()
                .unwrap_or_default()
                .contains("max_tools_per_agent (128)"),
            "{err:?}"
        );
        // so do toolset instances: as many as a resolved agent's edges can name pass, one more
        // is refused
        let mut agent_sets = base.clone();
        agent_sets.agents[0].tool_sets = (0..listed).map(|_| Uuid::new_v4()).collect();
        assert!(check_size(&agent_sets, &limits).is_ok());
        agent_sets.agents[0].tool_sets.push(Uuid::new_v4());
        assert!(check_size(&agent_sets, &limits).is_err());
        // the whole instance may list every agent's listed tools and harness, each with one
        // dependency
        assert_eq!(limits.max_instance_tools(), 32 * (listed + 1) * 2);
        // an agent with too many sub-agents is refused
        let mut agent_subs = base.clone();
        agent_subs.agents[0].sub_agents = (0..=limits.max_agents_per_workflow)
            .map(|_| Uuid::new_v4())
            .collect();
        let err = check_size(&agent_subs, &limits).unwrap_err();
        assert!(err.msg().unwrap_or_default().contains("triage"));
        // a lower configured agent limit refuses the two agent instance
        let lower = CrawlLimits {
            max_agents_per_workflow: 1,
            ..limits
        };
        let err = check_size(&base, &lower).unwrap_err();
        assert!(
            err.msg()
                .unwrap_or_default()
                .contains("max_agents_per_workflow (1)")
        );
    }

    /// Checking snapshot groups at the largest size a submission may have stays fast, since
    /// each edge finds its ends by id
    #[test]
    fn snapshot_groups_at_size_limits_is_fast() {
        // a workflow, the most agents, and the most tools, each agent using the most tools
        let limits = CrawlLimits::default();
        let (max_agents, max_tools, max_agent_tools) = (
            limits.max_agents_per_workflow,
            limits.max_instance_tools(),
            limits.max_tools_per_agent,
        );
        let groups = vec!["minerva".to_owned()];
        let workflow = Uuid::new_v4();
        let agents = (0..max_agents)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<Uuid>>();
        let tools = (0..max_tools)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<Uuid>>();
        let mut nodes = vec![SnapshotNode {
            id: workflow,
            name: "wf",
            recorded: &groups,
            stored: &groups,
        }];
        nodes.extend(agents.iter().chain(&tools).map(|id| SnapshotNode {
            id: *id,
            name: "node",
            recorded: &groups,
            stored: &groups,
        }));
        let mut edges = Vec::new();
        for (index, agent) in agents.iter().enumerate() {
            edges.push(SnapshotEdge {
                source: workflow,
                target: *agent,
            });
            let start = (index * 7) % (tools.len() - max_agent_tools);
            edges.extend(
                tools[start..start + max_agent_tools]
                    .iter()
                    .map(|tool| SnapshotEdge {
                        source: *agent,
                        target: *tool,
                    }),
            );
        }
        // the whole snapshot is checked quickly
        let start = std::time::Instant::now();
        validate_snapshot_groups(&member(), &nodes, &edges).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(1), "{elapsed:?}");
    }

    /// Checking an instance's internal references at the largest size a submission may have
    /// stays fast
    #[test]
    fn internal_references_at_size_limits_are_fast() {
        // the most tools, and the most agents, each given the most tools and delegating to the
        // next agent
        let limits = CrawlLimits::default();
        let (max_agents, max_tools, max_agent_tools) = (
            limits.max_agents_per_workflow,
            limits.max_instance_tools(),
            limits.max_tools_per_agent,
        );
        let tools = (0..max_tools)
            .map(|index| InstancedTool {
                tool: Uuid::new_v4(),
                tool_instance: Uuid::new_v4(),
                name: format!("tool-{index}"),
                groups: Vec::new(),
                kind: ToolKind::Agentic,
                version: "1.0.0".to_owned(),
                spec_hash: format!("v1:{index:064x}"),
                function_name: None,
                pinned: false,
                dependency: false,
            })
            .collect::<Vec<InstancedTool>>();
        let mut instance = instance();
        let template = instance.agents[0].clone();
        instance.agents = (0..max_agents)
            .map(|index| {
                let start = (index * 7) % (tools.len() - max_agent_tools);
                let mut agent = template.clone();
                agent.agent = Uuid::new_v4();
                agent.tools = tools[start..start + max_agent_tools]
                    .iter()
                    .map(|tool| tool.tool_instance)
                    .collect();
                agent.toolset_digest = agent.compute_toolset_digest(&tools).unwrap();
                agent
            })
            .collect();
        for index in 1..instance.agents.len() {
            let next = instance.agents[index].agent;
            instance.agents[index - 1].sub_agents.push(next);
        }
        instance.tools = tools;
        // every reference is checked quickly, leaving out the toolset digests built above
        let start = std::time::Instant::now();
        check_references(&instance).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "{elapsed:?}"
        );
    }

    /// A workflow instance only takes a new name or description
    #[test]
    fn instance_update_limits() {
        // a name and description are accepted
        let form = EntityUpdateForm {
            name: Some("renamed".to_owned()),
            description: Some("first build".to_owned()),
            ..EntityUpdateForm::default()
        };
        assert!(check_instance_update(&form, false).is_ok());
        // any metadata field is refused, naming it
        let mut form = EntityUpdateForm::default();
        form.metadata
            .sent
            .push("metadata[workflow_limits]".to_owned());
        let err = check_instance_update(&form, false).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.msg().unwrap_or_default().contains("workflow_limits"));
        // so is an image
        assert!(check_instance_update(&EntityUpdateForm::default(), true).is_err());
    }

    /// A user named after io in groups minerva and mars, who can't see any other group
    fn member() -> User {
        user("io", UserRole::User, &["minerva", "mars"])
    }

    /// Build a list of owned group names from string slices
    ///
    /// # Arguments
    ///
    /// * `groups` - The group names to own
    fn groups(groups: &[&str]) -> Vec<String> {
        groups.iter().map(|group| (*group).to_string()).collect()
    }

    /// A snapshot that matches the stored groups and is contained passes
    #[test]
    fn snapshot_groups_match_and_contained() {
        // a workflow in minerva and an agent in minerva and mars, recorded in any order
        let (workflow, agent) = (Uuid::new_v4(), Uuid::new_v4());
        let (wf_groups, agent_groups, agent_stored) = (
            groups(&["minerva"]),
            groups(&["mars", "minerva"]),
            groups(&["minerva", "mars"]),
        );
        let nodes = [
            SnapshotNode {
                id: workflow,
                name: "wf",
                recorded: &wf_groups,
                stored: &wf_groups,
            },
            SnapshotNode {
                id: agent,
                name: "agent",
                recorded: &agent_groups,
                stored: &agent_stored,
            },
        ];
        // the workflow orchestrates the agent
        let edges = [SnapshotEdge {
            source: workflow,
            target: agent,
        }];
        assert!(validate_snapshot_groups(&member(), &nodes, &edges).is_ok());
    }

    /// A snapshot whose recorded groups differ from the stored ones is a 400
    #[test]
    fn snapshot_groups_must_equal_stored() {
        // an agent recorded in minerva but stored in minerva and mars
        let (recorded, stored) = (groups(&["minerva"]), groups(&["minerva", "mars"]));
        let nodes = [SnapshotNode {
            id: Uuid::new_v4(),
            name: "agent",
            recorded: &recorded,
            stored: &stored,
        }];
        // the snapshot is refused
        let err = validate_snapshot_groups(&member(), &nodes, &[]).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.msg().unwrap_or_default().contains("`agent`"));
    }

    /// An edge whose target misses some of its source's groups is a 400 naming them
    #[test]
    fn snapshot_groups_must_be_contained() {
        // a workflow in minerva and mars and an agent in minerva only
        let (workflow, agent) = (Uuid::new_v4(), Uuid::new_v4());
        let (wf_groups, agent_groups) = (groups(&["minerva", "mars"]), groups(&["minerva"]));
        let nodes = [
            SnapshotNode {
                id: workflow,
                name: "wf",
                recorded: &wf_groups,
                stored: &wf_groups,
            },
            SnapshotNode {
                id: agent,
                name: "agent",
                recorded: &agent_groups,
                stored: &agent_groups,
            },
        ];
        // the workflow orchestrates the agent
        let edges = [SnapshotEdge {
            source: workflow,
            target: agent,
        }];
        let err = validate_snapshot_groups(&member(), &nodes, &edges).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            err.msg().unwrap_or_default(),
            "`agent` is missing groups [\"mars\"] of `wf`, which references it"
        );
        // an edge to an entity outside the snapshot is refused too
        let dangling = [SnapshotEdge {
            source: workflow,
            target: Uuid::new_v4(),
        }];
        let err = validate_snapshot_groups(&member(), &nodes, &dangling).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    /// Refusals never name a group the submitter can't see
    #[test]
    fn snapshot_groups_hide_unseen_groups() {
        // an agent stored in minerva and the hidden group vulcan, recorded in minerva only
        let (recorded, stored) = (groups(&["minerva"]), groups(&["minerva", "vulcan"]));
        let nodes = [SnapshotNode {
            id: Uuid::new_v4(),
            name: "agent",
            recorded: &recorded,
            stored: &stored,
        }];
        // the snapshot is refused without naming the hidden group
        let err = validate_snapshot_groups(&member(), &nodes, &[]).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        let msg = err.msg().unwrap_or_default();
        assert!(!msg.contains("vulcan"), "{msg}");
        assert_eq!(
            msg,
            "the snapshot of `agent` doesn't record the groups it is in"
        );
        // a visible mismatch names only the visible groups
        let (recorded, stored) = (groups(&["mars"]), groups(&["minerva", "vulcan"]));
        let nodes = [SnapshotNode {
            id: Uuid::new_v4(),
            name: "agent",
            recorded: &recorded,
            stored: &stored,
        }];
        let err = validate_snapshot_groups(&member(), &nodes, &[]).unwrap_err();
        assert_eq!(
            err.msg().unwrap_or_default(),
            "the snapshot of `agent` records groups [\"mars\"] but it is in [\"minerva\"]"
        );
        // an edge whose target misses a hidden group of its source names no group
        let (workflow, agent) = (Uuid::new_v4(), Uuid::new_v4());
        let (wf_groups, agent_groups) = (groups(&["minerva", "vulcan"]), groups(&["minerva"]));
        let nodes = [
            SnapshotNode {
                id: workflow,
                name: "wf",
                recorded: &wf_groups,
                stored: &wf_groups,
            },
            SnapshotNode {
                id: agent,
                name: "agent",
                recorded: &agent_groups,
                stored: &agent_groups,
            },
        ];
        let edges = [SnapshotEdge {
            source: workflow,
            target: agent,
        }];
        let err = validate_snapshot_groups(&member(), &nodes, &edges).unwrap_err();
        assert_eq!(
            err.msg().unwrap_or_default(),
            "`agent` is missing groups of `wf`, which references it"
        );
        // an admin sees every group
        let admin = user("root", UserRole::Admin, &[]);
        let err = validate_snapshot_groups(&admin, &nodes, &edges).unwrap_err();
        assert_eq!(
            err.msg().unwrap_or_default(),
            "`agent` is missing groups [\"vulcan\"] of `wf`, which references it"
        );
    }

    /// Every instance the resolver builds, even one invalid because some target misses its
    /// source's groups, fits the submission bounds and passes the snapshot group checks
    #[tokio::test]
    async fn resolved_instances_are_submittable() {
        use crate::models::entities::workflows::resolve::tests::uncontained_graphs;
        use crate::models::entities::workflows::resolve_workflow;
        // an over-limit agent, an agent at the toolset edge limit, and uncontained targets
        let limits = CrawlLimits::default();
        let mut graphs = uncontained_graphs()
            .into_iter()
            .map(|(g, wf, _)| (g, wf))
            .collect::<Vec<_>>();
        let mut g = Graph::default();
        let lead = g.agent("lead", &[MINERVA]);
        for index in 0..=limits.max_tools_per_agent {
            let name = format!("tool-{index}");
            let (root, _) = g.tool(&name, &[MINERVA], agentic(&name.replace('-', "_")));
            g.edge(AssociationKind::Calls, lead, root);
        }
        let wf = g.workflow("Tooled", &[MINERVA], &[lead], Some(lead));
        graphs.push((g, wf));
        let mut g = Graph::default();
        let (_, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
        let lead = g.agent("lead", &[MINERVA]);
        for index in 0..=limits.max_tools_per_agent {
            let ts = g.tool_set(&format!("kit-{index}"), &[MINERVA], false, None);
            g.tool_set_instance(ts, &[(scan, ToolSetInclusion::Selector)], true);
            g.edge(AssociationKind::Calls, lead, ts);
        }
        let wf = g.workflow("Kits", &[MINERVA], &[lead], Some(lead));
        graphs.push((g, wf));
        for (g, wf) in graphs {
            // resolve the graph as an admin would
            let resolved = resolve_workflow(&g.reader, wf, &limits).await.unwrap();
            let instance = &resolved.instance;
            assert!(
                check_size(instance, &limits).is_ok(),
                "{:?}",
                instance.status
            );
            assert!(check_references(instance).is_ok(), "{:?}", instance.status);
            // every snapshot records its stored groups, and toolset instances stand in for
            // their own record
            let workflow = g.reader.entity(wf).unwrap();
            let mut nodes = vec![SnapshotNode {
                id: wf,
                name: &workflow.name,
                recorded: &instance.workflow_groups,
                stored: &workflow.groups,
            }];
            nodes.extend(instance.agents.iter().map(|agent| SnapshotNode {
                id: agent.agent,
                name: &agent.name,
                recorded: &agent.groups,
                stored: &g.reader.entity(agent.agent).unwrap().groups,
            }));
            nodes.extend(instance.tools.iter().map(|tool| SnapshotNode {
                id: tool.tool_instance,
                name: &tool.name,
                recorded: &tool.groups,
                stored: &g.reader.entity(tool.tool_instance).unwrap().groups,
            }));
            let sets = instance
                .agents
                .iter()
                .flat_map(|agent| agent.tool_sets.iter())
                .map(|id| g.reader.entity(*id).unwrap())
                .collect::<Vec<_>>();
            nodes.extend(sets.iter().map(|set| SnapshotNode {
                id: set.id,
                name: &set.name,
                recorded: &set.groups,
                stored: &set.groups,
            }));
            let edges = snapshot_edges(wf, instance);
            let checked = validate_snapshot_groups(&admin(), &nodes, &edges);
            assert!(checked.is_ok(), "{checked:?} for {:?}", instance.status);
        }
    }
}
