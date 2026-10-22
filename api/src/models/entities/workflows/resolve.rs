//! Resolves a workflow catalog's graph into a workflow instance
//!
//! The crawl only moves forward along the graph's edges: from the catalog to the agents it
//! orchestrates, from each agent to the agents it delegates to, the tools and toolsets it calls,
//! and the harness it runs in, from the entry agent's harness instance to the native tools it
//! provides, and from a tool instance to the Image instance its container backing pins. It never follows lineage, unrelated kinds, or reverse rows, never reads a
//! toolset's `Includes` edges or runs its selector, never visits another workflow, and stops at
//! the crawl limits. Latest edges resolve to their tool's latest instance or their toolset's
//! newest ready instance, whose members an agent is given as the instance snapshotted them, and
//! the result is validated and digested. The graph is read through [`GraphReader`], which the
//! Thorium client and an in-memory test graph implement, so the event handler and tests share one
//! resolver.

use futures::StreamExt;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use uuid::Uuid;

use super::{
    CrawlLimits, FOLLOWS_TOOL_SET_TAG, FOLLOWS_TOOL_TAG, InstancedAgent, InstancedTool,
    InstancedToolBinding, WORKFLOW_ID_TAG, WorkflowCatalogEntity, WorkflowInstanceEntity,
    WorkflowInstanceStatus, WorkflowInstanceTrigger,
};
use crate::Error;
use crate::models::entities::digests::ThorDigest;
use crate::models::entities::toolsets::TOOL_SET_FOLLOWS_TOOL_TAG;
use crate::models::{
    AgentEntity, AssociationKind, BindingCheck, Directionality, Entity, EntityKinds,
    EntityListOpts, EntityMetadata, ToolChoice, ToolInstanceEntity, ToolKind, ToolSetInclusion,
    ToolSpec,
};
use crate::utils::helpers;

#[cfg(any(test, feature = "test-utilities"))]
mod fixture;
#[cfg(test)]
pub(crate) mod test_graph;
#[cfg(test)]
pub(crate) mod tests;

#[cfg(any(test, feature = "test-utilities"))]
pub use fixture::{GraphRead, MemoryGraph};

/// The edge kinds read from a workflow catalog
const WORKFLOW_EDGES: [AssociationKind; 1] = [AssociationKind::Orchestrates];

/// The edge kinds read from an agent
const AGENT_EDGES: [AssociationKind; 3] = [
    AssociationKind::DelegatesTo,
    AssociationKind::Calls,
    AssociationKind::RunsIn,
];

/// The edge kinds read from the entry agent's harness instance
const HARNESS_EDGES: [AssociationKind; 1] = [AssociationKind::Provides];

/// The most tag lookup results the Thorium client reader asks for in one page
const TAG_PAGE_SIZE: usize = 500;

/// How many entities the resolver reads at once ahead of resolving a workflow's agents
const PREFETCH_CONCURRENCY: usize = 16;

/// One edge of an entity, as a graph reader returns it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    /// The kind of edge
    pub kind: AssociationKind,
    /// Whether the edge leaves the entity it was read from (`To`) or arrives at it (`From`)
    pub direction: Directionality,
    /// The entity at the other end of the edge
    pub other: Uuid,
    /// The kind of the entity at the other end of the edge
    pub other_kind: EntityKinds,
}

/// An entity and the edges read with it
#[derive(Debug, Clone)]
pub struct GraphNode {
    /// The entity, in all of its groups
    pub entity: Entity,
    /// The entity's edges of the kinds and direction that were asked for
    pub edges: Vec<GraphEdge>,
}

/// Reads the graph a workflow is resolved from
#[async_trait::async_trait]
pub trait GraphReader: Sync {
    /// Read an entity by id with its edges of some kinds in one direction
    ///
    /// Returns `None` when the entity doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    /// * `kinds` - The edge kinds to return
    /// * `direction` - The direction of the edges to return
    async fn node(
        &self,
        id: Uuid,
        kinds: &[AssociationKind],
        direction: Directionality,
    ) -> Result<Option<GraphNode>, Error>;

    /// Find the entities of a kind tagged with a value in any of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search, and the only groups sent with the lookup
    async fn tagged(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Entity>, Error>;

    /// Find the ids of the entities of a kind tagged with a value in any of some groups, without
    /// reading the entities
    ///
    /// A reader that can list ids alone does, so a lookup costs no entity reads.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search, and the only groups sent with the lookup
    async fn tagged_ids(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Uuid>, Error> {
        // find every match and keep only its id
        let found = self.tagged(kind, key, value, groups).await?;
        Ok(found.into_iter().map(|entity| entity.id).collect())
    }

    /// Find at most some number of the entities of a kind tagged with a value in any of some
    /// groups
    ///
    /// A reader that can stop listing early does, so a lookup that matches many entities costs
    /// no more than the bound.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search, and the only groups sent with the lookup
    /// * `limit` - The most entities to return
    async fn tagged_up_to(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
        limit: usize,
    ) -> Result<Vec<Entity>, Error> {
        // find every match and keep the first ones
        let mut found = self.tagged(kind, key, value, groups).await?;
        found.truncate(limit);
        Ok(found)
    }
}

/// Get the graph edges the API fills into an entity's metadata, all of them outgoing
///
/// # Arguments
///
/// * `entity` - The entity as the API returned it
fn outgoing_edges(entity: &Entity) -> Vec<GraphEdge> {
    // build an outgoing edge to an entity the metadata holds
    let edge = |kind: AssociationKind, target: &Entity| GraphEdge {
        kind,
        direction: Directionality::To,
        other: target.id,
        other_kind: target.kind,
    };
    // read the edges each kind's metadata holds
    match &entity.metadata {
        // a workflow's agents are its Orchestrates edges
        EntityMetadata::WorkflowCatalog(workflow) => workflow
            .agents
            .iter()
            .map(|agent| edge(AssociationKind::Orchestrates, agent))
            .collect(),
        // an agent's sub-agents, tools, toolsets, and harness are its own edges
        EntityMetadata::Agent(agent) => {
            let mut edges = agent
                .sub_agents
                .iter()
                .map(|target| edge(AssociationKind::DelegatesTo, target))
                .collect::<Vec<GraphEdge>>();
            edges.extend(
                agent
                    .latest_tools
                    .iter()
                    .chain(&agent.pinned_tools)
                    .chain(&agent.tool_sets)
                    .map(|target| edge(AssociationKind::Calls, target)),
            );
            edges.extend(
                agent
                    .harness
                    .iter()
                    .map(|target| edge(AssociationKind::RunsIn, target)),
            );
            edges
        }
        // a harness instance's native tools are its Provides edges
        EntityMetadata::ToolInstance(instance) => instance
            .provides
            .iter()
            .map(|target| GraphEdge {
                kind: AssociationKind::Provides,
                direction: Directionality::To,
                other: *target,
                other_kind: EntityKinds::ToolInstance,
            })
            .collect(),
        // a toolset's included tools are its Includes edges
        EntityMetadata::ToolSet(tool_set) => tool_set
            .tools
            .iter()
            .map(|target| edge(AssociationKind::Includes, target))
            .collect(),
        // no other kind has edges the resolvers follow
        _ => Vec::new(),
    }
}

/// Build the entity list options of a tag lookup
///
/// # Arguments
///
/// * `kind` - The kind of entity to find
/// * `key` - The tag key
/// * `value` - The tag value
/// * `groups` - The groups to search
fn tag_lookup(kind: EntityKinds, key: &str, value: &str, groups: &[String]) -> EntityListOpts {
    // search only these groups for this kind and tag
    EntityListOpts::default()
        .groups(groups.to_vec())
        .kind(kind)
        .tag(key, value)
        .page_size(TAG_PAGE_SIZE)
}

/// Build the entity list options of a tag lookup that returns at most some entities
///
/// # Arguments
///
/// * `kind` - The kind of entity to find
/// * `key` - The tag key
/// * `value` - The tag value
/// * `groups` - The groups to search
/// * `limit` - The most entities to return
pub(crate) fn bounded_tag_lookup(
    kind: EntityKinds,
    key: &str,
    value: &str,
    groups: &[String],
    limit: usize,
) -> EntityListOpts {
    // ask for at most the bound, in pages no larger than it
    tag_lookup(kind, key, value, groups)
        .page_size(limit.clamp(1, TAG_PAGE_SIZE))
        .limit(limit)
}

/// Reads the graph through the Thorium API
///
/// The client must be an admin's: the API returns an entity's groups and filled in edges only
/// in the groups its reader can see, so another user's resolution records partial snapshot
/// groups that a submission refuses. Only edges to targets the API reads are returned.
#[async_trait::async_trait]
impl GraphReader for crate::Thorium {
    /// Read an entity by id with its outgoing edges of some kinds
    ///
    /// The API fills an entity's outgoing graph edges into its metadata, so only forward edges
    /// can be read.
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    /// * `kinds` - The edge kinds to return
    /// * `direction` - The direction of the edges to return, which must be `To`
    async fn node(
        &self,
        id: Uuid,
        kinds: &[AssociationKind],
        direction: Directionality,
    ) -> Result<Option<GraphNode>, Error> {
        // the API only returns outgoing edges
        if direction != Directionality::To {
            return Err(Error::new(
                "the Thorium API only returns an entity's outgoing graph edges",
            ));
        }
        // read the entity, which doesn't exist when it isn't found
        let entity = match self.entities.get(id).await {
            Ok(entity) => entity,
            Err(err) if err.status() == Some(reqwest::StatusCode::NOT_FOUND) => return Ok(None),
            Err(err) => return Err(err),
        };
        // keep the edges of the asked kinds
        let edges = outgoing_edges(&entity)
            .into_iter()
            .filter(|edge| kinds.contains(&edge.kind))
            .collect();
        Ok(Some(GraphNode { entity, edges }))
    }

    /// Find the entities of a kind tagged with a value in any of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    async fn tagged(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Entity>, Error> {
        // list the tagged entities in only these groups
        let opts = tag_lookup(kind, key, value, groups);
        let mut cursor = self.entities.list_details(&opts).await?;
        // gather every page
        let mut found = std::mem::take(&mut cursor.data);
        while !cursor.exhausted() {
            cursor.refill().await?;
            found.append(&mut cursor.data);
        }
        Ok(found)
    }

    /// Find the ids of the entities of a kind tagged with a value in any of some groups from the
    /// entity list, which reads no entity
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    async fn tagged_ids(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Uuid>, Error> {
        // list the tagged ids in only these groups
        let opts = tag_lookup(kind, key, value, groups);
        let mut cursor = self.entities.list(&opts).await?;
        // gather every page, keeping each id once even when it's listed in several groups
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        loop {
            for line in cursor.data.drain(..) {
                if seen.insert(line.id) {
                    found.push(line.id);
                }
            }
            if cursor.exhausted() {
                break;
            }
            cursor.refill().await?;
        }
        Ok(found)
    }

    /// Find at most some number of the entities of a kind tagged with a value in any of some
    /// groups, without listing past the bound
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    /// * `limit` - The most entities to return
    async fn tagged_up_to(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
        limit: usize,
    ) -> Result<Vec<Entity>, Error> {
        // list the tagged entities in only these groups, up to the bound
        let opts = bounded_tag_lookup(kind, key, value, groups, limit);
        let mut cursor = self.entities.list_details(&opts).await?;
        // gather pages until the bound is reached or nothing is left
        let mut found = std::mem::take(&mut cursor.data);
        while found.len() < limit && !cursor.exhausted() {
            cursor.refill().await?;
            found.append(&mut cursor.data);
        }
        found.truncate(limit);
        Ok(found)
    }
}

/// Reads the graph through another reader, counting the reads and noting each entity an edge
/// led to that turned out not to exist
///
/// An edge's target that can't be found was deleted after the edge was read, and deleting an
/// entity also removes the edges that point at it, so a caller can retry the resolution instead
/// of recording the target as missing. Entities found any other way (an instance a workflow
/// names, a tool instance a spec names) aren't watched.
pub struct WatchedReader<'a, R: GraphReader + ?Sized> {
    /// The reader every read goes through
    inner: &'a R,
    /// Every entity an edge read so far led to
    targets: std::sync::Mutex<HashSet<Uuid>>,
    /// The edge targets that didn't exist when they were read
    missing: std::sync::Mutex<BTreeSet<Uuid>>,
    /// How many entities were read by id
    node_reads: std::sync::atomic::AtomicUsize,
    /// How many tag lookups were made
    tag_lookups: std::sync::atomic::AtomicUsize,
}

impl<'a, R: GraphReader + ?Sized> WatchedReader<'a, R> {
    /// Watch the reads made through a reader
    ///
    /// # Arguments
    ///
    /// * `inner` - The reader every read goes through
    pub fn new(inner: &'a R) -> Self {
        // nothing has been read yet
        WatchedReader {
            inner,
            targets: std::sync::Mutex::new(HashSet::new()),
            missing: std::sync::Mutex::new(BTreeSet::new()),
            node_reads: std::sync::atomic::AtomicUsize::new(0),
            tag_lookups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Get the edge targets that didn't exist when they were read
    #[must_use]
    pub fn missing_targets(&self) -> BTreeSet<Uuid> {
        // a poisoned lock still holds every id recorded before the panic
        match self.missing.lock() {
            Ok(missing) => missing.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Get how many entities were read by id
    #[must_use]
    pub fn node_reads(&self) -> usize {
        // read the counter
        self.node_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get how many tag lookups were made
    #[must_use]
    pub fn tag_lookups(&self) -> usize {
        // read the counter
        self.tag_lookups.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl<R: GraphReader + ?Sized> GraphReader for WatchedReader<'_, R> {
    /// Read an entity by id with its edges of some kinds in one direction, noting the entities
    /// its edges lead to and whether it was an edge target that's gone
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    /// * `kinds` - The edge kinds to return
    /// * `direction` - The direction of the edges to return
    async fn node(
        &self,
        id: Uuid,
        kinds: &[AssociationKind],
        direction: Directionality,
    ) -> Result<Option<GraphNode>, Error> {
        // count the read and make it
        self.node_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let node = self.inner.node(id, kinds, direction).await?;
        // a poisoned lock still holds every id recorded before the panic
        let mut targets = match self.targets.lock() {
            Ok(targets) => targets,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &node {
            // remember where this entity's edges lead
            Some(found) => targets.extend(found.edges.iter().map(|edge| edge.other)),
            // an edge's target that's gone was deleted after the edge was read
            None if targets.contains(&id) => {
                let mut missing = match self.missing.lock() {
                    Ok(missing) => missing,
                    Err(poisoned) => poisoned.into_inner(),
                };
                missing.insert(id);
            }
            None => (),
        }
        Ok(node)
    }

    /// Find the entities of a kind tagged with a value in any of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    async fn tagged(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Entity>, Error> {
        // count the lookup and make it
        self.tag_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.tagged(kind, key, value, groups).await
    }

    /// Find the ids of the entities of a kind tagged with a value in any of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    async fn tagged_ids(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Uuid>, Error> {
        // count the lookup and make it
        self.tag_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.tagged_ids(kind, key, value, groups).await
    }

    /// Find at most some number of the entities of a kind tagged with a value in any of some
    /// groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    /// * `limit` - The most entities to return
    async fn tagged_up_to(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
        limit: usize,
    ) -> Result<Vec<Entity>, Error> {
        // count the lookup and make it with the inner reader's bound
        self.tag_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner
            .tagged_up_to(kind, key, value, groups, limit)
            .await
    }
}

/// A workflow resolved into an instance that has no revision yet
///
/// The instance's revision is 0, it supersedes nothing, and its trigger is `Manual` with no
/// `triggered_by` until it's built ([`build_workflow_instance`]).
#[derive(Debug, Clone)]
pub struct ResolvedWorkflow {
    /// The workflow catalog, as it was read
    pub workflow: Entity,
    /// The resolved instance
    pub instance: WorkflowInstanceEntity,
    /// The tools the workflow's agents follow at latest through their own edges
    pub follows_tools: BTreeSet<Uuid>,
    /// The toolsets the workflow's agents follow at latest
    pub follows_tool_sets: BTreeSet<Uuid>,
}

impl ResolvedWorkflow {
    /// Get the sorted ids of the entities this instance uses
    #[must_use]
    pub fn referenced_ids(&self) -> BTreeSet<Uuid> {
        // the snapshot's ids plus the roots followed at latest
        referenced_ids(
            &self.instance,
            self.follows_tools.iter().copied(),
            self.follows_tool_sets.iter().copied(),
        )
    }

    /// Get this instance's dedupe key
    #[must_use]
    pub fn dedupe_key(&self) -> DedupeKey {
        // the digest, referenced ids, wiring, and status of the instance
        DedupeKey {
            digest: self.instance.digest.clone(),
            referenced: self.referenced_ids(),
            entry_agent: self.instance.entry_agent,
            delegations: delegations(&self.instance),
            errors: status_errors(&self.instance.status),
        }
    }
}

/// Collect each delegation in a workflow instance as its agent and sub-agent ids
///
/// # Arguments
///
/// * `instance` - The workflow instance
fn delegations(instance: &WorkflowInstanceEntity) -> BTreeSet<(Uuid, Uuid)> {
    // every agent paired with each agent it delegates to
    instance
        .agents
        .iter()
        .flat_map(|agent| agent.sub_agents.iter().map(|sub| (agent.agent, *sub)))
        .collect()
}

/// Get the sorted reasons a workflow instance is invalid, or none for a ready one
///
/// # Arguments
///
/// * `status` - The instance's status
fn status_errors(status: &WorkflowInstanceStatus) -> Vec<String> {
    // a ready instance has no errors
    let mut errors = match status {
        WorkflowInstanceStatus::Ready => Vec::new(),
        WorkflowInstanceStatus::Invalid { errors } => errors.clone(),
    };
    // sort them, so the order they were found in doesn't matter
    errors.sort_unstable();
    errors
}

/// Collect the ids of the entities a workflow instance uses
///
/// These are its agents, the tool and toolset roots it follows at latest, its pinned tool
/// instances, its toolset instances, and its harness instances.
///
/// # Arguments
///
/// * `instance` - The workflow instance
/// * `follows_tools` - The tools its agents follow at latest
/// * `follows_tool_sets` - The toolsets its agents follow at latest
fn referenced_ids(
    instance: &WorkflowInstanceEntity,
    follows_tools: impl Iterator<Item = Uuid>,
    follows_tool_sets: impl Iterator<Item = Uuid>,
) -> BTreeSet<Uuid> {
    // the agents and the roots followed at latest
    let mut ids = instance
        .agents
        .iter()
        .map(|agent| agent.agent)
        .collect::<BTreeSet<Uuid>>();
    ids.extend(follows_tools);
    ids.extend(follows_tool_sets);
    // the pinned tool instances
    ids.extend(
        instance
            .tools
            .iter()
            .filter(|tool| tool.pinned)
            .map(|tool| tool.tool_instance),
    );
    // each agent's toolset instances and harness
    for agent in &instance.agents {
        ids.extend(agent.tool_sets.iter().copied());
        ids.extend(agent.harness);
    }
    ids
}

/// What makes two workflow or toolset instances the same: their digest, the ids they reference
/// and how a workflow's agents are wired by id, and their status
///
/// Digests only cover content, so an agent or tool swapped for an identical copy keeps the
/// digest; the referenced ids and the wiring tell the two apart, so the new instance names
/// what it actually uses. A change that only makes an instance valid or invalid keeps every id
/// and the digest, so the status is compared too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupeKey {
    /// The instance's content digest
    pub digest: String,
    /// The sorted ids of the entities the instance uses
    pub referenced: BTreeSet<Uuid>,
    /// The id of a workflow instance's entry agent
    pub entry_agent: Option<Uuid>,
    /// Each delegation of a workflow instance as its agent and sub-agent ids
    pub delegations: BTreeSet<(Uuid, Uuid)>,
    /// The sorted reasons the instance is invalid, which a ready instance has none of
    pub errors: Vec<String>,
}

impl DedupeKey {
    /// Whether two keys match in everything but their digests
    ///
    /// # Arguments
    ///
    /// * `other` - The key to compare with
    #[must_use]
    pub fn same_except_digest(&self, other: &DedupeKey) -> bool {
        // the ids, wiring, and status must all match
        self.referenced == other.referenced
            && self.entry_agent == other.entry_agent
            && self.delegations == other.delegations
            && self.errors == other.errors
    }
}

impl DedupeKey {
    /// Get a stored workflow or toolset instance's dedupe key from its snapshot and tags
    ///
    /// Returns `None` for an entity that isn't a workflow or toolset instance.
    ///
    /// # Arguments
    ///
    /// * `entity` - The stored instance, with its tags
    #[must_use]
    pub fn of_stored(entity: &Entity) -> Option<DedupeKey> {
        // read the ids a tag holds
        let tagged = |key: &str| {
            entity
                .tags
                .get(key)
                .into_iter()
                .flat_map(|values| values.keys())
                .filter_map(|value| Uuid::parse_str(value).ok())
                .collect::<Vec<Uuid>>()
        };
        // each instance kind keeps its referenced ids in its own tags and snapshot
        match &entity.metadata {
            // a workflow instance's followed roots come from its tags and the rest from its
            // snapshot
            EntityMetadata::WorkflowInstance(instance) => Some(DedupeKey {
                digest: instance.digest.clone(),
                referenced: referenced_ids(
                    instance,
                    tagged(FOLLOWS_TOOL_TAG).into_iter(),
                    tagged(FOLLOWS_TOOL_SET_TAG).into_iter(),
                ),
                entry_agent: instance.entry_agent,
                delegations: delegations(instance),
                errors: status_errors(&instance.status),
            }),
            // a toolset instance's followed tools come from its tags and its members from its
            // snapshot
            EntityMetadata::ToolSetInstance(instance) => Some(DedupeKey {
                digest: instance.digest.clone(),
                referenced: crate::models::entities::toolsets::resolve::referenced_ids(
                    instance,
                    tagged(TOOL_SET_FOLLOWS_TOOL_TAG).into_iter(),
                ),
                entry_agent: None,
                delegations: BTreeSet::new(),
                errors: crate::models::entities::toolsets::resolve::status_errors(&instance.status),
            }),
            // nothing else has a dedupe key
            _ => None,
        }
    }
}

/// The outcome of building a workflow's next instance
#[derive(Debug, Clone)]
pub enum WorkflowBuild {
    /// The newest instance already has the resolved instance's dedupe key
    Current {
        /// The newest instance
        newest: Uuid,
        /// The workflow as it resolved
        resolved: Box<ResolvedWorkflow>,
    },
    /// A new instance to submit, with its revision, previous instance, and trigger set
    New(Box<ResolvedWorkflow>),
}

/// An agent the crawl took in
struct CrawledAgent {
    /// The agent, in all of its groups
    entity: Entity,
    /// The agent's metadata
    meta: AgentEntity,
    /// The agent's outgoing graph edges, sorted
    edges: Vec<GraphEdge>,
    /// The agents it delegates to that the crawl took in
    sub_agents: Vec<Uuid>,
}

/// What one agent was given
struct AgentTools {
    /// Its effective tool instances: its own tools plus its toolsets' members
    tools: Vec<Uuid>,
    /// The harness instance it runs in
    harness: Option<Uuid>,
    /// The toolset instances whose members it was given
    tool_sets: Vec<Uuid>,
    /// The mechanism each of its tools is bound to
    bindings: Vec<InstancedToolBinding>,
}

/// The effective tools an agent has been given so far
#[derive(Default)]
struct GivenTools {
    /// Every tool instance it was given
    ids: BTreeSet<Uuid>,
    /// The first instance of each tool it was given, and where that instance came from
    roots: HashMap<Uuid, (Uuid, String)>,
}

/// A tool instance the resolved instance lists
struct ToolEntry {
    /// The tool instance, in all of its groups
    entity: Entity,
    /// The tool instance's metadata
    meta: ToolInstanceEntity,
    /// Whether any agent or backing pins it
    pinned: bool,
    /// Whether an agent is given it or runs in it, rather than it only backing another tool
    direct: bool,
}

/// The state of one workflow's resolution
struct Resolver<'a, R: GraphReader + ?Sized> {
    /// Reads the graph
    reader: &'a R,
    /// The crawl limits
    limits: CrawlLimits,
    /// The workflow, in all of its groups
    workflow: Entity,
    /// Why the instance is invalid
    errors: Vec<String>,
    /// Non fatal issues
    warnings: Vec<String>,
    /// Every tool and tool instance read so far, or `None` when it doesn't exist
    read: HashMap<Uuid, Option<Entity>>,
    /// Every tool instance the instance lists
    tools: BTreeMap<Uuid, ToolEntry>,
    /// The tools followed at latest
    follows_tools: BTreeSet<Uuid>,
    /// The toolsets followed at latest
    follows_tool_sets: BTreeSet<Uuid>,
    /// The digest of every toolset instance an agent was given
    tool_set_digests: BTreeMap<Uuid, String>,
    /// The workflow's entry agent
    entry_agent: Option<Uuid>,
    /// The native tool instances each harness instance read with its edges provides
    provides: HashMap<Uuid, BTreeSet<Uuid>>,
}

/// Keep the outgoing edges of some kinds, sorted and without repeats
///
/// # Arguments
///
/// * `edges` - The edges a reader returned
/// * `kinds` - The kinds to keep
pub(crate) fn forward(edges: Vec<GraphEdge>, kinds: &[AssociationKind]) -> Vec<GraphEdge> {
    // a reverse row or an unasked kind is never followed
    let mut kept = edges
        .into_iter()
        .filter(|edge| edge.direction == Directionality::To && kinds.contains(&edge.kind))
        .collect::<Vec<GraphEdge>>();
    // sort so the crawl's order doesn't depend on the order rows were read in
    kept.sort_by(|left, right| {
        (left.kind.as_str(), left.other).cmp(&(right.kind.as_str(), right.other))
    });
    kept.dedup_by(|left, right| left.kind == right.kind && left.other == right.other);
    kept
}

/// Whether an entity covers an agent's groups, so the agent can call it
///
/// # Arguments
///
/// * `agent` - The agent
/// * `target` - The entity it calls
fn covers(agent: &CrawledAgent, target: &Entity) -> bool {
    // a call must be contained
    AssociationKind::Calls
        .group_policy(&agent.entity.groups, &target.groups)
        .is_ok()
}

/// Copy and sort a list of groups
///
/// # Arguments
///
/// * `groups` - The groups to sort
pub(crate) fn sorted(groups: &[String]) -> Vec<String> {
    // own and sort the groups
    let mut sorted = groups.to_vec();
    sorted.sort();
    sorted
}

impl<'a, R: GraphReader + ?Sized> Resolver<'a, R> {
    /// Start resolving a workflow
    ///
    /// # Arguments
    ///
    /// * `reader` - Reads the graph
    /// * `limits` - The crawl limits
    /// * `workflow` - The workflow, in all of its groups
    /// * `entry_agent` - The workflow's entry agent
    fn new(
        reader: &'a R,
        limits: CrawlLimits,
        workflow: Entity,
        entry_agent: Option<Uuid>,
    ) -> Self {
        Resolver {
            reader,
            limits,
            workflow,
            errors: Vec::new(),
            warnings: Vec::new(),
            read: HashMap::new(),
            tools: BTreeMap::new(),
            follows_tools: BTreeSet::new(),
            follows_tool_sets: BTreeSet::new(),
            tool_set_digests: BTreeMap::new(),
            entry_agent,
            provides: HashMap::new(),
        }
    }

    /// Record an error if a target's groups don't cover its source's
    ///
    /// Returns whether the target covers its source, since a target that doesn't is left out of
    /// the instance.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of edge, or `None` for an edge every group must cover
    /// * `source` - The source's name and groups
    /// * `target` - The target's name and groups
    fn check_contained(
        &mut self,
        kind: Option<AssociationKind>,
        source: (&str, &[String]),
        target: (&str, &[String]),
    ) -> bool {
        // find the source groups the target is missing under this kind's policy
        let missing = match kind {
            Some(kind) => kind
                .group_policy(source.1, target.1)
                .err()
                .unwrap_or_default(),
            None => helpers::missing_groups(source.1, target.1),
        };
        // a target missing any of them breaks containment
        if !missing.is_empty() {
            self.errors.push(format!(
                "`{}` is missing groups {missing:?} of `{}`, which references it",
                target.0, source.0
            ));
        }
        missing.is_empty()
    }

    /// Read a tool, tool instance, toolset, or toolset instance once, however many agents use it
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    async fn read(&mut self, id: Uuid) -> Result<Option<Entity>, Error> {
        // reuse an earlier read
        if let Some(found) = self.read.get(&id) {
            return Ok(found.clone());
        }
        // read the entity without any edges
        let found = self
            .reader
            .node(id, &[], Directionality::To)
            .await?
            .map(|node| node.entity);
        self.read.insert(id, found.clone());
        Ok(found)
    }

    /// Read the entry agent's harness instance with the native tools it provides
    ///
    /// Only the `Provides` edges leaving the instance are kept, so the crawl never moves from a
    /// native tool back to the harnesses that provide it.
    ///
    /// # Arguments
    ///
    /// * `id` - The harness instance to read
    async fn read_harness(&mut self, id: Uuid) -> Result<Option<Entity>, Error> {
        // reuse an earlier read that kept the instance's native tools
        if self.provides.contains_key(&id) {
            return Ok(self.read.get(&id).cloned().flatten());
        }
        // read the instance with its outgoing Provides edges
        let found = self
            .reader
            .node(id, &HARNESS_EDGES, Directionality::To)
            .await?;
        let entity = found.map(|node| {
            // keep the native tool instances it provides
            let provided = forward(node.edges, &HARNESS_EDGES)
                .into_iter()
                .filter(|edge| edge.other_kind == EntityKinds::ToolInstance)
                .map(|edge| edge.other)
                .collect::<BTreeSet<Uuid>>();
            self.provides.insert(id, provided);
            node.entity
        });
        self.read.insert(id, entity.clone());
        Ok(entity)
    }

    /// Get an entity this resolution already read
    ///
    /// # Arguments
    ///
    /// * `id` - The entity
    fn cached(&self, id: Uuid) -> Option<&Entity> {
        // an entity that was read and exists
        self.read.get(&id).and_then(Option::as_ref)
    }

    /// Read some entities a few at a time, skipping the ones already read
    ///
    /// # Arguments
    ///
    /// * `ids` - The entities to read
    async fn read_many(&mut self, ids: BTreeSet<Uuid>) -> Result<(), Error> {
        // only read what hasn't been read yet
        let wanted = ids
            .into_iter()
            .filter(|id| !self.read.contains_key(id))
            .collect::<Vec<Uuid>>();
        // read them concurrently, keeping each result as it arrives in order
        let reader = self.reader;
        let mut reads = futures::stream::iter(wanted)
            .map(|id| async move { (id, reader.node(id, &[], Directionality::To).await) })
            .buffered(PREFETCH_CONCURRENCY);
        while let Some((id, found)) = reads.next().await {
            self.read.insert(id, found?.map(|node| node.entity));
        }
        Ok(())
    }

    /// Read ahead, a few at a time, the entities resolving the agents reads one by one
    ///
    /// Only entities the resolution reads anyway are read, layer by layer: the tool and toolset
    /// targets of each agent whose edges fit its budget, then the latest instance of each tool
    /// and toolset, then the Image instances the contained tools' backings pin and the members
    /// of the ready, contained toolset instances that surely fit the agent's budget, and then
    /// the Image instances those members pin. Anything left is read when it's needed, so the
    /// same entities are read once each and the result doesn't change.
    ///
    /// # Arguments
    ///
    /// * `crawled` - The agents the crawl took in
    async fn prefetch(&mut self, crawled: &[CrawledAgent]) -> Result<(), Error> {
        // each agent's tool and toolset edges, when they fit its budget
        let budget = self.limits.max_listed_tools_per_agent();
        let agents = crawled
            .iter()
            .filter_map(|agent| {
                let edges = agent
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.kind == AssociationKind::Calls
                            && matches!(
                                edge.other_kind,
                                EntityKinds::Tool
                                    | EntityKinds::ToolInstance
                                    | EntityKinds::ToolSet
                                    | EntityKinds::ToolSetInstance
                            )
                    })
                    .collect::<Vec<&GraphEdge>>();
                (edges.len() <= budget).then_some((agent, edges))
            })
            .collect::<Vec<(&CrawledAgent, Vec<&GraphEdge>)>>();
        // read the targets of those edges
        let targets = agents
            .iter()
            .flat_map(|(_, edges)| edges.iter().map(|edge| edge.other))
            .collect::<BTreeSet<Uuid>>();
        self.read_many(targets).await?;
        // read the latest instance of each tool and toolset they target
        let latest = agents
            .iter()
            .flat_map(|(_, edges)| edges.iter())
            .filter_map(|edge| self.resolved_target(edge))
            .collect::<BTreeSet<Uuid>>();
        self.read_many(latest).await?;
        // read the Image instances the contained tools pin and the members the agents surely
        // get
        let mut next = BTreeSet::new();
        let mut given = Vec::new();
        for (agent, edges) in &agents {
            let (tools, sets): (Vec<&&GraphEdge>, Vec<&&GraphEdge>) =
                edges.iter().partition(|edge| {
                    matches!(
                        edge.other_kind,
                        EntityKinds::Tool | EntityKinds::ToolInstance
                    )
                });
            for edge in tools {
                next.extend(self.contained_dependency(agent, self.resolved_target(edge)));
            }
            // the members given so far never outnumber the tool edges and the members added
            let mut bound = edges.len() - sets.len();
            for edge in sets {
                // only a ready toolset instance that covers the agent gives it members
                let Some(entity) = self.resolved_target(edge).and_then(|id| self.cached(id)) else {
                    continue;
                };
                let EntityMetadata::ToolSetInstance(instance) = &entity.metadata else {
                    continue;
                };
                if !instance.is_ready() || !covers(agent, entity) {
                    continue;
                }
                // a toolset that might not fit what the agent was given ends the read ahead
                // for this agent
                if bound + instance.members.len() > budget {
                    break;
                }
                bound += instance.members.len();
                let members = instance
                    .members
                    .iter()
                    .map(|member| member.tool_instance)
                    .collect::<Vec<Uuid>>();
                next.extend(members.iter().copied());
                given.push((*agent, members));
            }
        }
        self.read_many(next).await?;
        // read the Image instances the contained members pin
        let dependencies = given
            .iter()
            .flat_map(|(agent, members)| {
                members
                    .iter()
                    .filter_map(|member| self.contained_dependency(agent, Some(*member)))
            })
            .collect::<BTreeSet<Uuid>>();
        self.read_many(dependencies).await
    }

    /// Get the instance an agent's tool or toolset edge resolves to from what was read
    ///
    /// # Arguments
    ///
    /// * `edge` - The agent's edge
    fn resolved_target(&self, edge: &GraphEdge) -> Option<Uuid> {
        // a root resolves to its latest instance and an instance is pinned as it is
        match (
            edge.other_kind,
            self.cached(edge.other).map(|root| &root.metadata),
        ) {
            (EntityKinds::Tool, Some(EntityMetadata::Tool(meta))) => meta.latest,
            (EntityKinds::ToolSet, Some(EntityMetadata::ToolSet(meta))) => meta.latest_instance,
            (EntityKinds::ToolInstance | EntityKinds::ToolSetInstance, _) => Some(edge.other),
            _ => None,
        }
    }

    /// Get the Image instance a tool instance an agent is given pins, when the instance was
    /// read and covers the agent's groups
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent
    /// * `id` - The tool instance
    fn contained_dependency(&self, agent: &CrawledAgent, id: Option<Uuid>) -> Option<Uuid> {
        // only a contained tool instance's backing is taken in
        let entity = self.cached(id?)?;
        match &entity.metadata {
            EntityMetadata::ToolInstance(meta) if covers(agent, entity) => {
                meta.spec.pinned_instance()
            }
            _ => None,
        }
    }

    /// Crawl the agents a workflow reaches, breadth first, within the crawl limits
    ///
    /// Each agent is read once. A delegation past the crawl depth or past the agent limit is
    /// recorded as an error and not followed.
    ///
    /// # Arguments
    ///
    /// * `roots` - The agents the workflow orchestrates
    async fn crawl_agents(&mut self, roots: &[Uuid]) -> Result<Vec<CrawledAgent>, Error> {
        // the limits the crawl stops at
        let max_agents = self.limits.max_agents_per_workflow;
        let max_depth = usize::from(self.limits.max_crawl_depth);
        // take in the orchestrated agents first, up to the agent limit
        let mut taken = HashSet::new();
        let mut overflow = HashSet::new();
        let mut too_deep = BTreeMap::new();
        let mut queue = VecDeque::new();
        for root in roots {
            if taken.len() >= max_agents {
                overflow.insert(*root);
            } else if taken.insert(*root) {
                queue.push_back((*root, 0_usize));
            }
        }
        // read each agent and take in the agents it delegates to
        let mut crawled = Vec::new();
        while let Some((id, depth)) = queue.pop_front() {
            // an agent that's gone or isn't an agent can't be resolved
            let Some(node) = self
                .reader
                .node(id, &AGENT_EDGES, Directionality::To)
                .await?
            else {
                self.errors.push(format!(
                    "workflow `{}` reaches agent `{id}`, which doesn't exist",
                    self.workflow.name
                ));
                continue;
            };
            let EntityMetadata::Agent(meta) = node.entity.metadata.clone() else {
                self.errors.push(format!(
                    "`{}` is a {}, not an agent",
                    node.entity.name, node.entity.kind
                ));
                continue;
            };
            // keep the agent's outgoing graph edges
            let edges = forward(node.edges, &AGENT_EDGES);
            // follow each delegation within the depth and agent limits
            let mut sub_agents = Vec::new();
            for edge in edges
                .iter()
                .filter(|edge| edge.kind == AssociationKind::DelegatesTo)
            {
                let target = edge.other;
                if taken.contains(&target) {
                    sub_agents.push(target);
                } else if depth + 1 > max_depth {
                    too_deep.entry(target).or_insert(depth + 1);
                } else if taken.len() >= max_agents {
                    overflow.insert(target);
                } else {
                    taken.insert(target);
                    queue.push_back((target, depth + 1));
                    sub_agents.push(target);
                }
            }
            // keep the agent with the delegations the crawl followed
            crawled.push(CrawledAgent {
                entity: node.entity,
                meta,
                edges,
                sub_agents,
            });
        }
        // a delegation to an agent that couldn't be read isn't kept
        let ids = crawled
            .iter()
            .map(|agent| agent.entity.id)
            .collect::<HashSet<Uuid>>();
        for agent in &mut crawled {
            agent.sub_agents.retain(|id| ids.contains(id));
        }
        // name the limits the crawl stopped at
        for (target, hops) in too_deep {
            self.errors.push(format!(
                "agent `{target}` is {hops} DelegatesTo hops from workflow `{}`'s agents, over max_crawl_depth ({max_depth})",
                self.workflow.name
            ));
        }
        if !overflow.is_empty() {
            self.errors.push(format!(
                "workflow `{}` reaches at least {} agents, over max_agents_per_workflow ({max_agents})",
                self.workflow.name,
                max_agents + overflow.len()
            ));
        }
        Ok(crawled)
    }

    /// Resolve the tools an agent is given, through its own edges and its toolsets, and the
    /// harness it runs in
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent
    async fn resolve_agent(&mut self, agent: &CrawledAgent) -> Result<AgentTools, Error> {
        // the agent's name, for errors
        let name = agent.entity.name.as_str();
        // sort the agent's call and harness edges by what they point to
        let mut tool_edges = Vec::new();
        let mut set_edges = Vec::new();
        let mut harness_edges = Vec::new();
        for edge in &agent.edges {
            match (edge.kind, edge.other_kind) {
                (AssociationKind::Calls, EntityKinds::Tool | EntityKinds::ToolInstance) => {
                    tool_edges.push(edge);
                }
                (AssociationKind::Calls, EntityKinds::ToolSet) => {
                    self.follows_tool_sets.insert(edge.other);
                    set_edges.push(edge);
                }
                (AssociationKind::Calls, EntityKinds::ToolSetInstance) => set_edges.push(edge),
                (AssociationKind::RunsIn, _) => harness_edges.push(edge),
                _ => (),
            }
        }
        // a tool or harness followed at latest through the agent's own edge is followed whatever
        // it resolves to
        self.follows_tools.extend(
            tool_edges
                .iter()
                .chain(&harness_edges)
                .filter(|edge| edge.other_kind == EntityKinds::Tool)
                .map(|edge| edge.other),
        );
        // resolve the tools, unless there are too many edges for them to fit the limit; a latest
        // edge and a pin can name the same instance, so at most half of them can repeat
        let max_tools = self.limits.max_tools_per_agent;
        let budget = self.limits.max_listed_tools_per_agent();
        let mut given = GivenTools::default();
        let mut tool_sets = Vec::new();
        let edges = tool_edges.len() + set_edges.len();
        if edges > budget {
            self.errors.push(format!(
                "agent `{name}` calls {edges} tools and toolsets, over max_tools_per_agent ({max_tools})"
            ));
        } else {
            // the agent's own tools come first
            for edge in tool_edges {
                if let Some(id) = self.resolve_tool(agent, edge).await? {
                    self.give(name, &mut given, id, "its own Calls edge");
                }
            }
            // then the members of each of its toolsets
            for edge in set_edges {
                if let Some(id) = self.give_tool_set(agent, edge, budget, &mut given).await? {
                    tool_sets.push(id);
                }
            }
            if given.ids.len() > max_tools {
                self.errors.push(format!(
                    "agent `{name}` is given {} tools, over max_tools_per_agent ({max_tools})",
                    given.ids.len()
                ));
            }
        }
        // the agent's own tool limit applies too
        if let Some(own) = agent.meta.config.max_tools
            && given.ids.len() > own as usize
        {
            self.errors.push(format!(
                "agent `{name}` is given {} tools, over its max_tools ({own})",
                given.ids.len()
            ));
        }
        // a model calls each of the agent's tools by a different function name
        self.check_function_names(name, &given.ids);
        // an agent runs in at most one harness, which must be a Harness tool
        let harness = match harness_edges.as_slice() {
            [] => None,
            [edge] => {
                // resolve the harness and check that it is one
                let harness = self.resolve_tool(agent, edge).await?;
                if let Some(id) = harness {
                    self.check_harness(name, id);
                }
                harness
            }
            _ => {
                self.errors.push(format!(
                    "agent `{name}` runs in {} harnesses, but an agent runs in one",
                    harness_edges.len()
                ));
                None
            }
        };
        tool_sets.sort_unstable();
        Ok(AgentTools {
            tools: given.ids.into_iter().collect(),
            harness,
            tool_sets,
            bindings: Vec::new(),
        })
    }

    /// Bind every agent's tools to a mechanism of the entry agent's harness instance
    ///
    /// Sub-agents run inside the entry agent's harness, so every agent's tools are bound against
    /// it. Each tool that can't be bound is an error, and every one is reported. Nothing is bound
    /// when the entry agent has no Harness instance to run in, which is already an error.
    ///
    /// # Arguments
    ///
    /// * `crawled` - The agents the crawl took in
    /// * `resolved` - What each agent was given, in the same order
    async fn bind_tools(
        &mut self,
        crawled: &[CrawledAgent],
        resolved: &mut [AgentTools],
    ) -> Result<(), Error> {
        // find the entry agent's harness instance
        let entry = crawled
            .iter()
            .zip(resolved.iter())
            .find(|(agent, _)| Some(agent.entity.id) == self.entry_agent)
            .and_then(|(_, given)| given.harness);
        let Some(harness_id) = entry else {
            return Ok(());
        };
        // read the native tools it provides, unless its read already kept them
        if !self.provides.contains_key(&harness_id) {
            self.read_harness(harness_id).await?;
        }
        let provided = self.provides.get(&harness_id).cloned().unwrap_or_default();
        // only a Harness instance can carry out tool calls
        let Some(harness_entry) = self.tools.get(&harness_id) else {
            return Ok(());
        };
        let ToolSpec::Harness(harness) = &harness_entry.meta.spec else {
            return Ok(());
        };
        let harness_name = harness_entry.entity.name.as_str();
        // bind each agent's tools, collecting every tool that can't be bound
        let mut errors = Vec::new();
        for (agent, given) in crawled.iter().zip(resolved.iter_mut()) {
            for id in &given.tools {
                let Some(entry) = self.tools.get(id) else {
                    continue;
                };
                let check = BindingCheck {
                    tool_name: &entry.entity.name,
                    tool: &entry.meta.spec,
                    harness_name,
                    harness,
                    provided: provided.contains(id),
                };
                match check.bind() {
                    Ok(mechanism) => given.bindings.push(InstancedToolBinding {
                        tool_instance: *id,
                        mechanism,
                    }),
                    Err(err) => errors.push(format!("agent `{}`: {err}", agent.entity.name)),
                }
            }
        }
        self.errors.extend(errors);
        Ok(())
    }

    /// Add a tool instance to an agent's effective tools
    ///
    /// The same instance given twice is kept once, and another instance of a tool the agent was
    /// already given is an error naming where each instance came from.
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent's name
    /// * `given` - The agent's effective tools so far
    /// * `id` - The tool instance to give
    /// * `source` - Where the instance came from, for errors
    fn give(&mut self, agent: &str, given: &mut GivenTools, id: Uuid, source: &str) {
        // only a listed tool instance can be given
        let Some(entry) = self.tools.get(&id) else {
            return;
        };
        let tool = entry.meta.tool;
        // a second instance of the same tool conflicts with the first
        match given.roots.get(&tool) {
            Some((first, first_source)) if *first != id => {
                let first_name = self
                    .tools
                    .get(first)
                    .map_or_else(|| first.to_string(), |first| first.entity.name.clone());
                self.errors.push(format!(
                    "agent `{agent}` is given tool `{tool}` at two instances: `{first_name}` from {first_source} and `{}` from {source}",
                    entry.entity.name
                ));
            }
            Some(_) => (),
            None => {
                given.roots.insert(tool, (id, source.to_owned()));
            }
        }
        given.ids.insert(id);
    }

    /// Give an agent the members of the toolset instance one of its toolset edges resolves to
    ///
    /// A latest edge resolves to the toolset's newest ready instance and a pinned edge to that
    /// instance, which must be ready. The members are used as the toolset instance snapshotted
    /// them. Returns the toolset instance when all of its members were given.
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent
    /// * `edge` - The agent's edge to a toolset (latest) or toolset instance (pinned)
    /// * `budget` - The most tools the agent may be considered for before it's refused unread
    /// * `given` - The agent's effective tools so far
    async fn give_tool_set(
        &mut self,
        agent: &CrawledAgent,
        edge: &GraphEdge,
        budget: usize,
        given: &mut GivenTools,
    ) -> Result<Option<Uuid>, Error> {
        // the agent's name, for errors
        let name = agent.entity.name.as_str();
        // a toolset followed at latest resolves to its newest ready instance
        let id = if edge.other_kind == EntityKinds::ToolSet {
            let Some(root) = self.read(edge.other).await? else {
                self.errors.push(format!(
                    "agent `{name}` calls toolset `{}`, which doesn't exist",
                    edge.other
                ));
                return Ok(None);
            };
            let EntityMetadata::ToolSet(meta) = &root.metadata else {
                self.errors
                    .push(format!("`{}` is a {}, not a toolset", root.name, root.kind));
                return Ok(None);
            };
            let Some(latest) = meta.latest_instance else {
                self.errors.push(format!(
                    "agent `{name}` follows toolset `{}` at latest, but it has no Ready instance",
                    root.name
                ));
                return Ok(None);
            };
            latest
        } else {
            edge.other
        };
        // the toolset instance must exist and be ready
        let Some(entity) = self.read(id).await? else {
            self.errors.push(format!(
                "agent `{name}` calls toolset instance `{id}`, which doesn't exist"
            ));
            return Ok(None);
        };
        let EntityMetadata::ToolSetInstance(instance) = &entity.metadata else {
            self.errors.push(format!(
                "`{}` is a {}, not a toolset instance",
                entity.name, entity.kind
            ));
            return Ok(None);
        };
        if !instance.is_ready() {
            self.errors.push(format!(
                "agent `{name}` calls toolset instance `{}` ({id}), which is {}; only a Ready instance can be given to an agent",
                entity.name,
                instance.status.as_str()
            ));
            return Ok(None);
        }
        // the toolset instance must cover the agent's groups, or it gives the agent nothing
        if !self.check_contained(
            Some(AssociationKind::Calls),
            (name, &agent.entity.groups),
            (&entity.name, &entity.groups),
        ) {
            return Ok(None);
        }
        // members past what the agent could ever be given aren't read
        let considered = given.ids.len() + instance.members.len();
        if considered > budget {
            self.errors.push(format!(
                "agent `{name}` is given at least {considered} tools with toolset instance `{}`, over max_tools_per_agent ({})",
                entity.name, self.limits.max_tools_per_agent
            ));
            return Ok(None);
        }
        // give each member at the instance the toolset snapshotted, pinned as the toolset was
        let source = format!("toolset instance `{}`", entity.name);
        let members = instance.members.clone();
        let digest = instance.digest.clone();
        let mut complete = true;
        for member in &members {
            let pinned = member.inclusion == ToolSetInclusion::ExplicitPinned;
            match self
                .take_instance(agent, AssociationKind::Calls, member.tool_instance, pinned)
                .await?
            {
                Some(taken) => self.give(name, given, taken, &source),
                None => complete = false,
            }
        }
        // a toolset instance is recorded only with all of its members
        if !complete {
            return Ok(None);
        }
        self.tool_set_digests.insert(id, digest);
        Ok(Some(id))
    }

    /// Record an error when an agent runs in a tool instance that isn't a Harness
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent's name
    /// * `id` - The tool instance it runs in
    fn check_harness(&mut self, agent: &str, id: Uuid) {
        // only a listed tool instance has a kind to check
        let Some(entry) = self.tools.get(&id) else {
            return;
        };
        // an agent only runs in a Harness tool instance
        let kind = entry.meta.spec.kind();
        if kind != ToolKind::Harness {
            self.errors.push(format!(
                "agent `{agent}` runs in `{}`, which is a {kind} tool, not a Harness tool",
                entry.entity.name
            ));
        }
    }

    /// Record an error for every delegation of an agent that a model couldn't be given as a
    /// delegation tool
    ///
    /// A sub-agent needs a function name and a model description, and each delegation tool's
    /// function name must differ from the agent's tools' and its other delegation tools'.
    ///
    /// # Arguments
    ///
    /// * `agent` - The delegating agent
    /// * `given` - The agent's effective tool instances
    /// * `agents` - Every crawled agent by id
    fn check_delegations(
        &mut self,
        agent: &CrawledAgent,
        given: &[Uuid],
        agents: &HashMap<Uuid, &CrawledAgent>,
    ) {
        // the agent's name, for errors
        let name = agent.entity.name.as_str();
        // map each of the agent's tools' function names to the tool's name
        let tools = given
            .iter()
            .filter_map(|id| self.tools.get(id))
            .filter_map(|entry| {
                entry
                    .meta
                    .spec
                    .function_name()
                    .map(|function| (function.to_owned(), entry.entity.name.clone()))
            })
            .collect::<HashMap<String, String>>();
        // check each sub-agent the crawl took in
        let mut delegations: HashMap<&str, &str> = HashMap::new();
        let mut errors = Vec::new();
        for sub in agent.sub_agents.iter().filter_map(|id| agents.get(id)) {
            // the sub-agent's name, for errors
            let sub_name = sub.entity.name.as_str();
            // a delegation tool is described by the sub-agent's model description
            if sub.meta.config.model_description.is_none() {
                errors.push(format!(
                    "agent `{name}` delegates to `{sub_name}`, which has no model_description"
                ));
            }
            // and called by its function name
            let Some(function) = sub.meta.config.function_name.as_deref() else {
                errors.push(format!(
                    "agent `{name}` delegates to `{sub_name}`, which has no function_name"
                ));
                continue;
            };
            // which none of the agent's tools may share
            if let Some(tool) = tools.get(function) {
                errors.push(format!(
                    "agent `{name}` has a tool and a delegation tool named `{function}`: `{tool}` and `{sub_name}`"
                ));
            }
            // nor another of its delegation tools
            match delegations.get(function) {
                Some(first) => errors.push(format!(
                    "agent `{name}` has two delegation tools named `{function}`: `{first}` and `{sub_name}`"
                )),
                None => {
                    delegations.insert(function, sub_name);
                }
            }
        }
        // record every problem found
        self.errors.extend(errors);
    }

    /// Record an error when an agent forces a call to a function it has no tool or delegation
    /// tool for
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent
    /// * `given` - The agent's effective tool instances
    /// * `agents` - Every crawled agent by id
    fn check_tool_choice(
        &mut self,
        agent: &CrawledAgent,
        given: &[Uuid],
        agents: &HashMap<Uuid, &CrawledAgent>,
    ) {
        // only a forced function choice names a tool
        let ToolChoice::Function(function) = &agent.meta.config.tool_choice else {
            return;
        };
        // one of its tools may have that function name
        let tool = given
            .iter()
            .filter_map(|id| self.tools.get(id))
            .any(|entry| entry.meta.spec.function_name() == Some(function.as_str()));
        // or one of the agents it delegates to may be called by it
        let delegation = agent
            .sub_agents
            .iter()
            .filter_map(|id| agents.get(id))
            .any(|sub| sub.meta.config.function_name.as_deref() == Some(function.as_str()));
        if !tool && !delegation {
            self.errors.push(format!(
                "agent `{}` forces a call to function `{function}`, but it has no tool or delegation tool by that name",
                agent.entity.name
            ));
        }
    }

    /// Resolve one of an agent's tool or harness edges to a tool instance
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent
    /// * `edge` - The agent's edge to a tool (latest) or tool instance (pinned)
    async fn resolve_tool(
        &mut self,
        agent: &CrawledAgent,
        edge: &GraphEdge,
    ) -> Result<Option<Uuid>, Error> {
        // the agent's name, for errors
        let name = agent.entity.name.as_str();
        match edge.other_kind {
            // a tool resolves to its latest instance
            EntityKinds::Tool => {
                let Some(root) = self.read(edge.other).await? else {
                    self.errors.push(format!(
                        "agent `{name}` calls tool `{}`, which doesn't exist",
                        edge.other
                    ));
                    return Ok(None);
                };
                let EntityMetadata::Tool(meta) = &root.metadata else {
                    self.errors
                        .push(format!("`{}` is a {}, not a tool", root.name, root.kind));
                    return Ok(None);
                };
                let Some(latest) = meta.latest else {
                    self.errors.push(format!(
                        "agent `{name}` follows tool `{}` at latest, but it has no latest instance (every instance is deprecated or it has none)",
                        root.name
                    ));
                    return Ok(None);
                };
                self.take_instance(agent, edge.kind, latest, false).await
            }
            // a tool instance is pinned as it is
            EntityKinds::ToolInstance => {
                self.take_instance(agent, edge.kind, edge.other, true).await
            }
            // nothing else is a tool
            other => {
                self.errors.push(format!(
                    "agent `{name}` has a {} edge to `{}`, which is a {other}, not a tool",
                    edge.kind, edge.other
                ));
                Ok(None)
            }
        }
    }

    /// Take a tool instance into the resolved instance for an agent
    ///
    /// A pinned deprecated instance is kept with a warning, and the Image instance a container
    /// backing pins is taken in as a dependency.
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent given the instance
    /// * `kind` - The kind of the agent's edge
    /// * `id` - The tool instance
    /// * `pinned` - Whether the agent pins it
    async fn take_instance(
        &mut self,
        agent: &CrawledAgent,
        kind: AssociationKind,
        id: Uuid,
        pinned: bool,
    ) -> Result<Option<Uuid>, Error> {
        // the agent's name, for errors
        let name = agent.entity.name.as_str();
        // an instance already given to an agent only needs its containment checked for this
        // one, without reading or copying it again
        if let Some(entry) = self.tools.get(&id)
            && entry.direct
        {
            let target = (entry.entity.name.clone(), entry.entity.groups.clone());
            // a deprecated pin is worth a warning for every agent that pins it
            if pinned && entry.meta.deprecated {
                self.warnings.push(format!(
                    "agent `{name}` pins deprecated tool instance `{}` ({id})",
                    target.0
                ));
            }
            if !self.check_contained(
                Some(kind),
                (name, &agent.entity.groups),
                (&target.0, &target.1),
            ) {
                return Ok(None);
            }
            if let Some(entry) = self.tools.get_mut(&id) {
                entry.pinned |= pinned;
            }
            return Ok(Some(id));
        }
        // the instance must exist and be a tool instance; the entry agent's harness is read with
        // the native tools it provides
        let found = if kind == AssociationKind::RunsIn && self.entry_agent == Some(agent.entity.id)
        {
            self.read_harness(id).await?
        } else {
            self.read(id).await?
        };
        let Some(entity) = found else {
            self.errors.push(format!(
                "agent `{name}` calls tool instance `{id}`, which doesn't exist"
            ));
            return Ok(None);
        };
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            self.errors.push(format!(
                "`{}` is a {}, not a tool instance",
                entity.name, entity.kind
            ));
            return Ok(None);
        };
        let meta = meta.clone();
        // a deprecated pin still works but is worth a warning
        if pinned && meta.deprecated {
            self.warnings.push(format!(
                "agent `{name}` pins deprecated tool instance `{}` ({id})",
                entity.name
            ));
        }
        // the instance must cover the agent's groups, or it isn't listed for the agent
        if !self.check_contained(
            Some(kind),
            (name, &agent.entity.groups),
            (&entity.name, &entity.groups),
        ) {
            return Ok(None);
        }
        // list the instance once, given directly
        let backing = meta.spec.pinned_instance();
        let owner = entity.name.clone();
        let entry = self.tools.entry(id).or_insert(ToolEntry {
            entity,
            meta,
            pinned: false,
            direct: false,
        });
        entry.pinned |= pinned;
        entry.direct = true;
        // a container backing depends on the Image instance it pins
        if let Some(dependency) = backing {
            self.take_dependency(&owner, dependency).await?;
        }
        Ok(Some(id))
    }

    /// Take in the Image instance a container backing pins
    ///
    /// # Arguments
    ///
    /// * `owner` - The name of the tool instance whose backing pins it
    /// * `id` - The pinned tool instance
    async fn take_dependency(&mut self, owner: &str, id: Uuid) -> Result<(), Error> {
        // a dependency already listed only needs marking as pinned
        if let Some(entry) = self.tools.get_mut(&id) {
            entry.pinned = true;
            return Ok(());
        }
        // the dependency must exist and be a tool instance
        let Some(entity) = self.read(id).await? else {
            self.errors.push(format!(
                "tool instance `{owner}` is backed by tool instance `{id}`, which doesn't exist"
            ));
            return Ok(());
        };
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            self.errors.push(format!(
                "`{}` backs `{owner}` but is a {}, not a tool instance",
                entity.name, entity.kind
            ));
            return Ok(());
        };
        // list it as only a dependency until an agent is given it
        let meta = meta.clone();
        self.tools.insert(
            id,
            ToolEntry {
                entity,
                meta,
                pinned: true,
                direct: false,
            },
        );
        Ok(())
    }

    /// Record an error for every function name two of an agent's tools share
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent's name
    /// * `given` - The agent's tool instances
    fn check_function_names(&mut self, agent: &str, given: &BTreeSet<Uuid>) {
        // map each function name to the first tool that uses it
        let mut names: HashMap<&str, &str> = HashMap::new();
        let mut collisions = Vec::new();
        for id in given {
            let Some(entry) = self.tools.get(id) else {
                continue;
            };
            let Some(function) = entry.meta.spec.function_name() else {
                continue;
            };
            match names.get(function) {
                Some(first) => collisions.push(format!(
                    "agent `{agent}` gives two tools the function name `{function}`: `{first}` and `{}`",
                    entry.entity.name
                )),
                None => {
                    names.insert(function, &entry.entity.name);
                }
            }
        }
        self.errors.extend(collisions);
    }

    /// Record an error for every edge from the workflow or between agents that isn't contained,
    /// leaving those edges and the agents missing a workflow group out
    ///
    /// Every agent in an instance is snapshotted in all of the workflow's groups, so an agent
    /// that isn't in one of them is left out; an agent reached through a delegation that isn't
    /// contained is named once even when it's in every workflow group.
    ///
    /// # Arguments
    ///
    /// * `crawled` - The agents the crawl took in
    /// * `roots` - The agents the workflow orchestrates
    fn check_agent_edges(&mut self, crawled: &mut Vec<CrawledAgent>, roots: &[Uuid]) {
        // index each agent's name and groups by id
        let groups = crawled
            .iter()
            .map(|agent| {
                (
                    agent.entity.id,
                    (agent.entity.name.clone(), agent.entity.groups.clone()),
                )
            })
            .collect::<HashMap<Uuid, (String, Vec<String>)>>();
        let workflow = (self.workflow.name.clone(), self.workflow.groups.clone());
        // each sub-agent covers its delegator's groups, or the delegation is left out
        let mut named = HashSet::new();
        for agent in crawled.iter_mut() {
            let source = (agent.entity.name.clone(), agent.entity.groups.clone());
            let mut kept = Vec::with_capacity(agent.sub_agents.len());
            for sub in std::mem::take(&mut agent.sub_agents) {
                let Some((name, sub_groups)) = groups.get(&sub) else {
                    continue;
                };
                if self.check_contained(
                    Some(AssociationKind::DelegatesTo),
                    (&source.0, &source.1),
                    (name, sub_groups),
                ) {
                    kept.push(sub);
                } else {
                    named.insert(sub);
                }
            }
            agent.sub_agents = kept;
        }
        // each agent covers the workflow's groups, or it's left out of the instance
        let mut dropped = HashSet::new();
        for agent in crawled.iter() {
            let id = agent.entity.id;
            let target = (agent.entity.name.as_str(), agent.entity.groups.as_slice());
            let missing = helpers::missing_groups(&workflow.1, target.1);
            if missing.is_empty() {
                continue;
            }
            dropped.insert(id);
            // name an orchestrated agent, or one no delegation error named already
            if roots.contains(&id) || !named.contains(&id) {
                self.check_contained(
                    Some(AssociationKind::Orchestrates),
                    (&workflow.0, &workflow.1),
                    target,
                );
            }
        }
        crawled.retain(|agent| !dropped.contains(&agent.entity.id));
        for agent in crawled.iter_mut() {
            agent.sub_agents.retain(|sub| !dropped.contains(sub));
        }
    }

    /// Build the instance's tools, leaving out every dependency that doesn't cover the
    /// workflow's groups
    fn instanced_tools(&mut self) -> Vec<InstancedTool> {
        // every dependency must cover the workflow's groups
        let workflow = (self.workflow.name.clone(), self.workflow.groups.clone());
        let dependencies = self
            .tools
            .iter()
            .filter(|(_, entry)| !entry.direct)
            .map(|(id, entry)| (*id, entry.entity.name.clone(), entry.entity.groups.clone()))
            .collect::<Vec<(Uuid, String, Vec<String>)>>();
        let mut uncontained = HashSet::new();
        for (id, name, groups) in dependencies {
            if !self.check_contained(None, (&workflow.0, &workflow.1), (&name, &groups)) {
                uncontained.insert(id);
            }
        }
        // describe each listed instance as it's stored
        let mut tools = Vec::with_capacity(self.tools.len());
        for (id, entry) in &self.tools {
            if uncontained.contains(id) {
                continue;
            }
            let dependency = !entry.direct;
            tools.push(InstancedTool {
                tool: entry.meta.tool,
                tool_instance: *id,
                name: entry.entity.name.clone(),
                groups: sorted(&entry.entity.groups),
                kind: entry.meta.spec.kind(),
                version: entry.meta.version.to_string(),
                spec_hash: entry.meta.spec_hash.clone(),
                function_name: entry.meta.spec.function_name().map(ToOwned::to_owned),
                pinned: entry.pinned,
                dependency,
            });
        }
        tools
    }
}

/// Describe each crawled agent as it's snapshotted into the instance, with its toolset digest
///
/// # Arguments
///
/// * `crawled` - The agents the crawl took in
/// * `resolved` - What each agent was given, in the same order
/// * `tools` - The instance's tools
fn instanced_agents(
    crawled: Vec<CrawledAgent>,
    resolved: Vec<AgentTools>,
    tools: &[InstancedTool],
) -> Result<Vec<InstancedAgent>, Error> {
    // snapshot each agent with what it was given
    let mut agents = Vec::with_capacity(crawled.len());
    for (agent, given) in crawled.into_iter().zip(resolved) {
        let mut instanced = InstancedAgent {
            agent: agent.entity.id,
            name: agent.entity.name,
            groups: sorted(&agent.entity.groups),
            config: agent.meta.config,
            config_hash: agent.meta.config_hash,
            harness: given.harness,
            tools: given.tools,
            tool_sets: given.tool_sets,
            bindings: given.bindings,
            sub_agents: agent.sub_agents,
            toolset_digest: String::new(),
        };
        // the toolset digest covers the agent's tools
        instanced.toolset_digest = instanced
            .compute_toolset_digest(tools)
            .map_err(|err| Error::new(format!("failed to hash an agent's tools: {err}")))?;
        agents.push(instanced);
    }
    Ok(agents)
}

/// Resolve a workflow catalog's graph into an instance
///
/// Problems with the graph make the instance `Invalid` rather than failing; only a read that
/// fails, a workflow that doesn't exist, or an entity that isn't a workflow catalog is an error.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `workflow` - The workflow catalog to resolve
/// * `limits` - The crawl limits
pub async fn resolve_workflow<R: GraphReader + ?Sized>(
    reader: &R,
    workflow: Uuid,
    limits: &CrawlLimits,
) -> Result<ResolvedWorkflow, Error> {
    // read the workflow and the agents it orchestrates
    let Some(node) = reader
        .node(workflow, &WORKFLOW_EDGES, Directionality::To)
        .await?
    else {
        return Err(Error::new(format!("workflow {workflow} doesn't exist")));
    };
    let EntityMetadata::WorkflowCatalog(catalog) = &node.entity.metadata else {
        return Err(Error::new(format!(
            "`{}` is a {}, not a workflow catalog",
            node.entity.name, node.entity.kind
        )));
    };
    let catalog: WorkflowCatalogEntity = catalog.clone();
    let roots = forward(node.edges, &WORKFLOW_EDGES)
        .into_iter()
        .map(|edge| edge.other)
        .collect::<Vec<Uuid>>();
    let mut resolver = Resolver::new(reader, *limits, node.entity, catalog.entry_agent);
    let name = resolver.workflow.name.clone();
    // a workflow needs agents and an entry agent it orchestrates
    if roots.is_empty() {
        resolver
            .errors
            .push(format!("workflow `{name}` has no agents"));
    }
    match catalog.entry_agent {
        None => resolver
            .errors
            .push(format!("workflow `{name}` has no entry agent")),
        Some(entry) if !roots.contains(&entry) => resolver.errors.push(format!(
            "entry agent `{entry}` is not one of workflow `{name}`'s agents"
        )),
        Some(_) => (),
    }
    // crawl the agents and check that each edge between them is contained
    let mut crawled = resolver.crawl_agents(&roots).await?;
    resolver.check_agent_edges(&mut crawled, &roots);
    // read ahead what resolving the agents reads
    resolver.prefetch(&crawled).await?;
    // resolve each agent's tools, toolsets, and harness
    let mut resolved_agents = Vec::with_capacity(crawled.len());
    for agent in &crawled {
        resolved_agents.push(resolver.resolve_agent(agent).await?);
    }
    // the entry agent must run in a harness
    if let Some(entry) = crawled
        .iter()
        .find(|agent| Some(agent.entity.id) == catalog.entry_agent)
        && entry
            .edges
            .iter()
            .all(|edge| edge.kind != AssociationKind::RunsIn)
    {
        resolver.errors.push(format!(
            "entry agent `{}` has no harness to run in",
            entry.entity.name
        ));
    }
    // every agent's tools must be bound to a mechanism of the entry agent's harness
    resolver.bind_tools(&crawled, &mut resolved_agents).await?;
    // every delegation must be a valid delegation tool, and a forced function choice must name
    // one of the agent's tools or delegation tools
    let by_id = crawled
        .iter()
        .map(|agent| (agent.entity.id, agent))
        .collect::<HashMap<Uuid, &CrawledAgent>>();
    for (agent, given) in crawled.iter().zip(&resolved_agents) {
        resolver.check_delegations(agent, &given.tools, &by_id);
        resolver.check_tool_choice(agent, &given.tools, &by_id);
    }
    // describe the tools, then each agent with the digest of its tools
    let tools = resolver.instanced_tools();
    let agents = instanced_agents(crawled, resolved_agents, &tools)?;
    // build the instance, sort its lists, and digest it
    let status = if resolver.errors.is_empty() {
        WorkflowInstanceStatus::Ready
    } else {
        WorkflowInstanceStatus::Invalid {
            errors: std::mem::take(&mut resolver.errors),
        }
    };
    let mut instance = WorkflowInstanceEntity {
        workflow,
        workflow_groups: sorted(&resolver.workflow.groups),
        revision: 0,
        previous: None,
        status,
        warnings: std::mem::take(&mut resolver.warnings),
        trigger: WorkflowInstanceTrigger::Manual,
        triggered_by: String::new(),
        entry_agent: catalog.entry_agent,
        limits: catalog.limits,
        default_models: catalog.default_models,
        agents,
        tools,
        digest: String::new(),
    };
    instance.sort_lists();
    instance.digest = instance
        .compute_digest(&resolver.tool_set_digests)
        .map_err(|err| Error::new(format!("failed to hash a workflow instance: {err}")))?;
    Ok(ResolvedWorkflow {
        workflow: resolver.workflow,
        instance,
        follows_tools: resolver.follows_tools,
        follows_tool_sets: resolver.follows_tool_sets,
    })
}

/// Find a workflow's newest instance, whatever its status
///
/// The workflow names its newest instance, so only that instance is read, however many
/// revisions the workflow has. When the named instance is gone, or the workflow names none
/// after giving out revisions, its instances are found by their `WorkflowId` tag instead.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `workflow` - The workflow catalog, in all of its groups
pub async fn newest_instance<R: GraphReader + ?Sized>(
    reader: &R,
    workflow: &Entity,
) -> Result<Option<Entity>, Error> {
    // only a workflow catalog has instances
    let EntityMetadata::WorkflowCatalog(catalog) = &workflow.metadata else {
        return Ok(None);
    };
    // keep an entity only when it's an instance of this workflow
    let of_workflow = |entity: &Entity| match &entity.metadata {
        EntityMetadata::WorkflowInstance(instance) => instance.workflow == workflow.id,
        _ => false,
    };
    // read the instance the workflow names, which a workflow that never gave out a revision
    // doesn't have
    match catalog.newest_instance {
        Some(id) => {
            let named = reader
                .node(id, &[], Directionality::To)
                .await?
                .map(|node| node.entity)
                .filter(of_workflow);
            if named.is_some() {
                return Ok(named);
            }
        }
        None if catalog.highest_revision == 0 => return Ok(None),
        None => (),
    }
    // look up the workflow's instances by tag and keep the one with the highest revision
    let found = reader
        .tagged(
            EntityKinds::WorkflowInstance,
            WORKFLOW_ID_TAG,
            &workflow.id.to_string(),
            &workflow.groups,
        )
        .await?;
    let newest = found
        .into_iter()
        .filter(of_workflow)
        .filter_map(|entity| match &entity.metadata {
            EntityMetadata::WorkflowInstance(instance) => Some((instance.revision, entity)),
            _ => None,
        })
        .max_by_key(|(revision, _)| *revision)
        .map(|(_, entity)| entity);
    Ok(newest)
}

/// Read the digest of each toolset instance a stored workflow instance's agents were given
///
/// Each digest is used as it's stored, as a fresh resolve uses it, since a scheme change
/// rebuilds no toolset instance. Returns `None` when one of them is gone, since the stored
/// instance can't be recomputed then.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `instance` - The stored workflow instance
async fn toolset_digests<R: GraphReader + ?Sized>(
    reader: &R,
    instance: &WorkflowInstanceEntity,
) -> Result<Option<BTreeMap<Uuid, String>>, Error> {
    // read each toolset instance once
    let ids = instance
        .agents
        .iter()
        .flat_map(|agent| agent.tool_sets.iter().copied())
        .collect::<BTreeSet<Uuid>>();
    let mut digests = BTreeMap::new();
    for id in ids {
        match reader.node(id, &[], Directionality::To).await? {
            Some(GraphNode {
                entity:
                    Entity {
                        metadata: EntityMetadata::ToolSetInstance(tool_set),
                        ..
                    },
                ..
            }) => {
                digests.insert(id, tool_set.digest);
            }
            // a toolset instance that's gone leaves nothing to recompute from
            _ => return Ok(None),
        }
    }
    Ok(Some(digests))
}

/// Check whether a stored instance has a candidate's dedupe key under a scheme
///
/// A stored digest under an older scheme is recomputed from its snapshot, never rewritten.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `scheme` - The active digest scheme
/// * `candidate` - The candidate's dedupe key, digested under the active scheme
/// * `stored` - The stored instance, with its tags
pub(crate) async fn is_current_with_scheme<R: GraphReader + ?Sized>(
    reader: &R,
    scheme: &str,
    candidate: &DedupeKey,
    stored: &Entity,
) -> Result<bool, Error> {
    // only a workflow instance with the same referenced ids, wiring, and status can be current
    let Some(key) = DedupeKey::of_stored(stored) else {
        return Ok(false);
    };
    let EntityMetadata::WorkflowInstance(instance) = &stored.metadata else {
        return Ok(false);
    };
    if !key.same_except_digest(candidate) {
        return Ok(false);
    }
    // a digest under the active scheme compares directly
    if ThorDigest::scheme(&key.digest) == Some(scheme) {
        return Ok(key.digest == candidate.digest);
    }
    // an older digest is recomputed from the snapshot under the active scheme, and one whose
    // toolset instances are gone can't be current
    let Some(digests) = toolset_digests(reader, instance).await? else {
        return Ok(false);
    };
    ThorDigest::same_definition_with_scheme(
        scheme,
        &key.digest,
        || {
            instance
                .compute_digest_with_scheme(scheme, &digests)
                .map_err(|err| Error::new(format!("failed to hash a workflow instance: {err}")))
        },
        &candidate.digest,
    )
}

/// Build a workflow's next instance, or find that its newest instance is current
///
/// The newest instance, of any status, is current when it has the resolved instance's digest,
/// referenced ids, wiring, and status. Otherwise the new instance takes the revision after both the newest
/// instance's and the highest revision the workflow ever gave out, and supersedes the newest
/// instance. Nothing is written.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `workflow` - The workflow catalog to build
/// * `limits` - The crawl limits
/// * `trigger` - What caused the build
/// * `triggered_by` - The user whose change or request caused the build
pub async fn build_workflow_instance<R: GraphReader + ?Sized>(
    reader: &R,
    workflow: Uuid,
    limits: &CrawlLimits,
    trigger: WorkflowInstanceTrigger,
    triggered_by: &str,
) -> Result<WorkflowBuild, Error> {
    // resolve the graph and find the newest instance
    let mut resolved = resolve_workflow(reader, workflow, limits).await?;
    let newest = newest_instance(reader, &resolved.workflow).await?;
    // an unchanged graph builds nothing
    if let Some(newest) = &newest
        && is_current_with_scheme(reader, ThorDigest::SCHEME, &resolved.dedupe_key(), newest)
            .await?
    {
        return Ok(WorkflowBuild::Current {
            newest: newest.id,
            resolved: Box::new(resolved),
        });
    }
    // the next revision follows every revision given out so far
    let newest_revision = newest
        .as_ref()
        .and_then(|entity| match &entity.metadata {
            EntityMetadata::WorkflowInstance(instance) => Some(instance.revision),
            _ => None,
        })
        .unwrap_or(0);
    let highest = match &resolved.workflow.metadata {
        EntityMetadata::WorkflowCatalog(catalog) => catalog.highest_revision,
        _ => 0,
    };
    resolved.instance.revision = newest_revision.max(highest) + 1;
    resolved.instance.previous = newest.map(|entity| entity.id);
    resolved.instance.trigger = trigger;
    resolved.instance.triggered_by = triggered_by.to_owned();
    Ok(WorkflowBuild::New(Box::new(resolved)))
}
