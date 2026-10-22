//! Backend api support for associations

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use tracing::instrument;
use uuid::Uuid;

use super::db;
use crate::models::backends::db::{CursorCore, ScyllaCursor, ScyllaCursorSupport};
use crate::models::backends::entities::{WideningPlan, tools};
use crate::models::schema::queries::AssociationsListCursor;
use crate::models::schema::{Bucket, CursorLimit, DEFAULT_LIMIT_CAP};
use crate::models::{
    ApiCursor, Association, AssociationDeleteRequest, AssociationKind, AssociationListParams,
    AssociationListRow, AssociationRequest, AssociationTarget, AssociationTargetColumn,
    CrawlLimits, Directionality, Entity, EntityKinds, EntityMetadata, Event, Group,
    GroupAllowAction, ListableAssociation, Repo, Sample, ToolKind, ToolSetLimits, TreeNode, User,
    WorkflowGraphChange,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, internal_err, not_found, serialize, unauthorized};

/// How long an edge write waits in all for its targets' locks, well under how long the lock its
/// source's writer holds lasts
const EDGE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// How long an edge write waits for the lock of a target root that sorts before its source's
/// root before giving way with a `409`
const EDGE_LOCK_YIELD_WAIT: Duration = Duration::from_secs(2);

/// The form field an edge came from and the kinds of entity it may name
#[derive(Debug, Clone, Copy)]
pub(crate) struct EdgeField {
    /// The name of the form field
    pub name: &'static str,
    /// The kinds of entity the field may name
    pub kinds: &'static [EntityKinds],
    /// What the field names, for errors
    pub describe: &'static str,
}

/// A graph edge a request asks to add
#[derive(Debug, Clone, Copy)]
pub(crate) struct EdgeRequest {
    /// The kind of edge
    pub kind: AssociationKind,
    /// The id of the edge's target
    pub target: Uuid,
    /// The form field the edge came from, if it came from an entity form
    pub field: Option<EdgeField>,
}

/// A graph edge a request adds, checked against its source and ready to write
#[derive(Debug)]
pub(crate) struct CheckedEdge {
    /// The kind of edge
    pub kind: AssociationKind,
    /// The edge's target, in every one of its groups
    pub target: Entity,
}

/// The graph edges a request adds and the targets they widen, all checked before any is written
///
/// The plan holds the locks of its targets' roots from the check until its edges are written,
/// so no target's groups change in between; [`write_graph_edges`] or
/// [`EdgePlan::release_on_error`] releases them.
#[derive(Debug, Default)]
pub(crate) struct EdgePlan {
    /// The edges to write
    pub edges: Vec<CheckedEdge>,
    /// The targets that gain groups so their new edges stay contained
    pub widenings: Vec<WideningPlan>,
    /// The locks of the targets' roots
    pub locks: EdgeLocks,
    /// Whether the written edges queue no edge changes, because the caller queues a change
    /// that covers them
    pub skip_edge_events: bool,
}

impl EdgePlan {
    /// Whether this plan writes nothing
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty() && self.widenings.is_empty()
    }

    /// Release this plan's locks when a step between checking and writing it failed
    ///
    /// # Arguments
    ///
    /// * `result` - The result of the step
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn release_on_error<T>(
        &mut self,
        result: Result<T, ApiError>,
        shared: &Shared,
    ) -> Result<T, ApiError> {
        // a failed step releases the locks before its error is returned
        if result.is_err() {
            self.locks.release(shared).await?;
        }
        result
    }
}

/// The locks of the roots that guard the groups of some edge targets
#[derive(Debug, Default)]
pub(crate) struct EdgeLocks {
    /// Each locked root's id and the token its lock was taken with
    held: Vec<(Uuid, String)>,
}

impl EdgeLocks {
    /// Take the locks of the roots guarding some edge targets, in id order
    ///
    /// The source's own root is skipped, since its writer already holds that lock or the source
    /// is still being created. When the source's lock is held, the targets' locks are taken in
    /// one order with it: a root sorting after the source's is waited on for what's left of
    /// [`EDGE_LOCK_WAIT`], while a root sorting before it is only waited on for
    /// [`EDGE_LOCK_YIELD_WAIT`] before the request gives way with a `409`. Two requests that each
    /// hold a lock the other needs therefore never wait each other out, and no lock is held past
    /// its expiry while waiting. A source being created holds no lock, so its request waits on
    /// every root for what's left of [`EDGE_LOCK_WAIT`].
    ///
    /// # Arguments
    ///
    /// * `source` - The edges' source
    /// * `source_locked` - Whether the request holds the source's lock
    /// * `targets` - The edges' targets
    /// * `shared` - Shared Thorium objects
    async fn acquire<'a>(
        source: &Entity,
        source_locked: bool,
        targets: impl Iterator<Item = &'a Entity>,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // find each target's root once, leaving out the source's own root
        let source_root = tools::lock_root(source);
        let mut roots = targets
            .filter_map(|target| tools::lock_root(target).map(|root| (root, &target.name)))
            .filter(|(root, _)| Some(*root) != source_root)
            .collect::<Vec<(Uuid, &String)>>();
        roots.sort_by_key(|(root, _)| *root);
        roots.dedup_by_key(|(root, _)| *root);
        // take each lock in order within one overall wait, releasing the ones already taken if
        // one can't be
        let deadline = Instant::now() + EDGE_LOCK_WAIT;
        let mut locks = EdgeLocks::default();
        for (root, name) in roots {
            // a root sorting before a held source lock breaks the order, so it's only waited on
            // briefly
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = if source_locked && source_root.is_some_and(|source| root < source) {
                remaining.min(EDGE_LOCK_YIELD_WAIT)
            } else {
                remaining
            };
            match db::entities::lock_instances(root, name, wait, shared).await {
                Ok(token) => locks.held.push((root, token)),
                Err(err) => {
                    locks.release(shared).await?;
                    return Err(err);
                }
            }
        }
        Ok(locks)
    }

    /// Release every lock still held
    ///
    /// Every lock is released even when one release fails, and the first failure is returned.
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn release(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // release each lock, keeping the first failure
        let mut result = Ok(());
        for (root, token) in self.held.drain(..) {
            let released = tools::unlock(root, &token, shared).await;
            if result.is_ok() {
                result = released;
            }
        }
        result
    }
}

/// Describe an entity for an edge error
///
/// # Arguments
///
/// * `entity` - The entity to describe
fn describe_entity(entity: &Entity) -> String {
    // name the entity's kind and name
    format!("{} `{}`", entity.kind, entity.name)
}

/// Check that an agent's tool call or a toolset's include targets an agentic tool or tool instance
///
/// Agents only call agentic tools; Image tools run as jobs and are reached through an agentic
/// tool's container backing.
///
/// # Arguments
///
/// * `kind` - The kind of edge
/// * `target` - The tool or tool instance being called or included
fn check_agentic(kind: AssociationKind, target: &Entity) -> Result<(), ApiError> {
    // get the kind of tool the target defines
    let tool_kind = match &target.metadata {
        EntityMetadata::Tool(tool) => Some(tool.kind),
        EntityMetadata::ToolInstance(instance) => Some(instance.spec.kind()),
        _ => None,
    };
    // only agentic tools can be called by an agent or included in a toolset
    let rule = if kind == AssociationKind::Includes {
        "toolsets only include"
    } else {
        "agents only call"
    };
    match tool_kind {
        Some(ToolKind::Agentic) | None => Ok(()),
        Some(tool_kind) => bad!(format!(
            "{rule} Agentic tools, but {} is a {tool_kind} tool",
            describe_entity(target)
        )),
    }
}

/// Check that an agent's harness edge targets a Harness tool or tool instance
///
/// # Arguments
///
/// * `target` - The tool or tool instance the agent would run in
fn check_harness(target: &Entity) -> Result<(), ApiError> {
    // get the kind of tool the target defines
    let tool_kind = match &target.metadata {
        EntityMetadata::Tool(tool) => Some(tool.kind),
        EntityMetadata::ToolInstance(instance) => Some(instance.spec.kind()),
        _ => None,
    };
    // agents only run in harnesses
    match tool_kind {
        Some(ToolKind::Harness) | None => Ok(()),
        Some(tool_kind) => bad!(format!(
            "agents only run in Harness tools, but {} is a {tool_kind} tool",
            describe_entity(target)
        )),
    }
}

/// Check that a pinned edge target can still be newly pinned
///
/// A deprecated tool instance can't be newly pinned, and an agent can only pin a toolset
/// instance that is ready.
///
/// # Arguments
///
/// * `target` - The edge's target
fn check_pinnable(target: &Entity) -> Result<(), ApiError> {
    match &target.metadata {
        // a deprecated tool instance can't be newly pinned
        EntityMetadata::ToolInstance(instance) if instance.deprecated => bad!(format!(
            "tool instance `{}` is deprecated and can't be pinned",
            target.name
        )),
        // only a ready toolset instance can be attached
        EntityMetadata::ToolSetInstance(instance) if !instance.is_ready() => bad!(format!(
            "toolset instance `{}` is {} and can't be pinned; only a Ready instance can",
            target.name,
            instance.status.as_str()
        )),
        _ => Ok(()),
    }
}

/// Check one requested edge's target: it exists and is visible, its kind fits the field and the
/// kind pair, an agent only calls agentic tools, and an agent only runs in a harness
///
/// Returns the target in the groups the user can see it in.
///
/// # Arguments
///
/// * `user` - The user adding the edge
/// * `source` - The edge's source
/// * `request` - The requested edge
/// * `shared` - Shared Thorium objects
async fn check_target(
    user: &User,
    source: &Entity,
    request: &EdgeRequest,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // the target must exist and be visible to this user
    let (kind, id) = (request.kind, request.target);
    let target = match Entity::get(user, id, shared).await {
        Ok(target) => target,
        Err(err) if err.status() == axum::http::StatusCode::NOT_FOUND => {
            return bad!(format!(
                "the target {id} of a `{kind}` edge doesn't exist or you can't see it"
            ));
        }
        Err(err) => return Err(err),
    };
    // a form field may only name the kinds it's for
    if let Some(field) = &request.field
        && !field.kinds.contains(&target.kind)
    {
        return bad!(format!(
            "`{}` must name {}, but `{}` is a {}",
            field.name, field.describe, target.name, target.kind
        ));
    }
    // the kind pair must be allowed
    if !kind.allows_pair(source.kind, target.kind) {
        return bad!(format!(
            "a `{kind}` edge can't link {} to {}",
            describe_entity(source),
            describe_entity(&target)
        ));
    }
    // agents only call agentic tools, toolsets only include them, and pins must be usable
    if matches!(kind, AssociationKind::Calls | AssociationKind::Includes) {
        check_agentic(kind, &target)?;
        check_pinnable(&target)?;
    }
    // agents only run in harnesses, and a harness pin must be usable
    if kind == AssociationKind::RunsIn {
        check_harness(&target)?;
        check_pinnable(&target)?;
    }
    Ok(target)
}

/// Check the graph edges a request adds from one source, before anything is written
///
/// Each target must exist and be visible, the kind pair must be allowed, an agent may only call
/// agentic tools, and the target's groups must cover the source's. With `extend` a target that
/// misses some of the source's groups is widened instead, when the caller holds the rights a
/// group add on the target's root needs: edit rights, with entities allowed, in every one of
/// its groups and in the groups it gains. The caller must be able to write associations in every
/// group of the source. A target missing groups the caller can see, and not widened, is a `400`
/// naming them for each target and saying when the caller can't widen it; one missing only
/// hidden groups is a plain `401`.
///
/// # Arguments
///
/// * `user` - The user adding the edges
/// * `source` - The edges' source, in every one of its groups
/// * `source_locked` - Whether the request holds the source's lock, which a create doesn't
/// * `requested` - The edges to add
/// * `extend` - Whether to widen targets that miss some of the source's groups
/// * `shared` - Shared Thorium objects
#[instrument(name = "associations::check_graph_edges", skip_all, err(Debug))]
pub(crate) async fn check_graph_edges(
    user: &User,
    source: &Entity,
    source_locked: bool,
    requested: &[EdgeRequest],
    extend: bool,
    shared: &Shared,
) -> Result<EdgePlan, ApiError> {
    // a request without edges has nothing to check
    if requested.is_empty() {
        return Ok(EdgePlan::default());
    }
    // the user must be able to write associations in every group of the source
    let _ = Group::authorize_check_allow_all(
        user,
        &source.groups,
        Group::editable,
        "edit",
        Some(GroupAllowAction::Associations),
        shared,
    )
    .await?;
    // check each edge's target once
    let mut seen = HashSet::with_capacity(requested.len());
    let mut checked = Vec::with_capacity(requested.len());
    for request in requested {
        // skip an edge named twice in the same field
        let (kind, id) = (&request.kind, &request.target);
        let field = request.field.map(|field| field.name);
        if !seen.insert((*kind, *id, field)) {
            continue;
        }
        checked.push(CheckedEdge {
            kind: *kind,
            target: check_target(user, source, request, shared).await?,
        });
    }
    // hold the targets' roots' locks until the edges are written, so no target's groups change
    // between this check and the write
    let targets = checked.iter().map(|edge| &edge.target);
    let mut locks = EdgeLocks::acquire(source, source_locked, targets, shared).await?;
    // check the targets' groups under the locks, releasing them if the edges are refused
    match helpers::heap_future(|| check_edge_groups(user, source, checked, extend, shared)).await {
        Ok(mut plan) => {
            plan.locks = locks;
            Ok(plan)
        }
        Err(err) => {
            locks.release(shared).await?;
            Err(err)
        }
    }
}

/// Check that the targets of some checked edges cover their source's groups, planning the
/// widenings of the targets that don't when `extend` is set
///
/// # Arguments
///
/// * `user` - The user adding the edges
/// * `source` - The edges' source, in every one of its groups
/// * `checked` - The edges with their checked targets
/// * `extend` - Whether to widen targets that miss some of the source's groups
/// * `shared` - Shared Thorium objects
async fn check_edge_groups(
    user: &User,
    source: &Entity,
    checked: Vec<CheckedEdge>,
    extend: bool,
    shared: &Shared,
) -> Result<EdgePlan, ApiError> {
    // track the targets missing groups, and whether any misses only hidden groups
    let mut plan = EdgePlan::default();
    let mut uncovered: Vec<String> = Vec::new();
    let mut hidden_only = false;
    // track the groups each target must gain and the ones this user can see
    let mut widen: Vec<(Entity, Vec<String>, Vec<String>)> = Vec::new();
    for CheckedEdge { kind, mut target } in checked {
        // read every group of the target
        target.groups = db::entities::all_groups(target.id, shared).await?;
        // the target must cover every group of the source
        if let Err(missing) = kind.group_policy(&source.groups, &target.groups) {
            // name only the missing groups this user can see
            let visible = missing
                .iter()
                .filter(|group| user.is_admin() || user.groups.contains(*group))
                .cloned()
                .collect::<Vec<String>>();
            if extend {
                // widen this target instead of refusing the edge
                widen.push((target.clone(), missing, visible));
            } else if visible.is_empty() {
                hidden_only = true;
            } else {
                uncovered.push(format!(
                    "{} isn't in groups {visible:?}",
                    describe_entity(&target)
                ));
            }
        }
        // keep the checked edge once, even when two fields name it
        if !plan
            .edges
            .iter()
            .any(|edge| edge.kind == kind && edge.target.id == target.id)
        {
            plan.edges.push(CheckedEdge { kind, target });
        }
    }
    // check every widening before anything is written
    for (target, missing, visible) in widen {
        // a target this user may not add the groups to isn't widened
        let Some(widening) =
            helpers::heap_future(|| Entity::check_widening(user, &target, &missing, shared))
                .await?
        else {
            if visible.is_empty() {
                hidden_only = true;
            } else {
                uncovered.push(format!(
                    "{} isn't in groups {visible:?}, and you can't widen it",
                    describe_entity(&target)
                ));
            }
            continue;
        };
        // merge widenings of the same root, such as two instances of one tool
        match plan
            .widenings
            .iter_mut()
            .find(|existing| existing.root_id() == widening.root_id())
        {
            Some(existing) => existing.merge(widening),
            None => plan.widenings.push(widening),
        }
    }
    // refuse edges whose targets don't cover the source, naming what the user can see
    if !uncovered.is_empty() {
        return bad!(format!(
            "every target of an edge from {} must be in all of its groups: {}",
            describe_entity(source),
            uncovered.join("; ")
        ));
    }
    if hidden_only {
        return unauthorized!();
    }
    Ok(plan)
}

/// The most outgoing edges of a kind one source may have, and what that limit is called
///
/// # Arguments
///
/// * `kind` - The kind of edge
/// * `shared` - Shared Thorium objects
fn edge_cap(kind: AssociationKind, shared: &Shared) -> Option<(usize, String)> {
    // the limits a resolution reads the graph under
    let events = &shared.config.thorium.events;
    let crawl = CrawlLimits::from(events);
    match kind {
        // an agent's tools are only read while its calls fit twice its tool limit
        AssociationKind::Calls => Some((
            crawl.max_listed_tools_per_agent(),
            format!("twice max_tools_per_agent ({})", crawl.max_tools_per_agent),
        )),
        // no workflow reaches more agents than its agent limit
        AssociationKind::DelegatesTo | AssociationKind::Orchestrates => Some((
            crawl.max_agents_per_workflow,
            format!(
                "max_agents_per_workflow ({})",
                crawl.max_agents_per_workflow
            ),
        )),
        // an agent runs in one harness
        AssociationKind::RunsIn => Some((1, "one harness".to_owned())),
        // a toolset only considers so many explicit edges
        AssociationKind::Includes => {
            let tool_sets = ToolSetLimits::from(events);
            Some((
                tool_sets.max_candidates(),
                format!(
                    "4 x max_tools_per_toolset ({})",
                    tool_sets.max_tools_per_toolset
                ),
            ))
        }
        // no other kind is bounded
        _ => None,
    }
}

/// Get the outgoing graph edges an agent, toolset, or workflow has, from its filled in
/// metadata
///
/// # Arguments
///
/// * `source` - The source, with its edges filled in
fn current_edges(source: &Entity) -> HashSet<(AssociationKind, Uuid)> {
    // pair each filled in target with the kind of edge it came from
    let pair = |kind: AssociationKind, target: &Entity| (kind, target.id);
    match &source.metadata {
        EntityMetadata::Agent(agent) => agent
            .latest_tools
            .iter()
            .chain(&agent.pinned_tools)
            .chain(&agent.tool_sets)
            .map(|target| pair(AssociationKind::Calls, target))
            .chain(
                agent
                    .sub_agents
                    .iter()
                    .map(|target| pair(AssociationKind::DelegatesTo, target)),
            )
            .chain(
                agent
                    .harness
                    .iter()
                    .map(|target| pair(AssociationKind::RunsIn, target)),
            )
            .collect(),
        EntityMetadata::ToolSet(tool_set) => tool_set
            .tools
            .iter()
            .map(|target| pair(AssociationKind::Includes, target))
            .collect(),
        EntityMetadata::WorkflowCatalog(workflow) => workflow
            .agents
            .iter()
            .map(|target| pair(AssociationKind::Orchestrates, target))
            .collect(),
        _ => HashSet::new(),
    }
}

/// Refuse a plan that would give its source more outgoing edges of a kind than a resolution
/// reads
///
/// Only a kind whose count grows is refused, so a source already over a limit can shed edges.
///
/// An agent has at most twice its tool limit in calls, as many delegations as a workflow has
/// agents, and one harness; a workflow orchestrates at most its agent limit; and a toolset
/// includes at most the explicit edges it may consider. Bounding the edges when they're written
/// bounds the work every read of the source, and every resolution through it, does.
///
/// # Arguments
///
/// * `source` - The source, with its edges filled in
/// * `removes` - The edges the same request removes
/// * `plan` - The checked edges the request adds
/// * `shared` - Shared Thorium objects
pub(crate) fn check_edge_caps(
    source: &Entity,
    removes: &HashSet<(AssociationKind, Uuid)>,
    plan: &EdgePlan,
    shared: &Shared,
) -> Result<(), ApiError> {
    // skip the caps when a test stands in for a source written before them
    #[cfg(feature = "test-utilities")]
    if crate::test_utilities::faults::take_uncapped_edge_write(source.id) {
        return Ok(());
    }
    // the edges the source has now and once the request is applied
    let current = current_edges(source);
    let mut edges = current.clone();
    edges.retain(|edge| !removes.contains(edge));
    edges.extend(plan.edges.iter().map(|edge| (edge.kind, edge.target.id)));
    // count both by kind
    let count = |edges: &HashSet<(AssociationKind, Uuid)>| {
        let mut counts: HashMap<AssociationKind, usize> = HashMap::new();
        for (kind, _) in edges {
            *counts.entry(*kind).or_default() += 1;
        }
        counts
    };
    let before = count(&current);
    // each bounded kind that grows must fit its limit
    for (kind, count) in count(&edges) {
        let grows = count > before.get(&kind).copied().unwrap_or_default();
        // a second harness is refused naming the one the agent runs in
        if let (AssociationKind::RunsIn, true, EntityMetadata::Agent(agent)) =
            (kind, grows && count > 1, &source.metadata)
            && let Some(current) = &agent.harness
            && !removes.contains(&(kind, current.id))
        {
            return bad!(format!(
                "agent `{}` already runs in harness `{}`; clear it to replace it",
                source.name, current.name
            ));
        }
        if let Some((cap, limit)) = edge_cap(kind, shared)
            && grows
            && count > cap
        {
            return bad!(format!(
                "{} would have {count} `{kind}` edges, over the {cap} allowed by {limit}",
                describe_entity(source)
            ));
        }
    }
    Ok(())
}

/// Write the edges and widenings of a checked plan, then release its locks
///
/// Widened targets are written first, so every edge is written between entities that share
/// all of the source's groups. Each edge is written in every group of the source. The locks
/// are released whether or not the writes worked.
///
/// # Arguments
///
/// * `user` - The user adding the edges
/// * `source` - The edges' source, in every one of its groups
/// * `plan` - The checked plan to write
/// * `shared` - Shared Thorium objects
#[instrument(name = "associations::write_graph_edges", skip_all, err(Debug))]
pub(crate) async fn write_graph_edges(
    user: &User,
    source: &Entity,
    mut plan: EdgePlan,
    shared: &Shared,
) -> Result<(), ApiError> {
    // write the plan, then release its locks whatever happened
    let mut locks = std::mem::take(&mut plan.locks);
    let result = helpers::heap_future(|| write_plan(user, source, plan, shared)).await;
    locks.release(shared).await?;
    result
}

/// Write the edges and widenings of a checked plan
///
/// # Arguments
///
/// * `user` - The user adding the edges
/// * `source` - The edges' source, in every one of its groups
/// * `plan` - The checked plan to write
/// * `shared` - Shared Thorium objects
async fn write_plan(
    user: &User,
    source: &Entity,
    plan: EdgePlan,
    shared: &Shared,
) -> Result<(), ApiError> {
    // widen the targets that need it first
    for widening in plan.widenings {
        helpers::heap_future(|| Entity::widen(user, widening, shared)).await?;
    }
    // build the source of every edge
    let source_target = AssociationTarget::Entity {
        id: source.id,
        name: source.name.clone(),
    };
    // write each edge in every group of the source, keeping each written edge's change
    let mut changes = Vec::with_capacity(plan.edges.len());
    let mut written = Ok(());
    for (position, edge) in plan.edges.into_iter().enumerate() {
        let change = WorkflowGraphChange::EdgeChanged {
            source: source.id,
            target: edge.target.id,
            kind: edge.kind,
        };
        let target = AssociationTarget::Entity {
            id: edge.target.id,
            name: edge.target.name,
        };
        let created = if edge_write_fails(source.id, position) {
            internal_err!("the edge write failed".to_owned())
        } else {
            db::associations::create(
                user,
                source.groups.len(),
                edge.kind,
                source_target.clone(),
                &vec![(target, source.groups.clone())],
                None,
                Directionality::To,
                shared,
            )
            .await
        };
        if let Err(error) = created {
            written = Err(error);
            break;
        }
        if !plan.skip_edge_events {
            changes.push((source.groups.clone(), change));
        }
    }
    // queue the written edges' changes in every group they were written in, in one pipeline,
    // even when a later edge failed
    let queued = if edge_queue_fails(source.id) {
        Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Some("the edge changes couldn't be queued".to_owned()),
        ))
    } else {
        Event::queue_workflow_graphs(user, changes, shared).await
    };
    // a failed edge write is the first failure, so it's returned over a failure to queue
    written.and(queued)
}

/// Whether a test asked the write of the edge at a position in a source's edge write to fail
///
/// # Arguments
///
/// * `source` - The id of the edges' source
/// * `position` - The position of the edge about to be written, from zero
#[cfg(feature = "test-utilities")]
fn edge_write_fails(source: Uuid, position: usize) -> bool {
    // only the test API injects failures
    crate::test_utilities::faults::take_edge_write_failure(source, position)
}

/// Whether a test asked the write of the edge at a position in a source's edge write to fail,
/// which never happens outside the test API
///
/// # Arguments
///
/// * `_source` - The id of the edges' source
/// * `_position` - The position of the edge about to be written, from zero
#[cfg(not(feature = "test-utilities"))]
fn edge_write_fails(_source: Uuid, _position: usize) -> bool {
    false
}

/// Whether a test asked a source's edge write to fail to queue its changes
///
/// # Arguments
///
/// * `source` - The id of the edges' source
#[cfg(feature = "test-utilities")]
fn edge_queue_fails(source: Uuid) -> bool {
    // only the test API injects failures
    crate::test_utilities::faults::take_edge_queue_failure(source)
}

/// Whether a test asked a source's edge write to fail to queue its changes, which never happens
/// outside the test API
///
/// # Arguments
///
/// * `_source` - The id of the edges' source
#[cfg(not(feature = "test-utilities"))]
fn edge_queue_fails(_source: Uuid) -> bool {
    false
}

/// Queue an edge change for each graph edge among an entity's removed association rows
///
/// Each change is queued in the groups its edge's rows were removed from. Rows of other kinds
/// queue nothing.
///
/// # Arguments
///
/// * `user` - The user who removed the edges
/// * `entity` - The entity the rows were listed for
/// * `rows` - The removed rows, as listed from the entity's side
/// * `shared` - Shared Thorium objects
pub(crate) async fn queue_removed_edges(
    user: &User,
    entity: Uuid,
    rows: &[ListableAssociation],
    shared: &Shared,
) -> Result<(), ApiError> {
    // only the graph edges among the rows queue changes
    let mut changes = Vec::new();
    for row in rows.iter().filter(|row| row.kind.is_graph_edge()) {
        // graph edges only link entities
        let AssociationTargetColumn::Entity(other) = row.deserialize_other()? else {
            continue;
        };
        // a row listed from the target's side names the edge's source
        let (source, target) = match row.direction {
            Directionality::From => (other, entity),
            Directionality::To | Directionality::Bidirectional => (entity, other),
        };
        let change = WorkflowGraphChange::EdgeChanged {
            source,
            target,
            kind: row.kind,
        };
        // the change is queued in the groups the edge was removed from
        changes.push((row.groups.clone(), change));
    }
    // queue every change in one pipeline
    Event::queue_workflow_graphs(user, changes, shared).await
}

impl AssociationTarget {
    /// Make sure this target exists and get its groups
    pub async fn get_groups(&self, user: &User, shared: &Shared) -> Result<Vec<String>, ApiError> {
        match self {
            AssociationTarget::File(sha256) => {
                // get this sample and its groups
                let sample = Sample::get(user, sha256, shared).await?;
                // get this samples groups
                let groups = sample
                    .groups()
                    .into_iter()
                    .map(std::borrow::ToOwned::to_owned)
                    .collect::<Vec<String>>();
                Ok(groups)
            }
            AssociationTarget::Repo(url) => {
                // get this repo
                let repo = Repo::get(user, url, shared).await?;
                // get this repos groups
                let groups = repo
                    .groups()
                    .into_iter()
                    .map(std::borrow::ToOwned::to_owned)
                    .collect::<Vec<String>>();
                Ok(groups)
            }
            AssociationTarget::Entity { id, .. } => {
                // get a specific entity
                let entity = Entity::get(user, *id, shared).await?;
                // get this entities groups
                Ok(entity.groups)
            }
        }
    }
}

impl AssociationTarget {
    /// Describe this target in an error message
    fn describe(&self) -> String {
        match self {
            AssociationTarget::Entity { name, .. } => format!("`{name}`"),
            AssociationTarget::File(sha256) => format!("file `{sha256}`"),
            AssociationTarget::Repo(url) => format!("repo `{url}`"),
        }
    }
}

impl AssociationRequest {
    /// Get the groups of a source that a user may write associations in
    ///
    /// Writing an association is an edit, so a group is kept only if the user can edit data in it
    /// and it allows associations. A `401` is returned when no group is left.
    ///
    /// # Arguments
    ///
    /// * `user` - The user writing associations
    /// * `groups` - The groups of the association's source
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "AssociationRequest::authorize_source_groups",
        skip(user, shared),
        err(Debug)
    )]
    pub async fn authorize_source_groups(
        user: &User,
        groups: &[String],
        shared: &Shared,
    ) -> Result<Vec<String>, ApiError> {
        // only consider the groups this user is in, unless they're an admin
        let candidates = groups
            .iter()
            .filter(|group| user.is_admin() || user.groups.contains(*group))
            .collect::<Vec<&String>>();
        // get the details of the groups we can consider
        let details = db::groups::list_details(candidates.into_iter(), shared).await?;
        // keep the groups this user can edit that allow associations
        let allowed = details
            .into_iter()
            .filter(|group| group.editable(user).is_ok())
            .filter(|group| group.allowable(GroupAllowAction::Associations).is_ok())
            .map(|group| group.name)
            .collect::<Vec<String>>();
        // a user that can't write associations in any group is refused
        if allowed.is_empty() {
            return unauthorized!(
                "you don't have permission to add associations in any of the source's groups"
                    .to_owned()
            );
        }
        Ok(allowed)
    }

    /// Get the groups an association between a source and a target is written in
    ///
    /// # Arguments
    ///
    /// * `source` - The association's source
    /// * `source_groups` - The groups the user may write the source's associations in
    /// * `target` - The association's target
    /// * `target_groups` - The target's groups
    pub fn shared_groups(
        source: &AssociationTarget,
        source_groups: &[String],
        target: &AssociationTarget,
        target_groups: &[String],
    ) -> Result<Vec<String>, ApiError> {
        // get the groups both sides are in
        let shared_groups = helpers::intersection(source_groups, target_groups);
        // an association needs at least one group both sides are in
        if shared_groups.is_empty() {
            return bad!(format!(
                "{} in {:?} and {} in {:?} share no group to associate them in",
                source.describe(),
                source_groups,
                target.describe(),
                target_groups
            ));
        }
        Ok(shared_groups)
    }

    /// Make sure one end of an association is in every explicitly requested group
    ///
    /// # Arguments
    ///
    /// * `end` - The source or target to check
    /// * `end_groups` - The groups this end is in
    /// * `groups` - The explicitly requested groups
    fn check_explicit_groups(
        end: &AssociationTarget,
        end_groups: &[String],
        groups: &[String],
    ) -> Result<(), ApiError> {
        // find any requested groups this end isn't in
        let missing = helpers::missing_groups(groups, end_groups);
        if !missing.is_empty() {
            return bad!(format!(
                "{} is not in the association's groups {:?}",
                end.describe(),
                missing
            ));
        }
        Ok(())
    }

    /// Apply this association request to the desired entities/objects
    ///
    /// # Arguments
    ///
    /// * `user` - The user applying this association request
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "AssociationRequest::apply", skip_all, err(Debug))]
    pub async fn apply(self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // edges between a tool instance and its tool are only written when it's published
        if self.kind == AssociationKind::PublishedFrom {
            return bad!("`PublishedFrom` is managed by publishing a tool instance".to_owned());
        }
        // model configuration references are only written by their referrer's own updates
        if self.kind == AssociationKind::AuthenticatesWith {
            return bad!(
                "`AuthenticatesWith` is managed through the Model Endpoint's `credential`"
                    .to_owned()
            );
        }
        if self.kind == AssociationKind::Serves {
            return bad!("`Serves` is managed through the Model Profile's `endpoint`".to_owned());
        }
        // harness native tools are only provided when a harness instance is published
        if self.kind == AssociationKind::Provides {
            return bad!("`Provides` is managed by publishing a harness tool instance".to_owned());
        }
        // an instance is only linked to its toolset or workflow when the instance is submitted
        if self.kind == AssociationKind::BuiltFrom {
            return bad!(
                "`BuiltFrom` is managed by submitting a toolset or workflow instance".to_owned()
            );
        }
        // graph edges go through the shared edge checks
        if self.kind.is_graph_edge() {
            return helpers::heap_future(|| self.apply_graph(user, shared)).await;
        }
        // a request without targets has nothing to write
        if self.targets.is_empty() {
            return Ok(());
        }
        // make sure the source exists and get the groups we can see it in
        let source_groups = self.source.get_groups(user, shared).await?;
        // track whether groups were explicitly set
        let explicit = !self.groups.is_empty();
        // get the groups we are allowed to write associations in
        let groups = if explicit {
            // make sure all of these groups are valid and we can edit associations in them
            let _ = Group::authorize_check_allow_all(
                user,
                &self.groups,
                Group::editable,
                "edit",
                Some(GroupAllowAction::Associations),
                shared,
            )
            .await?;
            // the source must be in every explicit group
            Self::check_explicit_groups(&self.source, &source_groups, &self.groups)?;
            self.groups
        } else {
            // keep only the source's groups we can edit and that allow associations
            Self::authorize_source_groups(user, &source_groups, shared).await?
        };
        // build a list of targets and the groups to apply their associations too
        let mut target_list = Vec::with_capacity(self.targets.len());
        // keep track of the total number of rows we will need to insert
        let mut size_hint = 0;
        // validate every target before anything is written
        for target in self.targets {
            // make sure this target exists and get its groups
            let target_groups = target.get_groups(user, shared).await?;
            // get the groups to write this target's association in
            let assoc_groups = if explicit {
                // the target must be in every explicit group
                Self::check_explicit_groups(&target, &target_groups, &groups)?;
                groups.clone()
            } else {
                // use the groups both sides are in
                Self::shared_groups(&self.source, &groups, &target, &target_groups)?
            };
            // we will need one insert for each group
            size_hint += assoc_groups.len();
            // add our target and its validated editable groups
            target_list.push((target, assoc_groups));
        }
        // get the direction for this association
        let direction = if self.is_bidirectional {
            Directionality::Bidirectional
        } else {
            Directionality::To
        };
        // save this association
        db::associations::create(
            user,
            size_hint,
            self.kind,
            self.source,
            &target_list,
            self.submissions,
            direction,
            shared,
        )
        .await?;
        Ok(())
    }
}

impl AssociationDeleteRequest {
    /// Delete the associations matching this request
    ///
    /// Only the submitter of an association, a manager/owner of the group it is in, or an admin
    /// can delete it. If any matching association cannot be deleted by this user then nothing
    /// is deleted.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleting associations
    /// * `shared` - Shared Thorium objects
    pub async fn apply(self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // make sure we have at least one target to delete associations with
        if self.targets.is_empty() {
            return bad!("At least one target must be set to delete associations".to_owned());
        }
        // make sure our source exists and that we can see it
        self.source.get_groups(user, shared).await?;
        // make sure we can access the requested groups or default to all of our groups
        let mut groups = self.groups;
        user.authorize_groups(&mut groups, shared).await?;
        // get the column for our source
        let (source, _) = self.source.to_column()?;
        // serialize our targets so we can compare them to listed associations
        let mut targets = HashSet::with_capacity(self.targets.len());
        for target in self.targets {
            // get the column for this target (entity names are not part of this)
            let (column, _) = target.to_column()?;
            // serialize this target the same way it was saved
            targets.insert(serialize!(&column));
        }
        // build the params for listing all associations for our source
        let params = AssociationListParams {
            groups,
            limit: 1000,
            ..Default::default()
        };
        // list all associations for our source scoped to the requesting user
        let mut cursor = db::associations::list(user, params, &source, shared).await?;
        // keep a list of the associations to delete
        let mut matched = Vec::default();
        // step over all of our associations and find the ones to delete
        loop {
            // filter down to just the associations that match our request
            for assoc in cursor.data.drain(..) {
                // check if this association matches our request
                if assoc.kind == self.kind
                    && targets.contains(&assoc.other)
                    && (self.directions.is_empty() || self.directions.contains(&assoc.direction))
                {
                    matched.push(assoc);
                }
            }
            // check if this cursor is exhausted
            if cursor.exhausted() {
                break;
            }
            // get the next page of data in this cursor
            cursor.next(shared).await?;
        }
        // if we didn't find any associations then return a 404
        if matched.is_empty() {
            return not_found!("No matching associations found".to_owned());
        }
        // admins can delete any association
        if !user.is_admin() {
            // get the groups for any associations we didn't submit
            let foreign_groups = matched
                .iter()
                .filter(|assoc| assoc.submitter != user.username)
                .flat_map(|assoc| assoc.groups.iter())
                .collect::<BTreeSet<&String>>();
            // we must be a manager or owner in these groups to delete these associations
            if !foreign_groups.is_empty() {
                // get the details for these groups
                let details =
                    db::groups::list_details(foreign_groups.iter().copied(), shared).await?;
                // make sure we are a manager or owner in each group
                for group in details {
                    if !group.is_manager_or_owner(&user.username) {
                        return unauthorized!(format!(
                            "Cannot delete associations submitted by other users in group {}",
                            group.name
                        ));
                    }
                }
            }
        }
        // delete all of our matched associations
        db::associations::delete_many(&source, &matched, shared).await
    }
}

impl AssociationRequest {
    /// Apply a request for graph edges with the shared edge checks
    ///
    /// Graph edges are written in every group of their source, so explicit groups must be
    /// exactly the source's groups. A caller who can't see the source, or can't write
    /// associations in every one of its groups, is refused before waiting on its lock. The
    /// source's lock is held while its edges are checked and written, as its own updates hold
    /// it.
    ///
    /// # Arguments
    ///
    /// * `user` - The user applying this association request
    /// * `shared` - Shared Thorium objects
    async fn apply_graph(self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // a request without targets has nothing to write
        if self.targets.is_empty() {
            return Ok(());
        }
        // graph edges always start at an entity
        let AssociationTarget::Entity { id, name } = &self.source else {
            return bad!(format!("a `{}` edge must start at an entity", self.kind));
        };
        // a source the caller can't see is missing to them, before its lock is waited on
        let source_groups = db::entities::all_groups(*id, shared).await?;
        let visible = source_groups
            .iter()
            .any(|group| user.is_admin() || user.groups.contains(group));
        if !visible {
            return not_found!(format!("Entity {id} not found"));
        }
        // the caller must be able to write associations in every group of the source
        let _ = Group::authorize_check_allow_all(
            user,
            &source_groups,
            Group::editable,
            "edit",
            Some(GroupAllowAction::Associations),
            shared,
        )
        .await?;
        // agents, toolsets, and workflows are their own lock roots, so their lock is taken before
        // they're read again
        let (id, token) = (*id, tools::lock(*id, name, shared).await?);
        let result = helpers::heap_future(|| self.apply_graph_locked(id, user, shared)).await;
        // release the lock whether or not the edges were written
        tools::unlock(id, &token, shared).await?;
        result
    }

    /// Apply a request for graph edges while holding its source's lock
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the edges' source
    /// * `user` - The user applying this association request
    /// * `shared` - Shared Thorium objects
    async fn apply_graph_locked(
        self,
        id: Uuid,
        user: &User,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // get the source in every one of its groups
        let mut source = Entity::get(user, id, shared).await?;
        source.groups = db::entities::all_groups(source.id, shared).await?;
        // explicit groups must be every group of the source
        if !self.groups.is_empty()
            && (!helpers::is_superset(&self.groups, &source.groups)
                || !helpers::is_superset(&source.groups, &self.groups))
        {
            return bad!(format!(
                "`{}` edges are written in every group of their source; leave `groups` empty",
                self.kind
            ));
        }
        // collect the target ids, which must all be entities
        let mut requested = Vec::with_capacity(self.targets.len());
        for target in &self.targets {
            match target {
                AssociationTarget::Entity { id, .. } => requested.push(EdgeRequest {
                    kind: self.kind,
                    target: *id,
                    field: None,
                }),
                other => {
                    return bad!(format!(
                        "a `{}` edge can't point at {}",
                        self.kind,
                        other.describe()
                    ));
                }
            }
        }
        // the generic route can't add a second edge to a tool an agent already calls another way
        let mut plan = helpers::heap_future(|| {
            check_graph_edges(
                user,
                &source,
                true,
                &requested,
                self.extend_target_groups,
                shared,
            )
        })
        .await?;
        let checked = helpers::heap_future(|| {
            crate::models::backends::entities::check_new_calls(user, &source, &plan, shared)
        })
        .await
        .and_then(|()| check_edge_caps(&source, &HashSet::new(), &plan, shared));
        plan.release_on_error(checked, shared).await?;
        // write the checked edges
        helpers::heap_future(|| write_graph_edges(user, &source, plan, shared)).await?;
        // a new edge is an edit of its agent, toolset, or workflow source
        db::entities::mark_modified(source, shared).await
    }
}

impl AssociationTarget {
    /// Convert this association target to a column and any extra data
    pub fn to_column(self) -> Result<(AssociationTargetColumn, Option<String>), ApiError> {
        match self {
            AssociationTarget::Entity { id, name } => {
                Ok((AssociationTargetColumn::Entity(id), Some(name)))
            }
            AssociationTarget::File(sha256) => Ok((AssociationTargetColumn::File(sha256), None)),
            AssociationTarget::Repo(url) => Ok((AssociationTargetColumn::Repo(url), None)),
        }
    }
}

// implement cursor for our results stream
impl CursorCore for ListableAssociation {
    /// The params to build this cursor from
    type Params = AssociationListParams;

    /// The extra info to filter with
    type ExtraFilters = String;

    /// The type of data to group our rows by
    type GroupBy = String;

    /// Get our cursor id from params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_id(params: &Self::Params) -> Option<Uuid> {
        params.cursor
    }

    // Get our start and end timestamps
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_start_end(
        params: &Self::Params,
        shared: &Shared,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
        // get our end timestmap
        let end = params.end(shared)?;
        Ok((params.start, end))
    }

    /// Get any group restrictions from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_group_by(params: &mut Self::Params) -> Vec<Self::GroupBy> {
        std::mem::take(&mut params.groups)
    }

    /// Get our extra filters from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_extra_filters(_params: &mut Self::Params) -> Self::ExtraFilters {
        unimplemented!("USE FROM PARAMS EXTRA INSTEAD!")
    }

    /// Get our the max number of rows to return
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_limit(params: &Self::Params) -> Result<CursorLimit, ApiError> {
        // make sure this limit is one we can serve
        Ok(CursorLimit::from_user(
            params.limit,
            DEFAULT_LIMIT_CAP,
            "limit",
        )?)
    }

    /// Get the partition size for this cursor
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn partition_size(shared: &Shared) -> u16 {
        // get our partition size
        shared.config.thorium.associations.partition_size
    }

    /// Determines if a new item is a duplicate or not
    ///
    /// # Arguments
    ///
    /// * `set` - The current set of deduped data
    fn dedupe_item(&self, dedupe_set: &mut HashSet<String>) -> bool {
        // if this is already in our dedupe set then skip it
        if dedupe_set.contains(&self.other) {
            // we already have this commit so skip it
            false
        } else {
            // add this new commit to our dedupe set
            dedupe_set.insert(self.other.clone());
            // keep this new commit
            true
        }
    }
}

// implement cursor for our results stream
impl ScyllaCursorSupport for ListableAssociation {
    /// The intermediate list row to use
    type IntermediateRow = AssociationListRow;

    /// The statements this cursor pages with
    type Statements = AssociationsListCursor;

    /// The unique key for this cursors row
    type UniqueType<'a> = (&'a String, Directionality);

    /// Get the statements this cursor pages with
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn statements(shared: &Shared) -> &Self::Statements {
        &shared.scylla.queries.associations.list
    }

    /// Get the timestamp for this item
    ///
    /// # Arguments
    ///
    /// * `item` - The item to get a timestamp for
    fn get_timestamp(&self) -> DateTime<Utc> {
        self.created
    }

    /// Get the unique key for this intermediate row if it exists
    ///
    /// # Arguments
    ///
    /// * `intermediate` - The intermediate row to get a unique key for
    fn get_intermediate_unique_key(intermediate: &Self::IntermediateRow) -> Self::UniqueType<'_> {
        (&intermediate.other, intermediate.direction)
    }

    /// Get the unique key for this row if it exists
    fn get_unique_key(&self) -> Self::UniqueType<'_> {
        (&self.other, self.direction)
    }

    /// Add a group to a specific returned line
    ///
    /// # Arguments
    ///
    /// * `group` - The group to add to this line
    fn add_group_to_line(&mut self, group: String) {
        // add this group
        self.add_group(group);
    }

    /// Add a group to a specific returned line
    fn add_intermediate_to_line(&mut self, intermediate: Self::IntermediateRow) {
        // add this intermediate rows group
        self.add_group(intermediate.group);
    }

    /// Build all of the keys needs to retrieve census data
    ///
    /// # Arguments
    ///
    /// * `group_by` - The values to group our the rows by
    /// * `extra` - The extra values required to list this data
    /// * `year` - The year we are looking for census data for
    /// * `keys` - The vec to add our census stream keys too
    /// * `shared` - Shared Thorium objects
    fn census_keys<'a>(
        group_by: &'a [Self::GroupBy],
        extra: &Self::ExtraFilters,
        year: i32,
        bucket: Bucket,
        keys: &mut Vec<(&'a Self::GroupBy, String, Bucket)>,
        shared: &Shared,
    ) {
        // build the keys for each census stream we are going to crawl
        for group in group_by {
            // build the key for this associations census stream
            let key = super::db::keys::associations::census_stream(group, year, extra, shared);
            // add this key to our keys
            keys.push((group, key, bucket));
        }
    }
}

impl Association {
    /// List all associations to and from a specific source
    pub async fn list(
        user: &User,
        mut params: AssociationListParams,
        source: &AssociationTargetColumn,
        shared: &Shared,
    ) -> Result<ApiCursor<Association>, ApiError> {
        // make sure these groups are visible to our user
        user.authorize_groups(&mut params.groups, shared).await?;
        // list associations for our some entity/object scoped to the requesting user
        let cursor = db::associations::list(user, params, source, shared).await?;
        // convet our cursor to an api cursor
        let api_cursor = ApiCursor::try_from(cursor)?;
        Ok(api_cursor)
    }

    /// Generate a hash of everything in this association
    #[must_use]
    pub fn full_hash(&self) -> u64 {
        // build a hasher
        let mut hasher = gxhash::GxHasher::with_seed(1234);
        // hash this association
        self.hash(&mut hasher);
        // get this associations hash
        hasher.finish()
    }

    /// Generate a tree node hash for this item
    #[must_use]
    pub fn tree_hash(&self) -> u64 {
        // build a hasher
        let mut hasher = gxhash::GxHasher::with_seed(1234);
        // hash this specific items info
        match &self.other {
            AssociationTarget::Entity { id, .. } => hasher.write_u128(id.as_u128()),
            AssociationTarget::File(sha256) => hasher.write(sha256.as_bytes()),
            AssociationTarget::Repo(url) => hasher.write(url.as_bytes()),
        }
        // finalize our hasher
        hasher.finish()
    }

    /// get the data for this association
    pub async fn get_tree_node(&self, user: &User, shared: &Shared) -> Result<TreeNode, ApiError> {
        match &self.other {
            AssociationTarget::File(sha256) => {
                // get this samples data
                let sample = Sample::get(user, sha256, shared).await?;
                Ok(TreeNode::Sample(sample))
            }
            AssociationTarget::Repo(url) => {
                // get this repos data
                let repo = Repo::get(user, url, shared).await?;
                Ok(TreeNode::Repo(repo))
            }
            AssociationTarget::Entity { id, .. } => {
                // get this entities data without any secrets or groups this user can't see
                let mut entity = Entity::get(user, *id, shared).await?;
                crate::models::RedactSecrets::redact_secrets(&mut entity);
                entity.hide_snapshot_groups(user);
                Ok(TreeNode::Entity(entity))
            }
        }
    }
}

impl TryFrom<ScyllaCursor<ListableAssociation>> for ApiCursor<Association> {
    /// The error to return on failures
    type Error = ApiError;

    /// Convert a scylla cursor to an api cursor for associations
    fn try_from(cursor: ScyllaCursor<ListableAssociation>) -> Result<Self, Self::Error> {
        // get our cursor id if we aren't exhausted yet
        let id = if cursor.exhausted() {
            None
        } else {
            Some(cursor.id)
        };
        // build alist of assocations
        let data = cursor
            .data
            .into_iter()
            .map(Association::try_from)
            .collect::<Result<Vec<Association>, ApiError>>()?;
        // build our api cursor with our converted data
        Ok(ApiCursor { cursor: id, data })
    }
}

impl<S> FromRequestParts<S> for AssociationListParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            Ok(Self::default())
        }
    }
}
