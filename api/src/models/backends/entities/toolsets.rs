//! Toolset checks, `Includes` edges, and submitted toolset instances
//!
//! A toolset and the tools it includes are checked together before anything is written, and
//! the included tools are filled back in from its `Includes` edges when it's read. A toolset
//! instance is never built here: a submitted one is validated (permissions, members, snapshot
//! groups, digest, and revision) and stored as it was sent, under its toolset's lock so two
//! submissions of one revision can't both succeed.

use chrono::{SubsecRound, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use tracing::instrument;
use uuid::Uuid;

use super::agents::{self, GroupChange};
use super::tools;
use super::workflows::{self, SnapshotEdge, SnapshotNode, Snapshotted, validate_snapshot_groups};
use crate::models::backends::associations::{self as edges, EdgeField, EdgeRequest};
use crate::models::backends::db;
use crate::models::entities::toolsets::{
    TOOL_SET_FOLLOWS_TOOL_TAG, TOOL_SET_ID_TAG, TOOL_SET_SELECTOR_TAG, selector_from_fields,
};
use crate::models::{
    AssociationKind, AssociationTarget, AssociationTargetColumn, Directionality, Entity,
    EntityForm, EntityKinds, EntityMetadata, EntityMetadataUpdateForm, EntityResponse,
    EntityUpdateForm, Event, Group, GroupAllowAction, ListableAssociation, TagMap, ToolSetEntity,
    ToolSetEntityRequest, ToolSetInstanceEntity, ToolSetInstanceEntityRequest, ToolSetLimits,
    ToolSpec, User, WorkflowGraphChange,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, conflict, internal_err, not_found, tag, update};

/// The tag keys Thorium sets on every toolset instance, which its users can't set themselves
pub(crate) const INTRINSIC_TAG_KEYS: [&str; 6] = [
    "ToolSetId",
    "ToolSetRevision",
    "ToolSetDigest",
    "ToolSetStatus",
    "ToolSetFollowsTool",
    "ToolSetSelector",
];

/// How long a toolset instance submission waits for another write to its toolset to finish
const INSTANCE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The field naming the tools a new toolset includes
const TOOLS_FIELD: EdgeField = EdgeField {
    name: "toolset_tools",
    kinds: &[EntityKinds::Tool, EntityKinds::ToolInstance],
    describe: "a tool or tool instance",
};

/// The update field naming the tools to add to a toolset
const ADD_TOOLS_FIELD: EdgeField = EdgeField {
    name: "add_toolset_tools",
    ..TOOLS_FIELD
};

/// The edge kinds a toolset's own edges use
const TOOLSET_EDGES: [AssociationKind; 1] = [AssociationKind::Includes];

/// Build the `Includes` edge requests of a toolset's tool field
///
/// # Arguments
///
/// * `field` - The field the tools came from
/// * `tools` - The tools and tool instances to include
fn include_requests(field: EdgeField, tools: &[Uuid]) -> Vec<EdgeRequest> {
    // one request per named tool, tagged with its field
    tools
        .iter()
        .map(|id| EdgeRequest {
            kind: AssociationKind::Includes,
            target: *id,
            field: Some(field),
        })
        .collect()
}

/// Get a toolset's metadata from its entity
///
/// # Arguments
///
/// * `entity` - The toolset entity
fn tool_set_meta(entity: &Entity) -> Result<&ToolSetEntity, ApiError> {
    // only toolsets have toolset metadata
    match &entity.metadata {
        EntityMetadata::ToolSet(tool_set) => Ok(tool_set),
        _ => internal_err!(format!("{} is not a toolset", entity.id)),
    }
}

/// Create a toolset and its `Includes` edges from a submitted form
///
/// The settings and every edge are checked before anything is written; the toolset's rows are
/// written before its edges, so a failed write only ever leaves a toolset without edges behind.
///
/// # Arguments
///
/// * `user` - The user creating the toolset
/// * `id` - The id of the new toolset
/// * `form` - The submitted form
/// * `shared` - Shared Thorium objects
#[instrument(name = "toolsets::create", skip_all, fields(id = id.to_string()), err(Debug))]
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
    // take the toolset's fields from the form
    let req = ToolSetEntityRequest::from_form(&mut form.metadata)?;
    // build the toolset, which is first modified when it's created, at the millisecond
    // precision its creation time is stored with
    let now = Utc::now().trunc_subsecs(3);
    let entity = Entity {
        id,
        name: name.clone(),
        kind: EntityKinds::ToolSet,
        metadata: EntityMetadata::ToolSet(ToolSetEntity {
            read_only: req.read_only.unwrap_or(false),
            selector: req.selector,
            auto_instance: req.auto_instance.unwrap_or(true),
            tools: Vec::new(),
            modified: now,
            latest_instance: None,
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
    let requested = include_requests(TOOLS_FIELD, &req.tools);
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
    let checked = agents::check_latest_and_pinned(
        &format!("toolset `{name}`"),
        AssociationKind::Includes,
        &[],
        &plan.edges,
    )
    .and_then(|()| edges::check_edge_caps(&entity, &HashSet::new(), &plan, shared));
    plan.release_on_error(checked, shared).await?;
    // write the toolset, then its edges
    let inserted = db::entities::insert(user, &entity, form.tags, shared).await;
    plan.release_on_error(inserted, shared).await?;
    helpers::heap_future(|| edges::write_graph_edges(user, &entity, plan, shared)).await?;
    // queue the new toolset's change
    let change = WorkflowGraphChange::ToolSetChanged { tool_set: id };
    Event::queue_workflow_graph(user, &entity.groups, change, shared).await?;
    Ok(EntityResponse::new(id, name))
}

/// Queue a user's request for a new instance of a toolset
///
/// Only a user who can edit the toolset in every one of its groups may ask for one. Nothing is
/// built here; the request is only queued.
///
/// # Arguments
///
/// * `user` - The user asking for the instance
/// * `id` - The id of the toolset
/// * `shared` - Shared Thorium objects
#[instrument(name = "toolsets::request_instance", skip(user, shared), err(Debug))]
pub(crate) async fn request_instance(
    user: &User,
    id: Uuid,
    shared: &Shared,
) -> Result<(), ApiError> {
    // the toolset must exist and be visible
    let tool_set = Entity::get(user, id, shared).await?;
    // anything but a toolset isn't a toolset to this route
    if tool_set.kind != EntityKinds::ToolSet {
        return not_found!(format!("ToolSet {id} not found"));
    }
    // the user must be able to edit the toolset in every one of its groups
    let groups = db::entities::all_groups(tool_set.id, shared).await?;
    Group::authorize_graph(
        user,
        &groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "instance",
        &tool_set.name,
        shared,
    )
    .await?;
    // queue the request in every group of the toolset
    let change = WorkflowGraphChange::ManualToolSetInstance { tool_set: id };
    Event::queue_workflow_graph(user, &groups, change, shared).await
}

/// Apply the settings fields of an update to a toolset
///
/// The selector's key and value are set together, or the selector is cleared. Every update
/// marks the toolset modified.
///
/// # Arguments
///
/// * `tool_set` - The toolset's metadata
/// * `form` - The metadata update form
pub(crate) fn update_meta(
    tool_set: &mut ToolSetEntity,
    form: &mut EntityMetadataUpdateForm,
) -> Result<(), ApiError> {
    // the selector's two parts are set together
    let selector = selector_from_fields(
        form.toolset_selector_key.take(),
        form.toolset_selector_value.take(),
    )?;
    // a selector is either replaced or cleared
    let clear = form.clear_toolset_selector == Some(true);
    if clear && selector.is_some() {
        return bad!("set a new toolset selector or clear it, not both".to_owned());
    }
    if clear {
        tool_set.selector = None;
    }
    if selector.is_some() {
        tool_set.selector = selector;
    }
    // update the settings that were sent
    update!(tool_set.read_only, form.toolset_read_only);
    update!(tool_set.auto_instance, form.toolset_auto_instance);
    // any edit marks the toolset modified
    tool_set.modified = Utc::now();
    Ok(())
}

/// Check the `Includes` edges an update adds and removes, then write them
///
/// Every added edge is checked before any edge is added or removed, so one bad edge rejects
/// the whole update. Removing a tool the toolset doesn't include changes nothing, and adding
/// one it already includes adds nothing.
///
/// # Arguments
///
/// * `user` - The user updating the toolset
/// * `entity` - The toolset, with its tools filled in
/// * `form` - The metadata update form
/// * `shared` - Shared Thorium objects
#[instrument(name = "toolsets::update_edges", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn update_edges(
    user: &User,
    entity: &mut Entity,
    form: &mut EntityMetadataUpdateForm,
    shared: &Shared,
) -> Result<(), ApiError> {
    // collect the tools to remove and the edges to add
    let removes = form
        .remove_toolset_tools
        .iter()
        .copied()
        .collect::<HashSet<Uuid>>();
    let requested = include_requests(ADD_TOOLS_FIELD, &form.add_toolset_tools);
    // nothing changes without edge fields
    if requested.is_empty() && removes.is_empty() {
        return Ok(());
    }
    // the tools the toolset keeps and already includes
    let tool_set = tool_set_meta(entity)?;
    let kept = tool_set
        .tools
        .iter()
        .filter(|target| !removes.contains(&target.id))
        .collect::<Vec<&Entity>>();
    let existing = tool_set
        .tools
        .iter()
        .map(|target| target.id)
        .collect::<HashSet<Uuid>>();
    // check every added edge before anything is written
    let extend = form.extend_target_groups == Some(true);
    let mut plan = helpers::heap_future(|| {
        edges::check_graph_edges(user, entity, true, &requested, extend, shared)
    })
    .await?;
    let checked = agents::check_latest_and_pinned(
        &format!("toolset `{}`", entity.name),
        AssociationKind::Includes,
        &kept,
        &plan.edges,
    );
    plan.release_on_error(checked, shared).await?;
    plan.edges
        .retain(|edge| !existing.contains(&edge.target.id) || removes.contains(&edge.target.id));
    // the toolset's includes after the update must fit what a resolution reads
    let capped = removes
        .iter()
        .map(|id| (AssociationKind::Includes, *id))
        .collect::<HashSet<(AssociationKind, Uuid)>>();
    let checked = edges::check_edge_caps(entity, &capped, &plan, shared);
    plan.release_on_error(checked, shared).await?;
    // remove the edges named for removal, then add the new ones
    let dropped = remove_includes(user, entity, &removes, shared).await;
    plan.release_on_error(dropped, shared).await?;
    helpers::heap_future(|| edges::write_graph_edges(user, entity, plan, shared)).await
}

/// Remove a toolset's stored `Includes` edges to some tools
///
/// # Arguments
///
/// * `user` - The user updating the toolset
/// * `entity` - The toolset
/// * `removes` - The tools and tool instances whose edges to remove
/// * `shared` - Shared Thorium objects
async fn remove_includes(
    user: &User,
    entity: &Entity,
    removes: &HashSet<Uuid>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find the stored rows of the edges to remove
    let rows = includes(user, entity.id, &entity.groups, shared)
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

/// List a toolset's outgoing `Includes` edges
///
/// # Arguments
///
/// * `user` - The user reading the edges
/// * `tool_set` - The toolset
/// * `groups` - The groups to read the edges in
/// * `shared` - Shared Thorium objects
async fn includes(
    user: &User,
    tool_set: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<ListableAssociation>, ApiError> {
    // list the toolset's outgoing includes
    agents::edges_of(
        user,
        tool_set,
        groups,
        &TOOLSET_EDGES,
        Directionality::To,
        shared,
    )
    .await
}

/// Get every instance of a toolset in some groups, from their `BuiltFrom` edges
///
/// # Arguments
///
/// * `user` - The user reading the instances
/// * `tool_set` - The id of the toolset
/// * `groups` - The groups to read the instances in
/// * `shared` - Shared Thorium objects
pub(crate) async fn instances(
    user: &User,
    tool_set: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // find the instances that point at this toolset
    let built = agents::edges_of(
        user,
        tool_set,
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
    // read the instances, keeping only instances of this toolset
    let mut found = db::entities::get_many(groups, &ids, shared).await?;
    found.retain(|entity| {
        matches!(&entity.metadata, EntityMetadata::ToolSetInstance(instance) if instance.tool_set == tool_set)
    });
    Ok(found)
}

/// Get the revision of a toolset instance entity
///
/// # Arguments
///
/// * `entity` - The toolset instance entity
fn revision(entity: &Entity) -> Option<(u64, bool)> {
    // only toolset instances have a revision
    match &entity.metadata {
        EntityMetadata::ToolSetInstance(instance) => Some((instance.revision, instance.is_ready())),
        _ => None,
    }
}

/// Find a toolset's newest instance and its newest ready instance
///
/// # Arguments
///
/// * `found` - The toolset's instances
fn newest(found: &[Entity]) -> (Option<(&Entity, u64)>, Option<Uuid>) {
    // the newest instance of any status has the highest revision
    let mut newest: Option<(&Entity, u64)> = None;
    let mut ready: Option<(Uuid, u64)> = None;
    for entity in found {
        let Some((rev, is_ready)) = revision(entity) else {
            continue;
        };
        if newest.is_none_or(|(_, best)| rev > best) {
            newest = Some((entity, rev));
        }
        // the newest ready instance is the toolset's latest
        if is_ready && ready.is_none_or(|(_, best)| rev > best) {
            ready = Some((entity.id, rev));
        }
    }
    (newest, ready.map(|(id, _)| id))
}

/// Fill a toolset's included tools and its latest ready instance
///
/// The targets and instances are read in the toolset's visible groups, which they cover.
///
/// # Arguments
///
/// * `user` - The user reading the toolset
/// * `entity` - The toolset
/// * `shared` - Shared Thorium objects
#[instrument(name = "toolsets::populate", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn populate(
    user: &User,
    mut entity: Entity,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // collect the included tools
    let mut ids = Vec::new();
    for assoc in includes(user, entity.id, &entity.groups, shared).await? {
        if let Some(id) = agents::other_entity(&assoc)?
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    let tools = if ids.is_empty() {
        Vec::new()
    } else {
        db::entities::get_many(&entity.groups, &ids, shared).await?
    };
    // find the newest ready instance
    let found = instances(user, entity.id, &entity.groups, shared).await?;
    let (_, latest) = newest(&found);
    // fill the toolset's populated fields
    if let EntityMetadata::ToolSet(tool_set) = &mut entity.metadata {
        tool_set.tools = tools;
        tool_set.latest_instance = latest;
    }
    Ok(entity)
}

/// Add a toolset instance's intrinsic tags
///
/// The toolset, revision, digest, and status make instances queryable; each tool a member
/// follows at latest and the selector let changes to tools find the toolsets they affect. The
/// tools the toolset included at latest are read from its edges when the instance is submitted
/// and stored as tags, so the ones it already carries are kept.
///
/// # Arguments
///
/// * `instance` - The toolset instance
/// * `stored` - The tags the instance already carries
/// * `tags` - The tags to add to
// tag maps use the default hasher everywhere entities are tagged
#[allow(clippy::implicit_hasher)]
pub(crate) fn intrinsic_tags(
    instance: &ToolSetInstanceEntity,
    stored: &TagMap,
    tags: &mut HashMap<String, HashSet<String>>,
) {
    // tag the toolset, revision, digest, and status
    tag!(tags, TOOL_SET_ID_TAG, instance.tool_set.to_string());
    tag!(tags, "ToolSetRevision", instance.revision.to_string());
    tag!(tags, "ToolSetDigest", instance.digest.clone());
    tag!(tags, "ToolSetStatus", instance.status.as_str().to_owned());
    // tag each tool a member follows at latest, so its new versions find the toolset
    for member in &instance.members {
        if member.inclusion.follows_latest() {
            tag!(tags, TOOL_SET_FOLLOWS_TOOL_TAG, member.tool.to_string());
        }
    }
    // keep the included tools the instance was tagged with when it was submitted
    if let Some(values) = stored.get(TOOL_SET_FOLLOWS_TOOL_TAG) {
        for value in values.keys() {
            tag!(tags, TOOL_SET_FOLLOWS_TOOL_TAG, value.clone());
        }
    }
    // tag the selector, so tag changes find the toolset
    if let Some(selector) = &instance.selector {
        tag!(tags, TOOL_SET_SELECTOR_TAG, selector.to_string());
    }
}

/// Find the tools a toolset includes at latest through its own `Includes` edges
///
/// A tool is followed whatever it resolved to, even when it left no member, so its next
/// publish finds the toolset.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `tool_set` - The toolset, in all of its groups
/// * `shared` - Shared Thorium objects
async fn followed_tools(
    user: &User,
    tool_set: &Entity,
    shared: &Shared,
) -> Result<HashMap<String, HashSet<String>>, ApiError> {
    // collect the targets of the toolset's include edges
    let mut targets = Vec::new();
    for assoc in includes(user, tool_set.id, &tool_set.groups, shared).await? {
        if let Some(id) = agents::other_entity(&assoc)?
            && !targets.contains(&id)
        {
            targets.push(id);
        }
    }
    // a target that is a tool root is followed at latest
    let mut tags: HashMap<String, HashSet<String>> = HashMap::default();
    if targets.is_empty() {
        return Ok(tags);
    }
    for row in db::entities::groups_and_kinds(&targets, shared).await? {
        if row.kind == EntityKinds::Tool {
            tag!(tags, TOOL_SET_FOLLOWS_TOOL_TAG, row.id.to_string());
        }
    }
    Ok(tags)
}

/// Check a group change of a toolset
///
/// Every tool the toolset includes must be in each added group, except those the same update
/// removes, and so must every member its instances snapshotted; a group can't be removed while
/// a graph edge from another entity in that group references the toolset or one of its
/// instances.
///
/// # Arguments
///
/// * `user` - The user changing the toolset's groups
/// * `tool_set` - The toolset, in all of its groups
/// * `instances` - The toolset's instances
/// * `add` - The groups being added
/// * `remove` - The groups being removed
/// * `dropped` - The tools whose `Includes` edges the same update removes
/// * `shared` - Shared Thorium objects
#[instrument(name = "toolsets::check_group_change", skip_all, fields(id = tool_set.id.to_string()), err(Debug))]
pub(crate) async fn check_group_change(
    user: &User,
    tool_set: &Entity,
    instances: &[Entity],
    add: &[String],
    remove: &[String],
    dropped: &[Uuid],
    shared: &Shared,
) -> Result<(), ApiError> {
    // the toolset and its instances may be referenced
    let mut referenced = vec![tool_set.id];
    referenced.extend(instances.iter().map(|instance| instance.id));
    // check the toolset's includes and its referrers, except the includes being removed
    let dropped = dropped
        .iter()
        .map(|id| (AssociationKind::Includes, *id))
        .collect::<Vec<(AssociationKind, Uuid)>>();
    let label = format!("toolset `{}`", tool_set.name);
    let change = GroupChange {
        entity: tool_set,
        out_kinds: &TOOLSET_EDGES,
        referenced: &referenced,
        label: &label,
        add,
        remove,
        dropped: &dropped,
    };
    agents::check_contained_group_change(user, &change, shared).await?;
    // every member the instances snapshotted must be in each added group too
    let snapshotted = instances
        .iter()
        .filter_map(|entity| match &entity.metadata {
            EntityMetadata::ToolSetInstance(instance) => Some((entity, instance)),
            _ => None,
        })
        .flat_map(|(entity, instance)| {
            instance.members.iter().map(|member| Snapshotted {
                instance: &entity.name,
                id: member.tool_instance,
                name: member.name.clone(),
            })
        })
        .collect::<Vec<Snapshotted>>();
    workflows::check_snapshots_cover(&label, &snapshotted, add, shared).await
}

/// Refuse the changes a toolset instance never takes
///
/// A toolset instance takes only a new name or description; its metadata never changes, and
/// its groups follow its toolset.
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
            "a toolset instance's metadata can't change, so `{field}` was refused; only its name and description can"
        ));
    }
    // its image can't change either
    if form.clear_image.is_some() || image_uploaded {
        return bad!("a toolset instance only takes a new name or description".to_owned());
    }
    Ok(())
}

/// Drop the groups a reader can't see from a toolset instance's snapshot groups
///
/// The stored snapshot keeps every group, since submissions are checked against all of them;
/// a reader is only shown the groups they can see, as with an entity's own groups.
///
/// # Arguments
///
/// * `entity` - The entity being read
/// * `user` - The user reading it
pub(crate) fn hide_snapshot_groups(entity: &mut Entity, user: &User) {
    // admins see every group
    if user.is_admin() {
        return;
    }
    // only toolset instances record snapshot groups
    if let EntityMetadata::ToolSetInstance(instance) = &mut entity.metadata {
        // keep the toolset's and each member's groups this user is in
        instance
            .tool_set_groups
            .retain(|group| user.groups.contains(group));
        for member in &mut instance.members {
            member.groups.retain(|group| user.groups.contains(group));
        }
    }
}

/// Refuse a submitted toolset instance larger than any instance its toolset can resolve to, or
/// a ready one holding what the resolver would make invalid
///
/// The size is checked before any member is read, while the toolset's lock is held. A
/// resolution lists at most the explicit edges and the selected tools it may consider as
/// members, and leaves out at most those plus the tools tagged in each of the toolset's groups.
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `groups` - How many groups the toolset is in
/// * `limits` - The configured toolset limits
fn check_size(
    instance: &ToolSetInstanceEntity,
    groups: usize,
    limits: ToolSetLimits,
) -> Result<(), ApiError> {
    // bound the members and exclusions by what a resolution can list
    let cap = limits.max_candidates();
    let max_members = cap.saturating_mul(2);
    if instance.members.len() > max_members {
        return bad!(format!(
            "a toolset instance can list at most {max_members} members, not {}",
            instance.members.len()
        ));
    }
    let max_excluded = cap.saturating_mul(groups.saturating_add(2));
    if instance.excluded.len() > max_excluded {
        return bad!(format!(
            "a toolset instance can list at most {max_excluded} left out tools, not {}",
            instance.excluded.len()
        ));
    }
    // a ready instance has at most the member limit, one member per tool, each with its own
    // function name
    if instance.is_ready() {
        let max = limits.max_tools_per_toolset;
        if instance.members.len() > max {
            return bad!(format!(
                "a Ready toolset instance can have at most max_tools_per_toolset ({max}) members, not {}",
                instance.members.len()
            ));
        }
        let mut tools = HashSet::with_capacity(instance.members.len());
        let mut functions = HashSet::with_capacity(instance.members.len());
        for member in &instance.members {
            if !tools.insert(member.tool) {
                return bad!(format!(
                    "a Ready toolset instance can't have two members of tool `{}`",
                    member.tool
                ));
            }
            if !functions.insert(member.function_name.as_str()) {
                return bad!(format!(
                    "a Ready toolset instance can't give two members the function name `{}`",
                    member.function_name
                ));
            }
        }
    }
    Ok(())
}

/// Find the member tool instances of a submitted toolset instance and their stored groups
///
/// A member that doesn't exist or that the submitter can't see is the same `404`, whatever it
/// is, and a visible member that isn't a tool instance is a `400`.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `instance` - The submitted instance
/// * `shared` - Shared Thorium objects
async fn member_groups(
    user: &User,
    instance: &ToolSetInstanceEntity,
    shared: &Shared,
) -> Result<BTreeMap<Uuid, Vec<String>>, ApiError> {
    // read the group and kind of every member's rows at once
    let ids = instance
        .members
        .iter()
        .map(|member| member.tool_instance)
        .collect::<Vec<Uuid>>();
    let rows = db::entities::groups_and_kinds(&ids, shared).await?;
    // collect each member's groups and kind
    let mut groups: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
    let mut kinds: HashMap<Uuid, EntityKinds> = HashMap::default();
    for row in rows {
        kinds.insert(row.id, row.kind);
        groups.entry(row.id).or_default().push(row.group);
    }
    // every member must exist, be visible to the submitter, and be a tool instance
    for id in &ids {
        // a member the submitter can't see answers like a missing one
        let visible = groups.get(id).is_some_and(|stored| {
            user.is_admin() || stored.iter().any(|group| user.groups.contains(group))
        });
        if !visible {
            return not_found!(format!("member tool instance `{id}` doesn't exist"));
        }
        // only tool instances can be members
        if kinds.get(id) != Some(&EntityKinds::ToolInstance) {
            return bad!(format!("member `{id}` is not a tool instance"));
        }
    }
    Ok(groups)
}

/// Check that each member of a submitted toolset instance matches the stored tool instance
///
/// # Arguments
///
/// * `instance` - The submitted instance
/// * `groups` - Every member's stored groups
/// * `shared` - Shared Thorium objects
async fn check_members(
    instance: &ToolSetInstanceEntity,
    groups: &BTreeMap<Uuid, Vec<String>>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // read every member in the groups it's stored in
    let all = groups
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<String>>();
    let all = all.into_iter().collect::<Vec<String>>();
    let ids = groups.keys().copied().collect::<Vec<Uuid>>();
    let stored = if ids.is_empty() {
        Vec::new()
    } else {
        db::entities::get_many(&all, &ids, shared).await?
    };
    // each member's tool and spec hash must match the stored instance
    for member in &instance.members {
        let Some(EntityMetadata::ToolInstance(found)) = stored
            .iter()
            .find(|entity| entity.id == member.tool_instance)
            .map(|entity| &entity.metadata)
        else {
            return not_found!(format!(
                "member tool instance `{}` doesn't exist",
                member.tool_instance
            ));
        };
        if found.tool != member.tool {
            return bad!(format!(
                "member `{}` names tool `{}`, but tool instance `{}` belongs to tool `{}`",
                member.name, member.tool, member.tool_instance, found.tool
            ));
        }
        if found.spec_hash != member.spec_hash {
            return bad!(format!(
                "member `{}` has spec_hash `{}`, but tool instance `{}` has `{}`",
                member.name, member.spec_hash, member.tool_instance, found.spec_hash
            ));
        }
        // the function name and version a member is used by come from the stored instance too
        let function_name = found.spec.function_name().unwrap_or_default();
        if function_name != member.function_name {
            return bad!(format!(
                "member `{}` has function_name `{}`, but tool instance `{}` has `{function_name}`",
                member.name, member.function_name, member.tool_instance
            ));
        }
        let version = found.version.to_string();
        if version != member.version {
            return bad!(format!(
                "member `{}` has version `{}`, but tool instance `{}` has `{version}`",
                member.name, member.version, member.tool_instance
            ));
        }
        // only agentic tools are members, and a ready read only toolset has none with effects
        let ToolSpec::Agentic(agentic) = &found.spec else {
            return bad!(format!(
                "member `{}` isn't an agentic tool, so it can't be a toolset member",
                member.name
            ));
        };
        if instance.read_only && instance.is_ready() && !agentic.capabilities.effects.is_empty() {
            return bad!(format!(
                "member `{}` declares effects, so a Ready read only toolset instance can't hold it",
                member.name
            ));
        }
    }
    Ok(())
}

/// Check a submitted toolset instance's snapshot groups against the stored groups
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `tool_set` - The toolset, in all of its groups
/// * `instance` - The submitted instance
/// * `groups` - Every member's stored groups
fn check_snapshot(
    user: &User,
    tool_set: &Entity,
    instance: &ToolSetInstanceEntity,
    groups: &BTreeMap<Uuid, Vec<String>>,
) -> Result<(), ApiError> {
    // the toolset records its groups, and each member records its own
    let empty = Vec::new();
    let mut nodes = vec![SnapshotNode {
        id: tool_set.id,
        name: &tool_set.name,
        recorded: &instance.tool_set_groups,
        stored: &tool_set.groups,
    }];
    nodes.extend(instance.members.iter().map(|member| SnapshotNode {
        id: member.tool_instance,
        name: &member.name,
        recorded: &member.groups,
        stored: groups.get(&member.tool_instance).unwrap_or(&empty),
    }));
    // every member must cover the toolset's groups
    let edges = instance
        .members
        .iter()
        .map(|member| SnapshotEdge {
            source: tool_set.id,
            target: member.tool_instance,
        })
        .collect::<Vec<SnapshotEdge>>();
    validate_snapshot_groups(user, &nodes, &edges)
}

/// Check that a submitted toolset instance's digest matches its content
///
/// # Arguments
///
/// * `instance` - The submitted instance
fn check_digest(instance: &ToolSetInstanceEntity) -> Result<(), ApiError> {
    // recompute the digest from the instance's typed content
    let computed = match instance.compute_digest() {
        Ok(digest) => digest,
        Err(err) => return internal_err!(format!("failed to hash a toolset instance: {err}")),
    };
    // a digest that doesn't match the content is refused
    if computed != instance.digest {
        return bad!(format!(
            "digest `{}` doesn't match the toolset instance's content, which hashes to `{computed}`",
            instance.digest
        ));
    }
    Ok(())
}

/// Read a toolset for an instance submission
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `id` - The id of the toolset
/// * `shared` - Shared Thorium objects
async fn read_tool_set(user: &User, id: Uuid, shared: &Shared) -> Result<Entity, ApiError> {
    // the toolset must exist and be visible
    let tool_set = Entity::get(user, id, shared).await?;
    // it must actually be a toolset
    if tool_set.kind != EntityKinds::ToolSet {
        return bad!(format!(
            "`{}` is a {}, not a toolset",
            tool_set.name, tool_set.kind
        ));
    }
    Ok(tool_set)
}

/// Create a submitted toolset instance from a form
///
/// The instance is validated but never rebuilt: the submitter must be able to edit the toolset
/// in all of its groups, a non-admin may only name themselves in `triggered_by`, every member
/// must exist, be visible, and match its stored tool instance, the snapshot groups must match
/// the stored ones and stay contained, and the digest must match the content. A submitter who
/// can't edit the toolset is refused before waiting on its lock. Everything after finding the
/// toolset is checked again, and the instance written, under the toolset's lock, so a group
/// change or delete of the toolset that finishes first is always seen.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `id` - The id of the new instance
/// * `form` - The submitted form
/// * `shared` - Shared Thorium objects
#[instrument(name = "toolsets::create_instance", skip_all, fields(id = id.to_string()), err(Debug))]
pub(crate) async fn create_instance(
    user: &User,
    id: Uuid,
    mut form: EntityForm,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // parse the submitted instance, whose own tags can't use the keys Thorium sets on it
    let instance = ToolSetInstanceEntityRequest::parse_form(&mut form.metadata)?;
    Entity::refuse_intrinsic_tag_keys(EntityKinds::ToolSetInstance, form.tags.keys())?;
    // find the toolset
    let mut tool_set = read_tool_set(user, instance.tool_set, shared).await?;
    // refuse a submitter who can't edit the toolset in every group before waiting on its lock
    tool_set.groups = db::entities::all_groups(tool_set.id, shared).await?;
    Group::authorize_graph(
        user,
        &tool_set.groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "edit",
        &tool_set.name,
        shared,
    )
    .await?;
    // check and write the instance while holding the toolset's lock
    let token = lock_for_instance(&tool_set, shared).await?;
    let result = helpers::heap_future(|| {
        locked_create_instance(user, id, tool_set.clone(), instance, form, shared)
    })
    .await;
    // release the lock whether or not the write worked
    tools::unlock(tool_set.id, &token, shared).await?;
    let (resp, update) = result?;
    // a ready instance is what workflows following the toolset resolve to, so they're rebuilt
    if let Some(update) = update {
        db::events::create(&update, shared).await?;
    }
    Ok(resp)
}

/// Check a submitted toolset instance against its toolset, while holding the toolset's lock
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `tool_set` - The toolset, in all of its groups
/// * `instance` - The submitted instance
/// * `shared` - Shared Thorium objects
async fn check_submitted(
    user: &User,
    tool_set: &Entity,
    instance: &mut ToolSetInstanceEntity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // the submitter must be able to edit the toolset in every one of its groups
    Group::authorize_graph(
        user,
        &tool_set.groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "edit",
        &tool_set.name,
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
    // the instance is no larger than any instance the toolset resolves to
    let limits = ToolSetLimits::from(&shared.config.thorium.events);
    check_size(instance, tool_set.groups.len(), limits)?;
    // every member must exist, be visible, and match its stored instance
    let groups = member_groups(user, instance, shared).await?;
    check_members(instance, &groups, shared).await?;
    // the snapshot groups must match the stored groups and stay contained
    check_snapshot(user, tool_set, instance, &groups)?;
    // the digest must match the content
    check_digest(instance)?;
    // store the lists in their canonical order
    instance.sort_lists();
    Ok(())
}

/// Take a toolset's lock for an instance submission, waiting briefly for another write
///
/// # Arguments
///
/// * `tool_set` - The toolset
/// * `shared` - Shared Thorium objects
async fn lock_for_instance(tool_set: &Entity, shared: &Shared) -> Result<String, ApiError> {
    // wait a short time for another write to this toolset
    match db::entities::lock_instances(tool_set.id, &tool_set.name, INSTANCE_LOCK_WAIT, shared)
        .await
    {
        Ok(token) => Ok(token),
        Err(err) if err.status() == axum::http::StatusCode::CONFLICT => conflict!(format!(
            "another instance of `{}` is being created; retry",
            tool_set.name
        )),
        Err(err) => Err(err),
    }
}

/// Check a submitted instance and its revision and write it, while holding its toolset's lock
///
/// A revision is never reused: the next one follows the highest revision any instance of the
/// toolset was created at, even when that instance was deleted since. A ready instance also
/// gets the toolset update event to queue once the lock is released, recorded as caused by the
/// user the instance names; an invalid one leaves workflows on the last ready instance and gets
/// none.
///
/// # Arguments
///
/// * `user` - The user submitting the instance
/// * `id` - The id of the new instance
/// * `tool_set` - The toolset
/// * `instance` - The submitted instance
/// * `form` - The submitted form, for the description and user tags
/// * `shared` - Shared Thorium objects
async fn locked_create_instance(
    user: &User,
    id: Uuid,
    mut tool_set: Entity,
    mut instance: ToolSetInstanceEntity,
    form: EntityForm,
    shared: &Shared,
) -> Result<(EntityResponse, Option<Event>), ApiError> {
    // read the toolset's groups as the writes that finished before the lock left them
    tool_set.groups = db::entities::all_groups(tool_set.id, shared).await?;
    if tool_set.groups.is_empty() {
        return not_found!(format!("toolset `{}` doesn't exist", tool_set.name));
    }
    // check the instance against the toolset as it is now
    helpers::heap_future(|| check_submitted(user, &tool_set, &mut instance, shared)).await?;
    // read the toolset's instances and the highest revision it ever had, as stored now
    let found = instances(user, tool_set.id, &tool_set.groups, shared).await?;
    let (newest, _) = newest(&found);
    let stored = db::entities::get_untagged(&tool_set.groups, tool_set.id, shared).await?;
    let highest = tool_set_meta(&stored)?.highest_revision;
    // the revision must follow the highest one, and the instance supersedes the newest one left
    let newest_revision = newest.map_or(0, |(_, rev)| rev);
    let expected = newest_revision.max(highest) + 1;
    let previous = newest.map(|(entity, _)| entity.id);
    if instance.revision != expected {
        return conflict!(format!(
            "the next instance of toolset `{}` is revision {expected}, not {}",
            tool_set.name, instance.revision
        ));
    }
    // the instance must supersede the newest instance
    if instance.previous != previous {
        return bad!(format!(
            "revision {expected} of toolset `{}` must name {} as its previous instance",
            tool_set.name,
            previous.map_or_else(|| "no instance".to_owned(), |id| format!("`{id}`"))
        ));
    }
    // build the update a ready instance queues, recorded as caused by the user it names
    let update = instance.is_ready().then(|| {
        let change = WorkflowGraphChange::ToolSetUpdated {
            tool_set: tool_set.id,
            tool_set_instance: id,
        };
        let mut update = Event::workflow_graph(user, &tool_set.groups, change);
        update.user.clone_from(&instance.triggered_by);
        update
    });
    // build the instance in every group of its toolset
    let name = ToolSetInstanceEntity::name_for(&tool_set.name, instance.revision);
    let entity = Entity {
        id,
        name: name.clone(),
        kind: EntityKinds::ToolSetInstance,
        metadata: EntityMetadata::ToolSetInstance(instance),
        description: form.description,
        submitter: user.username.clone(),
        groups: tool_set.groups.clone(),
        tags: HashMap::default(),
        image: None,
        created: Utc::now(),
    };
    // tag the instance with the tools its toolset includes at latest, beside the submitter's
    // own tags
    let mut tags = form.tags;
    for (key, values) in followed_tools(user, &tool_set, shared).await? {
        tags.entry(key).or_default().extend(values);
    }
    // record the revision as taken on the toolset, then write the instance and link it to its
    // toolset in every group
    db::entities::set_highest_revision(stored, expected, shared).await?;
    db::entities::insert(user, &entity, tags, shared).await?;
    let source = AssociationTarget::Entity {
        id,
        name: name.clone(),
    };
    let target = AssociationTarget::Entity {
        id: tool_set.id,
        name: tool_set.name.clone(),
    };
    db::associations::create(
        user,
        tool_set.groups.len(),
        AssociationKind::BuiltFrom,
        source,
        &vec![(target, tool_set.groups.clone())],
        None,
        Directionality::To,
        shared,
    )
    .await?;
    Ok((EntityResponse::new(id, name), update))
}
