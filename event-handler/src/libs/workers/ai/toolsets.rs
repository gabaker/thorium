//! Builds toolset instances for workflow graph events
//!
//! Each event maps to the toolsets it rebuilds: the toolset it names, or the toolsets whose
//! newest instance carries the tag a lookup in the event's groups finds. A published tool reaches
//! the toolsets that follow it at latest and the toolsets whose selector names one of its tags;
//! a tool's tag change reaches the toolsets whose selector names a changed pair. The events for
//! one toolset are coalesced into one build, recorded as the latest qualifying event's, and a
//! build creates a new instance only when the toolset's newest instance isn't already current. The users who
//! caused a build are told how it ended.

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thorium::Error;
use thorium::models::entities::toolsets::{
    TOOL_SET_FOLLOWS_TOOL_TAG, TOOL_SET_SELECTOR_TAG, ToolSetBuild, build_tool_set_instance,
    newest_tool_set_instance,
};
use thorium::models::entities::workflows::{GraphReader, WatchedReader};
use thorium::models::{
    AssociationKind, Directionality, Entity, EntityKinds, EntityMetadata, Event, EventData,
    NotificationLevel, NotificationRequest, ToolSetInstanceEntity, ToolSetInstanceStatus,
    ToolSetInstanceTrigger, ToolSetLimits, User, WorkflowGraphChange,
};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::{Affected, AiWorkerStats, InstanceApi, PhaseOutcome, Reach, notify_all, tagged_roots};

#[cfg(test)]
mod tests;

/// How many toolsets of a batch are built at once
const BUILD_CONCURRENCY: usize = 8;

/// How many tag lookups and tool reads of a batch run at once
const LOOKUP_CONCURRENCY: usize = 8;

/// How many times a build that loses a revision race is rebuilt before its events are retried
/// later
const MAX_BUILD_ATTEMPTS: usize = 3;

/// One toolset a batch of events affects, and what its build records
pub type AffectedToolSet = Affected<ToolSetInstanceTrigger>;

/// The toolsets a batch of events affects
#[derive(Debug, Default)]
pub struct ToolSetMapping {
    /// Each affected toolset by id
    pub tool_sets: BTreeMap<Uuid, AffectedToolSet>,
    /// The events whose tool read or lookup failed, which stay in flight
    pub failed: BTreeSet<Uuid>,
    /// The events that affect no toolset, which are finished
    pub unmapped: Vec<Uuid>,
    /// How many tag lookups were made
    pub tag_lookups: usize,
    /// How many entities were read by id
    pub graph_reads: usize,
}

/// Where a change leads before any read
enum Target {
    /// The change names its toolset
    ToolSet(Uuid),
    /// A published tool, which reaches the toolsets following it and selecting one of its tags
    Tool(Uuid),
    /// Changed tag pairs, as `key=value`, which reach the toolsets selecting them
    Pairs(Vec<String>),
    /// The change rebuilds no toolset
    Nothing,
}

/// Find where a change leads
///
/// # Arguments
///
/// * `change` - The change
fn target(change: &WorkflowGraphChange) -> Target {
    // toolset changes name their toolset, a tool's new latest and tag changes are looked up, and
    // every other change rebuilds workflows only
    match change {
        WorkflowGraphChange::ToolSetChanged { tool_set }
        | WorkflowGraphChange::ManualToolSetInstance { tool_set }
        | WorkflowGraphChange::EdgeChanged {
            source: tool_set,
            kind: AssociationKind::Includes,
            ..
        } => Target::ToolSet(*tool_set),
        WorkflowGraphChange::ToolUpdated { tool, .. } => Target::Tool(*tool),
        WorkflowGraphChange::ToolTagsChanged { tags, .. } => Target::Pairs(
            tags.iter()
                .flat_map(|(key, values)| values.iter().map(move |value| format!("{key}={value}")))
                .collect(),
        ),
        WorkflowGraphChange::WorkflowUpdated { .. }
        | WorkflowGraphChange::ManualInstance { .. }
        | WorkflowGraphChange::AgentUpdated { .. }
        | WorkflowGraphChange::EdgeChanged { .. }
        | WorkflowGraphChange::ToolSetUpdated { .. } => Target::Nothing,
    }
}

/// Get the trigger a toolset instance built for a change records
///
/// # Arguments
///
/// * `change` - The change
fn trigger(change: &WorkflowGraphChange) -> ToolSetInstanceTrigger {
    // a request is manual, a tool's new latest names the tool, and the rest changed what the
    // toolset's settings, edges, or selector give it
    match change {
        WorkflowGraphChange::ManualToolSetInstance { .. } => ToolSetInstanceTrigger::Manual,
        WorkflowGraphChange::ToolUpdated {
            tool,
            tool_instance,
        } => ToolSetInstanceTrigger::ToolUpdated {
            tool: *tool,
            tool_instance: *tool_instance,
        },
        _ => ToolSetInstanceTrigger::ToolSetUpdated,
    }
}

/// Build the notification for a created toolset instance: Info when it's Ready, or an Error
/// listing why it's Invalid
///
/// # Arguments
///
/// * `name` - The toolset's name
/// * `instance` - The created instance
/// * `id` - The created instance's id
fn built_note(name: &str, instance: &ToolSetInstanceEntity, id: Uuid) -> NotificationRequest<User> {
    // name the toolset, the revision, and the instance, and list the errors of an invalid one
    let revision = instance.revision;
    match &instance.status {
        ToolSetInstanceStatus::Ready => NotificationRequest::new(
            format!("toolset `{name}` #r{revision} (`{id}`) is Ready"),
            NotificationLevel::Info,
        ),
        ToolSetInstanceStatus::Invalid { errors } => NotificationRequest::new(
            format!(
                "toolset `{name}` #r{revision} (`{id}`) is Invalid: {}",
                errors.join("; ")
            ),
            NotificationLevel::Error,
        ),
    }
}

/// Get the tag pairs a tool carries in any of some groups, as `key=value`
///
/// # Arguments
///
/// * `tool` - The tool, with its tags
/// * `groups` - The groups to keep the tags of
fn tag_pairs(tool: &Entity, groups: &[String]) -> Vec<String> {
    // keep each pair the tool carries in one of the groups
    let mut pairs = tool
        .tags
        .iter()
        .flat_map(|(key, values)| {
            values
                .iter()
                .filter(|(_, tagged)| groups.iter().any(|group| tagged.contains(group)))
                .map(move |(value, _)| format!("{key}={value}"))
        })
        .collect::<Vec<String>>();
    pairs.sort_unstable();
    pairs
}

/// The key of one tag lookup: the tag, its value, and the sorted groups searched
type LookupKey = (&'static str, String, Vec<String>);

/// A tag lookup, the toolsets and instances it found, and how many lookups and reads by id it
/// made
type LookupResult = (LookupKey, Result<Vec<(Uuid, Uuid)>, Error>, (usize, usize));

/// The key of one tool read: the tool and the sorted groups of the event that needs it
type ToolKey = (Uuid, Vec<String>);

/// A tool read and the tag pairs the tool carries in the event's groups
type ToolResult = (ToolKey, Result<Vec<String>, Error>);

/// An affected toolset and the stats of its build
type BuildResult<'b> = (&'b AffectedToolSet, Result<AiWorkerStats, Error>);

/// Sort a list of groups
///
/// # Arguments
///
/// * `groups` - The groups to sort
fn sorted(groups: &[String]) -> Vec<String> {
    // copy and sort the groups
    let mut groups = groups.to_vec();
    groups.sort_unstable();
    groups
}

/// Builds the toolset instances a batch of workflow graph events calls for
pub struct ToolSetInstancer<'a, A: InstanceApi + ?Sized> {
    /// The API reads and writes go through
    api: &'a A,
    /// The limits toolsets are resolved under
    limits: ToolSetLimits,
}

impl<'a, A: InstanceApi + ?Sized> ToolSetInstancer<'a, A> {
    /// Build an instancer over an API
    ///
    /// # Arguments
    ///
    /// * `api` - The API reads and writes go through
    /// * `limits` - The limits toolsets are resolved under
    pub fn new(api: &'a A, limits: ToolSetLimits) -> Self {
        // nothing is read until a batch is processed
        ToolSetInstancer { api, limits }
    }

    /// Read a published tool and pair the tag pairs it carries in an event's groups with the read
    ///
    /// A tool that no longer exists carries no tags.
    ///
    /// # Arguments
    ///
    /// * `tool_key` - The tool and the event's sorted groups
    fn keyed_tool(&self, tool_key: ToolKey) -> BoxFuture<'_, ToolResult> {
        // box the read so a batch's reads can run together in any worker
        Box::pin(async move {
            let result = self
                .api
                .reader()
                .node(tool_key.0, &[], Directionality::To)
                .await
                .map(|node| {
                    node.map_or_else(Vec::new, |node| tag_pairs(&node.entity, &tool_key.1))
                });
            (tool_key, result)
        })
    }

    /// Run one tag lookup on toolset instances, pairing each matched instance with its toolset,
    /// and pair the result and the lookups and reads by id it made with its key
    ///
    /// # Arguments
    ///
    /// * `lookup_key` - The lookup's tag key, value, and sorted groups
    fn keyed_lookup(&self, lookup_key: LookupKey) -> BoxFuture<'_, LookupResult> {
        // box the lookup so a batch's lookups can run together in any worker
        Box::pin(async move {
            let watched = WatchedReader::new(self.api.reader());
            let result = tagged_roots(
                &watched,
                EntityKinds::ToolSetInstance,
                lookup_key.0,
                &lookup_key.1,
                &lookup_key.2,
            )
            .await;
            let counts = (watched.tag_lookups(), watched.node_reads());
            (lookup_key, result, counts)
        })
    }

    /// Read some published tools a few at a time for the tag pairs each carries
    ///
    /// # Arguments
    ///
    /// * `wanted` - Each tool and the sorted groups of the events that need it
    async fn read_tools(
        &self,
        wanted: BTreeSet<ToolKey>,
    ) -> HashMap<ToolKey, Result<Vec<String>, Error>> {
        // read the tools together, a few at a time
        let reads = wanted
            .into_iter()
            .map(|tool_key| self.keyed_tool(tool_key))
            .collect::<Vec<BoxFuture<'_, ToolResult>>>();
        stream::iter(reads)
            .buffer_unordered(LOOKUP_CONCURRENCY)
            .collect()
            .await
    }

    /// Run some tag lookups on toolset instances a few at a time
    ///
    /// # Arguments
    ///
    /// * `wanted` - Each distinct lookup a batch needs
    async fn run_lookups(&self, wanted: BTreeSet<LookupKey>) -> Vec<LookupResult> {
        // run the lookups together, a few at a time
        let lookups = wanted
            .into_iter()
            .map(|lookup_key| self.keyed_lookup(lookup_key))
            .collect::<Vec<BoxFuture<'_, LookupResult>>>();
        stream::iter(lookups)
            .buffer_unordered(LOOKUP_CONCURRENCY)
            .collect()
            .await
    }

    /// Get the lookups an event needs, or none when a tool read it needs failed
    ///
    /// # Arguments
    ///
    /// * `target` - Where the event's change leads
    /// * `groups` - The event's sorted groups
    /// * `tools` - The tag pairs each published tool carries, by tool and groups
    fn lookups(
        target: &Target,
        groups: &[String],
        tools: &HashMap<ToolKey, Result<Vec<String>, Error>>,
    ) -> Option<Vec<LookupKey>> {
        // a published tool is followed by tag and selected by its tags; changed pairs are selected
        match target {
            Target::Tool(tool) => {
                let pairs = match tools.get(&(*tool, groups.to_vec())) {
                    Some(Ok(pairs)) => pairs,
                    Some(Err(_)) => return None,
                    None => &Vec::new(),
                };
                let mut keys = vec![(TOOL_SET_FOLLOWS_TOOL_TAG, tool.to_string(), groups.to_vec())];
                keys.extend(
                    pairs
                        .iter()
                        .map(|pair| (TOOL_SET_SELECTOR_TAG, pair.clone(), groups.to_vec())),
                );
                Some(keys)
            }
            Target::Pairs(pairs) => Some(
                pairs
                    .iter()
                    .map(|pair| (TOOL_SET_SELECTOR_TAG, pair.clone(), groups.to_vec()))
                    .collect(),
            ),
            Target::ToolSet(_) | Target::Nothing => Some(Vec::new()),
        }
    }

    /// Map a batch of events to the toolsets they affect, coalescing the events for each one
    ///
    /// Events that need the same tool read or tag lookup share one.
    ///
    /// # Arguments
    ///
    /// * `events` - The batch, oldest first
    #[instrument(name = "ToolSetInstancer::affected", skip_all, fields(events = events.len()))]
    pub async fn affected(&self, events: &[Event]) -> ToolSetMapping {
        // find where each event leads
        let targets = events
            .iter()
            .map(|event| match &event.data {
                EventData::WorkflowGraph(graph) => (target(&graph.change), sorted(&graph.groups)),
                _ => (Target::Nothing, Vec::new()),
            })
            .collect::<Vec<(Target, Vec<String>)>>();
        // read each published tool once for its tags in the groups of the events that need it
        let wanted_tools = targets
            .iter()
            .filter_map(|(target, groups)| match target {
                Target::Tool(tool) => Some((*tool, groups.clone())),
                _ => None,
            })
            .collect::<BTreeSet<ToolKey>>();
        let graph_reads = wanted_tools.len();
        let tools = self.read_tools(wanted_tools).await;
        // run each distinct lookup the batch needs a few at a time
        let needed = targets
            .iter()
            .map(|(target, groups)| Self::lookups(target, groups, &tools))
            .collect::<Vec<Option<Vec<LookupKey>>>>();
        let wanted = needed
            .iter()
            .flatten()
            .flatten()
            .cloned()
            .collect::<BTreeSet<LookupKey>>();
        let mut mapping = ToolSetMapping {
            graph_reads,
            ..ToolSetMapping::default()
        };
        let results = self.run_lookups(wanted).await;
        let mut found = HashMap::with_capacity(results.len());
        for (lookup_key, result, (tag_lookups, reads)) in results {
            mapping.tag_lookups += tag_lookups;
            mapping.graph_reads += reads;
            found.insert(lookup_key, result);
        }
        // map each event to its toolsets
        for (position, ((event, (target, _)), keys)) in
            events.iter().zip(&targets).zip(needed).enumerate()
        {
            // only workflow graph events affect toolsets
            let EventData::WorkflowGraph(graph) = &event.data else {
                continue;
            };
            // a failed tool read keeps its event for a retry
            let Some(keys) = keys else {
                event!(
                    Level::ERROR,
                    msg = "Failed to read the tool an event names",
                    event = event.id.to_string()
                );
                mapping.failed.insert(event.id);
                continue;
            };
            // find the toolsets the event reaches, and how it reached each
            let mut reached = match target {
                Target::ToolSet(tool_set) => vec![(*tool_set, Reach::Direct)],
                _ => Vec::new(),
            };
            let mut failed = false;
            for key in &keys {
                match found.get(key) {
                    Some(Ok(matches)) => {
                        reached.extend(matches.iter().map(|(tool_set, instance)| {
                            (*tool_set, Reach::tagged(key.0, &key.1, *instance))
                        }));
                    }
                    Some(Err(error)) => {
                        event!(
                            Level::ERROR,
                            msg = "Failed to find the toolsets an event affects",
                            event = event.id.to_string(),
                            error = error.to_string()
                        );
                        failed = true;
                    }
                    None => (),
                }
            }
            // a failed lookup keeps its event for a retry
            if failed {
                mapping.failed.insert(event.id);
                continue;
            }
            // an event that reaches no toolset is finished
            if reached.is_empty() {
                mapping.unmapped.push(event.id);
                continue;
            }
            // coalesce the event into each toolset it reaches
            let manual = matches!(
                graph.change,
                WorkflowGraphChange::ManualToolSetInstance { .. }
            );
            for (tool_set, reach) in reached {
                mapping
                    .tool_sets
                    .entry(tool_set)
                    .or_insert_with(|| Affected::new(tool_set))
                    .add(event, trigger(&graph.change), manual, position, reach);
            }
        }
        mapping
    }

    /// Build one affected toolset and pair the result with it
    ///
    /// # Arguments
    ///
    /// * `affected` - The toolset and the events coalesced into its build
    fn paired_build<'b>(&'b self, affected: &'b AffectedToolSet) -> BoxFuture<'b, BuildResult<'b>> {
        // box the build so a batch's builds can run together in any worker
        Box::pin(async move { (affected, self.build(affected).await) })
    }

    /// Build one affected toolset's next instance, or find that its newest one is current
    ///
    /// Only a read or write that failed is an error, which leaves the toolset's events in flight.
    /// A deleted toolset and one no event qualifies for (automatic instances off and no manual
    /// request, or lookups that only matched older instances) build nothing and finish their
    /// events.
    ///
    /// # Arguments
    ///
    /// * `affected` - The toolset and the events coalesced into its build
    #[instrument(
        name = "ToolSetInstancer::build",
        skip_all,
        fields(tool_set = affected.root.to_string()),
        err(Debug)
    )]
    async fn build(&self, affected: &AffectedToolSet) -> Result<AiWorkerStats, Error> {
        // read the toolset, which is gone when it was deleted after its events queued
        let mut stats = AiWorkerStats {
            graph_reads: 1,
            ..AiWorkerStats::default()
        };
        let reader = self.api.reader();
        let Some(node) = reader.node(affected.root, &[], Directionality::To).await? else {
            event!(
                Level::INFO,
                msg = "Skipping events for a toolset that was deleted",
                tool_set = affected.root.to_string()
            );
            return Ok(stats);
        };
        let EntityMetadata::ToolSet(meta) = &node.entity.metadata else {
            event!(
                Level::WARN,
                msg = "Skipping events for an entity that isn't a toolset",
                entity = affected.root.to_string()
            );
            return Ok(stats);
        };
        // a looked up event qualifies through the newest instance it matched or, when that
        // instance was created since the lookup, through the tags it carries
        let newest = if meta.auto_instance && !affected.matched().is_empty() {
            stats.tag_lookups += 1;
            newest_tool_set_instance(reader, &node.entity).await?
        } else {
            None
        };
        let newest_id = newest.as_ref().map(|entity| entity.id);
        let newest_tags = newest.as_ref().map(|entity| &entity.tags);
        // the latest qualifying event decides the build; with none the toolset isn't affected
        let Some(cause) = affected.decide(meta.auto_instance, newest_id, newest_tags) else {
            return Ok(stats);
        };
        // build, rebuilding from a fresh read when another build took the revision first
        let mut attempt = 0;
        loop {
            attempt += 1;
            let watched = WatchedReader::new(reader);
            let build = build_tool_set_instance(
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
                    "toolset `{}` reached entities deleted while it was read: {missing:?}",
                    node.entity.name
                )));
            }
            match build {
                // the newest instance is current, so only the manual requesters are told
                ToolSetBuild::Current { newest, resolved } => {
                    stats.tool_set_skipped += 1;
                    let current = NotificationRequest::new(
                        format!(
                            "toolset `{}` is unchanged; its newest instance `{newest}` is current",
                            resolved.tool_set.name
                        ),
                        NotificationLevel::Info,
                    );
                    notify_all(self.api, &affected.requesters(), &current, &mut stats).await;
                    return Ok(stats);
                }
                // create the new instance and tell everyone who caused it
                ToolSetBuild::New(resolved) => {
                    match self.api.create_tool_set_instance(&resolved.instance).await {
                        Ok(id) => {
                            stats.tool_set_instances_created += 1;
                            if !resolved.instance.is_ready() {
                                stats.tool_set_invalid += 1;
                            }
                            let announcement =
                                built_note(&resolved.tool_set.name, &resolved.instance, id);
                            notify_all(self.api, &cause.recipients, &announcement, &mut stats)
                                .await;
                            return Ok(stats);
                        }
                        // another build took the revision or holds the toolset's lock
                        Err(error)
                            if error.status() == Some(reqwest::StatusCode::CONFLICT)
                                && attempt < MAX_BUILD_ATTEMPTS =>
                        {
                            event!(
                                Level::INFO,
                                msg = "Rebuilding a toolset after losing a revision race",
                                tool_set = affected.root.to_string(),
                                error = error.to_string()
                            );
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    /// Build every toolset a batch of events affects and sort the events into the finished and
    /// the ones left in flight
    ///
    /// An event is finished when every toolset it reaches was handled; a failed read, a failed
    /// lookup, or a failed build leaves its events in flight.
    ///
    /// # Arguments
    ///
    /// * `events` - The batch, oldest first
    #[instrument(name = "ToolSetInstancer::process", skip_all, fields(events = events.len()))]
    pub async fn process(&self, events: &[Event]) -> PhaseOutcome {
        // map the events to their toolsets
        let mapping = self.affected(events).await;
        let mut outcome = PhaseOutcome::default();
        outcome.stats.tag_lookups = mapping.tag_lookups;
        outcome.stats.graph_reads = mapping.graph_reads;
        // build the toolsets a few at a time
        let builds = mapping
            .tool_sets
            .values()
            .map(|affected| self.paired_build(affected))
            .collect::<Vec<BoxFuture<'_, BuildResult<'_>>>>();
        let results = stream::iter(builds)
            .buffer_unordered(BUILD_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        // keep the events of every failed build, and of every failed read or lookup, in flight
        let mut held = mapping.failed;
        for (affected, result) in results {
            match result {
                Ok(stats) => outcome.stats += stats,
                Err(error) => {
                    event!(
                        Level::ERROR,
                        msg = "Failed to build a toolset; its events will be retried",
                        tool_set = affected.root.to_string(),
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
