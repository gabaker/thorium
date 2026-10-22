//! Builds workflow instances for workflow graph events
//!
//! Each event maps to the workflows it affects: the workflow it names, or the workflows whose
//! newest instance carries the tag a lookup in the event's groups finds. The events for one
//! workflow are coalesced into one build, recorded as the latest qualifying event's, and a build
//! creates a new instance only when the workflow's newest instance isn't already current. The
//! users who caused a build are told how it ended.

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thorium::Error;
use thorium::models::entities::workflows::{
    FOLLOWS_TOOL_SET_TAG, FOLLOWS_TOOL_TAG, GraphReader, INSTANCE_AGENT_TAG, WatchedReader,
    WorkflowBuild, build_workflow_instance,
};
use thorium::models::{
    AssociationKind, CrawlLimits, Directionality, EntityKinds, EntityMetadata, Event, EventData,
    NotificationLevel, NotificationRequest, User, WorkflowCatalogEntity, WorkflowGraphChange,
    WorkflowInstanceEntity, WorkflowInstanceStatus, WorkflowInstanceTrigger,
};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::{
    Affected, AiWorkerStats, BuildCause, InstanceApi, PhaseOutcome, Reach, notify_all, tagged_roots,
};

#[cfg(test)]
mod tests;

/// How many workflows of a batch are built at once
const BUILD_CONCURRENCY: usize = 8;

/// How many tag lookups of a batch run at once
const LOOKUP_CONCURRENCY: usize = 8;

/// How many times a build that loses a revision race is rebuilt before its events are retried
/// later
const MAX_BUILD_ATTEMPTS: usize = 3;

/// One workflow a batch of events affects, and what its build records
pub type AffectedWorkflow = Affected<WorkflowInstanceTrigger>;

/// The workflows a batch of events affects
#[derive(Debug, Default)]
pub struct Mapping {
    /// Each affected workflow by id
    pub workflows: BTreeMap<Uuid, AffectedWorkflow>,
    /// The events whose tag lookup failed, which stay in flight
    pub failed: BTreeSet<Uuid>,
    /// The events that affect no workflow, which are finished
    pub unmapped: Vec<Uuid>,
    /// How many tag lookups were made
    pub tag_lookups: usize,
    /// How many instances were read by id to find their workflows
    pub graph_reads: usize,
}

/// The intrinsic tag a workflow's instances are looked up by
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LookupTag {
    /// The tag naming each agent in an instance
    Agent,
    /// The tag naming each tool an instance follows at latest
    Tool,
    /// The tag naming each toolset an instance follows at latest
    ToolSet,
}

impl LookupTag {
    /// Get the tag's key
    fn key(self) -> &'static str {
        // each lookup reads its own intrinsic tag
        match self {
            LookupTag::Agent => INSTANCE_AGENT_TAG,
            LookupTag::Tool => FOLLOWS_TOOL_TAG,
            LookupTag::ToolSet => FOLLOWS_TOOL_SET_TAG,
        }
    }
}

/// Where a change leads before any lookup
enum Target {
    /// The change names its workflow
    Workflow(Uuid),
    /// The change reaches the workflows whose newest instance carries a tag
    Tagged {
        /// The tag
        key: LookupTag,
        /// The tag value
        value: Uuid,
    },
    /// The change affects no workflow directly
    Nothing,
}

/// Find where a change leads
///
/// # Arguments
///
/// * `change` - The change
fn target(change: &WorkflowGraphChange) -> Target {
    // workflow changes name their workflow; agent, tool, and toolset changes are looked up by
    // the tag their workflows' instances carry; toolset changes rebuild toolsets, not workflows
    match change {
        WorkflowGraphChange::WorkflowUpdated { workflow }
        | WorkflowGraphChange::ManualInstance { workflow } => Target::Workflow(*workflow),
        WorkflowGraphChange::EdgeChanged {
            source,
            kind: AssociationKind::Orchestrates,
            ..
        } => Target::Workflow(*source),
        WorkflowGraphChange::EdgeChanged {
            source,
            kind: AssociationKind::DelegatesTo | AssociationKind::Calls | AssociationKind::RunsIn,
            ..
        }
        | WorkflowGraphChange::AgentUpdated { agent: source } => Target::Tagged {
            key: LookupTag::Agent,
            value: *source,
        },
        WorkflowGraphChange::ToolUpdated { tool, .. } => Target::Tagged {
            key: LookupTag::Tool,
            value: *tool,
        },
        WorkflowGraphChange::ToolSetUpdated { tool_set, .. } => Target::Tagged {
            key: LookupTag::ToolSet,
            value: *tool_set,
        },
        WorkflowGraphChange::EdgeChanged { .. }
        | WorkflowGraphChange::ToolSetChanged { .. }
        | WorkflowGraphChange::ManualToolSetInstance { .. }
        | WorkflowGraphChange::ToolTagsChanged { .. } => Target::Nothing,
    }
}

/// Get the trigger an instance built for a change records
///
/// # Arguments
///
/// * `change` - The change
fn trigger(change: &WorkflowGraphChange) -> WorkflowInstanceTrigger {
    // each change records what it changed
    match change {
        WorkflowGraphChange::ManualInstance { .. } => WorkflowInstanceTrigger::Manual,
        WorkflowGraphChange::EdgeChanged {
            source,
            kind: AssociationKind::DelegatesTo | AssociationKind::Calls | AssociationKind::RunsIn,
            ..
        }
        | WorkflowGraphChange::AgentUpdated { agent: source } => {
            WorkflowInstanceTrigger::AgentUpdated { agent: *source }
        }
        WorkflowGraphChange::ToolUpdated {
            tool,
            tool_instance,
        } => WorkflowInstanceTrigger::ToolUpdated {
            tool: *tool,
            tool_instance: *tool_instance,
        },
        WorkflowGraphChange::ToolSetUpdated {
            tool_set,
            tool_set_instance,
        } => WorkflowInstanceTrigger::ToolSetUpdated {
            tool_set: *tool_set,
            tool_set_instance: *tool_set_instance,
        },
        // the catalog and its Orchestrates edges, and changes that build no workflow
        _ => WorkflowInstanceTrigger::WorkflowUpdated,
    }
}

/// Build the notification for a created instance: Info when it's Ready, or an Error listing why
/// it's Invalid
///
/// # Arguments
///
/// * `name` - The workflow catalog's name
/// * `instance` - The created instance
/// * `id` - The created instance's id
fn built_note(
    name: &str,
    instance: &WorkflowInstanceEntity,
    id: Uuid,
) -> NotificationRequest<User> {
    // name the catalog, the revision, and the instance, and list the errors of an invalid one
    let revision = instance.revision;
    match &instance.status {
        WorkflowInstanceStatus::Ready => NotificationRequest::new(
            format!("workflow `{name}` #r{revision} (`{id}`) is Ready"),
            NotificationLevel::Info,
        ),
        WorkflowInstanceStatus::Invalid { errors } => NotificationRequest::new(
            format!(
                "workflow `{name}` #r{revision} (`{id}`) is Invalid: {}",
                errors.join("; ")
            ),
            NotificationLevel::Error,
        ),
    }
}

/// The key of one tag lookup: the tag, its value, and the sorted groups searched
type LookupKey = (LookupTag, Uuid, Vec<String>);

/// A tag lookup, the workflows and instances it found, and how many lookups and reads by id it
/// made
type LookupResult = (LookupKey, Result<Vec<(Uuid, Uuid)>, Error>, (usize, usize));

/// An affected workflow and the stats of its build
type BuildResult<'b> = (&'b AffectedWorkflow, Result<AiWorkerStats, Error>);

/// Builds the workflow instances a batch of workflow graph events calls for
pub struct WorkflowInstancer<'a, A: InstanceApi + ?Sized> {
    /// The API reads and writes go through
    api: &'a A,
    /// The crawl limits workflows are resolved under
    limits: CrawlLimits,
}

impl<'a, A: InstanceApi + ?Sized> WorkflowInstancer<'a, A> {
    /// Build an instancer over an API
    ///
    /// # Arguments
    ///
    /// * `api` - The API reads and writes go through
    /// * `limits` - The crawl limits workflows are resolved under
    pub fn new(api: &'a A, limits: CrawlLimits) -> Self {
        // nothing is read until a batch is processed
        WorkflowInstancer { api, limits }
    }

    /// Find the workflows a tag lookup matches, with each instance that matched, and count the
    /// lookups and reads by id it made
    ///
    /// # Arguments
    ///
    /// * `key` - The tag to look up
    /// * `value` - The tag value
    /// * `groups` - The groups to search, every group of the changed entity
    async fn lookup(
        &self,
        key: LookupTag,
        value: Uuid,
        groups: &[String],
    ) -> (Result<Vec<(Uuid, Uuid)>, Error>, (usize, usize)) {
        // find the tagged instances' ids in only the event's groups, and pair each with its
        // workflow
        let watched = WatchedReader::new(self.api.reader());
        let found = tagged_roots(
            &watched,
            EntityKinds::WorkflowInstance,
            key.key(),
            &value.to_string(),
            groups,
        )
        .await;
        (found, (watched.tag_lookups(), watched.node_reads()))
    }

    /// Run one tag lookup and pair its result with its key
    ///
    /// # Arguments
    ///
    /// * `lookup_key` - The lookup's tag key, value, and sorted groups
    /// * `groups` - The groups to search, as the event carries them
    fn keyed_lookup(
        &self,
        lookup_key: LookupKey,
        groups: Vec<String>,
    ) -> BoxFuture<'_, LookupResult> {
        // box the lookup so a batch's lookups can run together in any worker
        Box::pin(async move {
            let (result, counts) = self.lookup(lookup_key.0, lookup_key.1, &groups).await;
            (lookup_key, result, counts)
        })
    }

    /// Map a batch of events to the workflows they affect, coalescing the events for each one
    ///
    /// Events that need the same tag lookup share one.
    ///
    /// # Arguments
    ///
    /// * `events` - The batch, oldest first
    #[instrument(name = "WorkflowInstancer::affected", skip_all, fields(events = events.len()))]
    pub async fn affected(&self, events: &[Event]) -> Mapping {
        // find each distinct lookup the batch needs, searching the groups the first event that
        // needs it carries
        let mut wanted: HashMap<LookupKey, Vec<String>> = HashMap::new();
        for event in events {
            if let EventData::WorkflowGraph(graph) = &event.data
                && let Target::Tagged { key, value } = target(&graph.change)
            {
                let mut sorted = graph.groups.clone();
                sorted.sort_unstable();
                wanted
                    .entry((key, value, sorted))
                    .or_insert_with(|| graph.groups.clone());
            }
        }
        // run the lookups a few at a time, counting what each one read
        let lookups = wanted
            .into_iter()
            .map(|(lookup_key, groups)| self.keyed_lookup(lookup_key, groups))
            .collect::<Vec<BoxFuture<'_, LookupResult>>>();
        let results = stream::iter(lookups)
            .buffer_unordered(LOOKUP_CONCURRENCY)
            .collect::<Vec<LookupResult>>()
            .await;
        let mut mapping = Mapping::default();
        let mut found = HashMap::with_capacity(results.len());
        for (lookup_key, result, (tag_lookups, graph_reads)) in results {
            mapping.tag_lookups += tag_lookups;
            mapping.graph_reads += graph_reads;
            found.insert(lookup_key, result);
        }
        // map each event to its workflows
        for (position, event) in events.iter().enumerate() {
            // only workflow graph events affect workflows
            let EventData::WorkflowGraph(graph) = &event.data else {
                continue;
            };
            // find the workflows the event reaches, and how it reached each
            let reached = match target(&graph.change) {
                Target::Workflow(workflow) => vec![(workflow, Reach::Direct)],
                Target::Tagged { key, value } => {
                    let mut sorted = graph.groups.clone();
                    sorted.sort_unstable();
                    match found.get(&(key, value, sorted)) {
                        Some(Ok(matches)) => matches
                            .iter()
                            .map(|(workflow, instance)| {
                                let value = value.to_string();
                                (*workflow, Reach::tagged(key.key(), &value, *instance))
                            })
                            .collect(),
                        // a failed lookup keeps its event for a retry
                        Some(Err(error)) => {
                            event!(
                                Level::ERROR,
                                msg = "Failed to find the workflows an event affects",
                                event = event.id.to_string(),
                                error = error.to_string()
                            );
                            mapping.failed.insert(event.id);
                            continue;
                        }
                        None => Vec::new(),
                    }
                }
                Target::Nothing => Vec::new(),
            };
            // an event that reaches no workflow is finished
            if reached.is_empty() {
                mapping.unmapped.push(event.id);
                continue;
            }
            // coalesce the event into each workflow it reaches
            for (workflow, reach) in reached {
                mapping
                    .workflows
                    .entry(workflow)
                    .or_insert_with(|| Affected::new(workflow))
                    .add(
                        event,
                        trigger(&graph.change),
                        matches!(graph.change, WorkflowGraphChange::ManualInstance { .. }),
                        position,
                        reach,
                    );
            }
        }
        mapping
    }

    /// Build one affected workflow and pair the result with it
    ///
    /// # Arguments
    ///
    /// * `affected` - The workflow and the events coalesced into its build
    fn paired_build<'b>(
        &'b self,
        affected: &'b AffectedWorkflow,
    ) -> BoxFuture<'b, BuildResult<'b>> {
        // box the build so a batch's builds can run together in any worker
        Box::pin(async move { (affected, self.build(affected).await) })
    }

    /// Decide what a workflow's build records, or that nothing builds it
    ///
    /// A looked up event that missed the workflow's newest instance, created after the lookup,
    /// can still qualify through that instance's tags, so they're read only then.
    ///
    /// # Arguments
    ///
    /// * `affected` - The workflow and the events coalesced into its build
    /// * `catalog` - The workflow catalog, as it was read
    /// * `stats` - The stats to count the read in
    async fn decide(
        &self,
        affected: &AffectedWorkflow,
        catalog: &WorkflowCatalogEntity,
        stats: &mut AiWorkerStats,
    ) -> Result<Option<BuildCause<WorkflowInstanceTrigger>>, Error> {
        // read the newest instance's tags only when a looked up event missed it
        let newest = catalog.newest_instance;
        let newest_tags = match newest {
            Some(id) if affected.needs_newest_tags(catalog.auto_instance, newest) => {
                stats.graph_reads += 1;
                self.api
                    .reader()
                    .node(id, &[], Directionality::To)
                    .await?
                    .map(|node| node.entity.tags)
            }
            _ => None,
        };
        // the latest qualifying event decides the build
        Ok(affected.decide(catalog.auto_instance, newest, newest_tags.as_ref()))
    }

    /// Build one affected workflow's next instance, or find that its newest one is current
    ///
    /// Only a read or write that failed is an error, which leaves the workflow's events in
    /// flight. A deleted workflow and one no event qualifies for (automatic instances off and no
    /// manual request, or lookups that only matched older revisions) build nothing and finish
    /// their events.
    ///
    /// # Arguments
    ///
    /// * `affected` - The workflow and the events coalesced into its build
    #[instrument(
        name = "WorkflowInstancer::build",
        skip_all,
        fields(workflow = affected.root.to_string()),
        err(Debug)
    )]
    async fn build(&self, affected: &AffectedWorkflow) -> Result<AiWorkerStats, Error> {
        // read the workflow catalog, which is gone when it was deleted after its events queued
        let mut stats = AiWorkerStats {
            graph_reads: 1,
            ..AiWorkerStats::default()
        };
        let reader = self.api.reader();
        let Some(node) = reader.node(affected.root, &[], Directionality::To).await? else {
            event!(
                Level::INFO,
                msg = "Skipping events for a workflow that was deleted",
                workflow = affected.root.to_string()
            );
            return Ok(stats);
        };
        let EntityMetadata::WorkflowCatalog(catalog) = &node.entity.metadata else {
            event!(
                Level::WARN,
                msg = "Skipping events for an entity that isn't a workflow catalog",
                entity = affected.root.to_string()
            );
            return Ok(stats);
        };
        // the latest qualifying event decides the build; with none the workflow isn't affected
        let Some(cause) = self.decide(affected, catalog, &mut stats).await? else {
            return Ok(stats);
        };
        // build, rebuilding from a fresh read when another build took the revision first
        let mut attempt = 0;
        loop {
            attempt += 1;
            let watched = WatchedReader::new(reader);
            let build = build_workflow_instance(
                &watched,
                affected.root,
                &self.limits,
                cause.trigger.clone(),
                &cause.triggered_by,
            )
            .await;
            stats.graph_reads += watched.node_reads();
            stats.tag_lookups += watched.tag_lookups();
            let build = build?;
            // an edge whose target is gone raced a delete, which removes the edge too, so the
            // build is retried later instead of recording the target as missing
            let missing = watched.missing_targets();
            if !missing.is_empty() {
                return Err(Error::new(format!(
                    "workflow `{}` reached entities deleted while it was read: {missing:?}",
                    node.entity.name
                )));
            }
            match build {
                // the newest instance is current, so only the manual requesters are told
                WorkflowBuild::Current { newest, resolved } => {
                    stats.skipped += 1;
                    let current = NotificationRequest::new(
                        format!(
                            "workflow `{}` is unchanged; its newest instance `{newest}` is current",
                            resolved.workflow.name
                        ),
                        NotificationLevel::Info,
                    );
                    notify_all(self.api, &affected.requesters(), &current, &mut stats).await;
                    return Ok(stats);
                }
                // create the new instance and tell everyone who caused it
                WorkflowBuild::New(resolved) => {
                    match self.api.create_workflow_instance(&resolved.instance).await {
                        Ok(id) => {
                            stats.instances_created += 1;
                            if !resolved.instance.is_ready() {
                                stats.invalid += 1;
                            }
                            let announcement =
                                built_note(&resolved.workflow.name, &resolved.instance, id);
                            notify_all(self.api, &cause.recipients, &announcement, &mut stats)
                                .await;
                            return Ok(stats);
                        }
                        // another build took the revision or holds the workflow's lock
                        Err(error)
                            if error.status() == Some(reqwest::StatusCode::CONFLICT)
                                && attempt < MAX_BUILD_ATTEMPTS =>
                        {
                            event!(
                                Level::INFO,
                                msg = "Rebuilding a workflow after losing a revision race",
                                workflow = affected.root.to_string(),
                                error = error.to_string()
                            );
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    /// Build every workflow a batch of events affects and sort the events into the finished
    /// and the ones left in flight
    ///
    /// An event is finished when every workflow it reaches was handled; a failed lookup or a
    /// failed build leaves its events in flight.
    ///
    /// # Arguments
    ///
    /// * `events` - The batch, oldest first
    #[instrument(name = "WorkflowInstancer::process", skip_all, fields(events = events.len()))]
    pub async fn process(&self, events: &[Event]) -> PhaseOutcome {
        // map the events to their workflows
        let mapping = self.affected(events).await;
        let mut outcome = PhaseOutcome::default();
        outcome.stats.tag_lookups = mapping.tag_lookups;
        outcome.stats.graph_reads = mapping.graph_reads;
        // build the workflows a few at a time
        let builds = mapping
            .workflows
            .values()
            .map(|affected| self.paired_build(affected))
            .collect::<Vec<BoxFuture<'_, BuildResult<'_>>>>();
        let results = stream::iter(builds)
            .buffer_unordered(BUILD_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        // keep the events of every failed build, and of every failed lookup, in flight
        let mut held = mapping.failed;
        for (affected, result) in results {
            match result {
                Ok(stats) => outcome.stats += stats,
                Err(error) => {
                    event!(
                        Level::ERROR,
                        msg = "Failed to build a workflow; its events will be retried",
                        workflow = affected.root.to_string(),
                        error = error.to_string()
                    );
                    held.extend(affected.events.iter().copied());
                }
            }
        }
        // finish every other workflow graph event
        outcome.sort(events, &held);
        outcome
    }
}
