//! The workflows worker, which handles the agentic event types in one worker
//!
//! It builds toolset and workflow instances for workflow graph events with the event handler's
//! admin client, recording the user whose change caused each build, and tells that user how the
//! build ended. The toolsets of a batch are built before its workflows. It also refreshes model
//! catalogs for model refresh events, reading each endpoint as the user who asked and sending
//! the listing to the API, which merges it.

use chrono::prelude::*;
use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::sync::Arc;
use thorium::models::entities::workflows::GraphReader;
use thorium::models::{
    CrawlLimits, EntityMetadataRequest, EntityRequest, Event, EventData, EventIds, EventType,
    NotificationParams, NotificationRequest, ToolSetInstanceEntity, ToolSetInstanceEntityRequest,
    ToolSetLimits, User, WorkflowInstanceEntity, WorkflowInstanceEntityRequest,
};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

#[cfg(test)]
mod fake;
pub mod models;
pub mod providers;
pub mod toolsets;
pub mod workflow_instances;

pub use models::{ModelRefresher, RefreshLimits, UserClients};
pub use providers::{AuthHosts, ListingLimits};
pub use toolsets::ToolSetInstancer;
pub use workflow_instances::WorkflowInstancer;

use crate::libs::EventWorkerCache;
use crate::libs::workers::{
    EventWorkerMutableSupport, EventWorkerSupportCore, WorkerCacheUpdateKinds, WorkerCacheUpdates,
};

/// How many minutes events left in flight wait before they're retried
const RETRY_MINUTES: i64 = 3;

/// The stats for the workflows worker
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct WorkflowsWorkerStats {
    /// The events that were finished and cleared
    pub events_processed: usize,
    /// The workflow instances created, ready or invalid
    pub instances_created: usize,
    /// The builds that created nothing because the newest instance was already current
    pub skipped: usize,
    /// The workflow instances created invalid
    pub invalid: usize,
    /// The user notifications written
    pub notifications_sent: usize,
    /// The user notifications that couldn't be written
    pub notification_failures: usize,
    /// The tag lookups made to find affected workflows and their instances
    pub tag_lookups: usize,
    /// The entities read by id while building workflows
    pub graph_reads: usize,
    /// The toolset instances created, ready or invalid
    pub tool_set_instances_created: usize,
    /// The toolset builds that created nothing because the newest instance was already current
    pub tool_set_skipped: usize,
    /// The toolset instances created invalid
    pub tool_set_invalid: usize,
    /// The catalog models refreshes added
    pub models_added: usize,
    /// The bans refreshes added (removed models and listing failures)
    pub bans_added: usize,
    /// The bans refreshes cleared
    pub bans_cleared: usize,
    /// The listings that failed with a credential or endpoint failure
    pub listing_failures: usize,
    /// The users read to build the clients refreshes act as
    pub user_lookups: usize,
}

impl std::ops::AddAssign for WorkflowsWorkerStats {
    /// Add another set of stats to these
    ///
    /// # Arguments
    ///
    /// * `rhs` - The stats to add
    fn add_assign(&mut self, rhs: Self) {
        // add each counter without overflowing
        self.events_processed = self.events_processed.saturating_add(rhs.events_processed);
        self.instances_created = self.instances_created.saturating_add(rhs.instances_created);
        self.skipped = self.skipped.saturating_add(rhs.skipped);
        self.invalid = self.invalid.saturating_add(rhs.invalid);
        self.notifications_sent = self
            .notifications_sent
            .saturating_add(rhs.notifications_sent);
        self.notification_failures = self
            .notification_failures
            .saturating_add(rhs.notification_failures);
        self.tag_lookups = self.tag_lookups.saturating_add(rhs.tag_lookups);
        self.graph_reads = self.graph_reads.saturating_add(rhs.graph_reads);
        self.tool_set_instances_created = self
            .tool_set_instances_created
            .saturating_add(rhs.tool_set_instances_created);
        self.tool_set_skipped = self.tool_set_skipped.saturating_add(rhs.tool_set_skipped);
        self.tool_set_invalid = self.tool_set_invalid.saturating_add(rhs.tool_set_invalid);
        self.models_added = self.models_added.saturating_add(rhs.models_added);
        self.bans_added = self.bans_added.saturating_add(rhs.bans_added);
        self.bans_cleared = self.bans_cleared.saturating_add(rhs.bans_cleared);
        self.listing_failures = self.listing_failures.saturating_add(rhs.listing_failures);
        self.user_lookups = self.user_lookups.saturating_add(rhs.user_lookups);
    }
}

/// What the workflows worker reads and writes through the API while it builds instances
///
/// The event handler's admin client implements it; tests implement it over an in-memory graph.
pub trait InstanceApi: Sync {
    /// The reader the graph is crawled through
    type Reader: GraphReader + ?Sized;

    /// Get the reader the graph is crawled through
    fn reader(&self) -> &Self::Reader;

    /// Create a workflow instance and get its id
    ///
    /// # Arguments
    ///
    /// * `instance` - The instance to create
    fn create_workflow_instance(
        &self,
        instance: &WorkflowInstanceEntity,
    ) -> impl Future<Output = Result<Uuid, Error>> + Send;

    /// Create a toolset instance and get its id
    ///
    /// # Arguments
    ///
    /// * `instance` - The instance to create
    fn create_tool_set_instance(
        &self,
        instance: &ToolSetInstanceEntity,
    ) -> impl Future<Output = Result<Uuid, Error>> + Send;

    /// Write a notification for a user
    ///
    /// # Arguments
    ///
    /// * `username` - The user to notify
    /// * `req` - The notification to write
    fn notify(
        &self,
        username: &str,
        req: &NotificationRequest<User>,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

impl InstanceApi for Thorium {
    /// The Thorium client reads the graph itself
    type Reader = Thorium;

    /// Get the client, which reads the graph itself
    fn reader(&self) -> &Thorium {
        self
    }

    /// Submit a workflow instance in its workflow's groups and get its id
    ///
    /// # Arguments
    ///
    /// * `instance` - The instance to create
    async fn create_workflow_instance(
        &self,
        instance: &WorkflowInstanceEntity,
    ) -> Result<Uuid, Error> {
        // the API names the instance, so any name is sent
        let metadata = WorkflowInstanceEntityRequest::new(instance)?;
        let req = EntityRequest::new(
            "workflow-instance",
            EntityMetadataRequest::WorkflowInstance(metadata),
            instance.workflow_groups.clone(),
        );
        let resp = self.entities.create(req).await?;
        Ok(resp.id)
    }

    /// Submit a toolset instance in its toolset's groups and get its id
    ///
    /// # Arguments
    ///
    /// * `instance` - The instance to create
    async fn create_tool_set_instance(
        &self,
        instance: &ToolSetInstanceEntity,
    ) -> Result<Uuid, Error> {
        // the API names the instance, so any name is sent
        let metadata = ToolSetInstanceEntityRequest::new(instance)?;
        let req = EntityRequest::new(
            "toolset-instance",
            EntityMetadataRequest::ToolSetInstance(metadata),
            instance.tool_set_groups.clone(),
        );
        let resp = self.entities.create(req).await?;
        Ok(resp.id)
    }

    /// Write a notification for a user with the default expiration
    ///
    /// # Arguments
    ///
    /// * `username` - The user to notify
    /// * `req` - The notification to write
    async fn notify(&self, username: &str, req: &NotificationRequest<User>) -> Result<(), Error> {
        // info notifications expire on their own and errors stay until they're dismissed
        self.users
            .create_notification(username, req, &NotificationParams::default())
            .await?;
        Ok(())
    }
}

/// How an event reached a root it affects
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The event names the root itself
    Direct,
    /// A tag lookup on the root's instances matched one of them
    Tagged {
        /// The tag key that was looked up
        key: String,
        /// The tag value that was looked up
        value: String,
        /// The instance of the root the lookup matched
        instance: Uuid,
    },
}

impl Reach {
    /// Build the reach of an event a tag lookup matched to one of a root's instances
    ///
    /// # Arguments
    ///
    /// * `key` - The tag key that was looked up
    /// * `value` - The tag value that was looked up
    /// * `instance` - The instance of the root the lookup matched
    #[must_use]
    pub fn tagged(key: &str, value: &str, instance: Uuid) -> Self {
        // own the looked up tag
        Reach::Tagged {
            key: key.to_owned(),
            value: value.to_owned(),
            instance,
        }
    }
}

/// One event coalesced into a root's build, with how it reached the root
#[derive(Debug, Clone)]
struct Cause<T> {
    /// The event
    event: Uuid,
    /// What the event's change records as the build's trigger
    trigger: T,
    /// The user whose change queued the event
    user: String,
    /// Whether the event is a user's request for an instance
    manual: bool,
    /// The tag key and value the event was looked up by, or none when it names the root
    lookup: Option<(String, String)>,
    /// The instances of the root the event's lookup matched
    matched: BTreeSet<Uuid>,
    /// When the event was queued and its place in the batch, which orders events that share a
    /// timestamp
    at: (DateTime<Utc>, usize),
}

impl<T> Cause<T> {
    /// Whether this event can decide the build of a root
    ///
    /// With automatic instances off only a manual request can. Otherwise an event that names the
    /// root can, and a looked up event can when it matched the root's newest instance, or when
    /// that newest instance (created after the lookup) carries the tag it was looked up by.
    ///
    /// # Arguments
    ///
    /// * `auto_instance` - Whether the root builds instances automatically
    /// * `newest` - The root's newest instance, if it has one
    /// * `newest_tags` - The newest instance's tags, when they were read
    fn qualifies(
        &self,
        auto_instance: bool,
        newest: Option<Uuid>,
        newest_tags: Option<&thorium::models::TagMap>,
    ) -> bool {
        // only manual requests build a root whose automatic instances are off
        if !auto_instance {
            return self.manual;
        }
        // an event naming the root, or any event for a root with no instance yet, qualifies
        let (Some((key, value)), Some(newest)) = (&self.lookup, newest) else {
            return true;
        };
        // a looked up event qualifies through the newest instance it matched or the tag it carries
        self.matched.contains(&newest)
            || newest_tags.is_some_and(|tags| {
                tags.get(key)
                    .is_some_and(|values| values.contains_key(value))
            })
    }
}

/// What a root's build records and who is told how it ended
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCause<T> {
    /// What caused the build: the latest qualifying event's change
    pub trigger: T,
    /// The user whose change caused the build: the latest qualifying event's user
    pub triggered_by: String,
    /// The users to tell how the build ended: its user, then each manual requester, each once
    pub recipients: Vec<String>,
}

/// One root a batch of events affects, and the events coalesced into its build
///
/// The events for one workflow or toolset are coalesced into one build. Which event decides the
/// build's trigger and user is only known once the root is read: the latest event that qualifies
/// for the root's settings and newest instance. Every user who asked for an instance is told how
/// the build ends. A manual request names exactly one root, so it's left in flight only when that
/// root's build failed, and its requester is told about the build only once.
#[derive(Debug, Clone)]
pub struct Affected<T> {
    /// The workflow catalog or toolset
    pub root: Uuid,
    /// The events coalesced into this build, each once
    pub events: Vec<Uuid>,
    /// Each event and how it reached the root, once per tag it was looked up by
    causes: Vec<Cause<T>>,
}

impl<T: Clone> Affected<T> {
    /// Start tracking a root that events affect
    ///
    /// # Arguments
    ///
    /// * `root` - The workflow catalog or toolset
    #[must_use]
    pub fn new(root: Uuid) -> Self {
        // no event is coalesced yet
        Affected {
            root,
            events: Vec::new(),
            causes: Vec::new(),
        }
    }

    /// Coalesce an event into this root's build
    ///
    /// # Arguments
    ///
    /// * `event` - The event
    /// * `trigger` - What the event's change records as the build's trigger
    /// * `manual` - Whether the event is a user's request for an instance
    /// * `position` - The event's place in the batch
    /// * `reach` - How the event reached this root
    pub fn add(&mut self, event: &Event, trigger: T, manual: bool, position: usize, reach: Reach) {
        // remember the event once, even when it matched several of the root's instances
        if self.events.last() != Some(&event.id) && !self.events.contains(&event.id) {
            self.events.push(event.id);
        }
        // split how the event reached the root into its lookup and the instance it matched
        let (lookup, instance) = match reach {
            Reach::Direct => (None, None),
            Reach::Tagged {
                key,
                value,
                instance,
            } => (Some((key, value)), Some(instance)),
        };
        // add a match to the event's cause for the same lookup, or start a new cause
        if let Some(cause) = self
            .causes
            .iter_mut()
            .find(|cause| cause.event == event.id && cause.lookup == lookup)
        {
            cause.matched.extend(instance);
            return;
        }
        self.causes.push(Cause {
            event: event.id,
            trigger,
            user: event.user.clone(),
            manual,
            lookup,
            matched: instance.into_iter().collect(),
            at: (event.timestamp, position),
        });
    }

    /// Whether an event names this root itself rather than reaching it through a tag lookup
    #[must_use]
    pub fn direct(&self) -> bool {
        // any cause without a lookup named the root
        self.causes.iter().any(|cause| cause.lookup.is_none())
    }

    /// Get every instance of this root a tag lookup matched
    #[must_use]
    pub fn matched(&self) -> BTreeSet<Uuid> {
        // gather the matches of every cause
        self.causes
            .iter()
            .flat_map(|cause| cause.matched.iter().copied())
            .collect()
    }

    /// Get the users who asked for an instance, each once, in the order they asked
    #[must_use]
    pub fn requesters(&self) -> Vec<String> {
        // keep each manual requester once, oldest request first
        let mut manual = self
            .causes
            .iter()
            .filter(|cause| cause.manual)
            .collect::<Vec<&Cause<T>>>();
        manual.sort_by_key(|cause| cause.at);
        let mut requesters: Vec<String> = Vec::with_capacity(manual.len());
        for cause in manual {
            if !requesters.contains(&cause.user) {
                requesters.push(cause.user.clone());
            }
        }
        requesters
    }

    /// Whether deciding this root's build needs its newest instance's tags: a looked up event
    /// that didn't match the newest instance can still qualify through them
    ///
    /// # Arguments
    ///
    /// * `auto_instance` - Whether the root builds instances automatically
    /// * `newest` - The root's newest instance, if it has one
    #[must_use]
    pub fn needs_newest_tags(&self, auto_instance: bool, newest: Option<Uuid>) -> bool {
        // only automatic builds look at lookups, and only a missed newest instance needs its tags
        auto_instance
            && newest.is_some_and(|newest| {
                self.causes
                    .iter()
                    .any(|cause| cause.lookup.is_some() && !cause.matched.contains(&newest))
            })
    }

    /// Decide what this root's build records and who is told, or that nothing builds it
    ///
    /// The latest qualifying event decides the trigger and the user; nothing builds the root when
    /// no event qualifies.
    ///
    /// # Arguments
    ///
    /// * `auto_instance` - Whether the root builds instances automatically
    /// * `newest` - The root's newest instance, if it has one
    /// * `newest_tags` - The newest instance's tags, when they were read
    #[must_use]
    pub fn decide(
        &self,
        auto_instance: bool,
        newest: Option<Uuid>,
        newest_tags: Option<&thorium::models::TagMap>,
    ) -> Option<BuildCause<T>> {
        // find the latest event that qualifies
        let latest = self
            .causes
            .iter()
            .filter(|cause| cause.qualifies(auto_instance, newest, newest_tags))
            .max_by_key(|cause| cause.at)?;
        // tell its user, then each manual requester who isn't them
        let mut recipients = vec![latest.user.clone()];
        recipients.extend(
            self.requesters()
                .into_iter()
                .filter(|user| *user != latest.user),
        );
        Some(BuildCause {
            trigger: latest.trigger.clone(),
            triggered_by: latest.user.clone(),
            recipients,
        })
    }
}

/// Find the roots whose instances carry a tag in some groups, with the matched instances of each
///
/// The lookup lists only instance ids. Finding an instance's root reads that one instance, and
/// then the ids of every instance of the root are listed, so the root's other matches are never
/// read. That listing is skipped when no match is left to pair or the instance read is its
/// root's first revision, which has no older revisions; a newer revision that matched is then
/// read for its own root.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `kind` - The instance kind, `WorkflowInstance` or `ToolSetInstance`
/// * `key` - The tag key
/// * `value` - The tag value
/// * `groups` - The groups to search
pub async fn tagged_roots<R: GraphReader + ?Sized>(
    reader: &R,
    kind: thorium::models::EntityKinds,
    key: &str,
    value: &str,
    groups: &[String],
) -> Result<Vec<(Uuid, Uuid)>, Error> {
    // list the ids of the tagged instances
    let ids = reader.tagged_ids(kind, key, value, groups).await?;
    let mut left = ids.iter().copied().collect::<BTreeSet<Uuid>>();
    let mut found = Vec::with_capacity(ids.len());
    for id in ids {
        // skip the instances already paired with their root
        if !left.remove(&id) {
            continue;
        }
        // read the instance for its root; one deleted since the lookup matches nothing
        let Some(node) = reader
            .node(id, &[], thorium::models::Directionality::To)
            .await?
        else {
            continue;
        };
        let (root, root_tag, revision) = match &node.entity.metadata {
            thorium::models::EntityMetadata::WorkflowInstance(instance) => (
                instance.workflow,
                thorium::models::entities::workflows::WORKFLOW_ID_TAG,
                instance.revision,
            ),
            thorium::models::EntityMetadata::ToolSetInstance(instance) => (
                instance.tool_set,
                thorium::models::entities::toolsets::TOOL_SET_ID_TAG,
                instance.revision,
            ),
            _ => continue,
        };
        found.push((root, id));
        // pair the root's other matched instances without reading them, unless none is left or
        // this first revision has no older ones
        if !left.is_empty() && revision > 1 {
            let siblings = reader
                .tagged_ids(kind, root_tag, &root.to_string(), groups)
                .await?;
            for sibling in siblings {
                if left.remove(&sibling) {
                    found.push((root, sibling));
                }
            }
        }
    }
    Ok(found)
}

/// What processing a batch of workflow graph events did
#[derive(Debug, Default)]
pub struct PhaseOutcome {
    /// The events that were finished and can be cleared
    pub cleared: Vec<Uuid>,
    /// The events left in flight to be retried
    pub in_flight: Vec<Uuid>,
    /// The stats for the batch
    pub stats: WorkflowsWorkerStats,
}

impl PhaseOutcome {
    /// Sort a batch's workflow graph events into the finished ones and the ones left in flight
    ///
    /// # Arguments
    ///
    /// * `events` - The batch, oldest first
    /// * `held` - The events to leave in flight
    pub fn sort(&mut self, events: &[Event], held: &BTreeSet<Uuid>) {
        // every workflow graph event that isn't held is finished
        for event in events {
            if !matches!(event.data, EventData::WorkflowGraph(_)) {
                continue;
            }
            if held.contains(&event.id) {
                self.in_flight.push(event.id);
            } else {
                self.cleared.push(event.id);
            }
        }
        self.stats.events_processed = self.cleared.len();
    }

    /// Combine the outcomes of two phases that each processed the same batch
    ///
    /// An event is finished only when both phases finished it.
    ///
    /// # Arguments
    ///
    /// * `other` - The other phase's outcome
    #[must_use]
    pub fn merge(mut self, other: PhaseOutcome) -> PhaseOutcome {
        // an event either phase left in flight stays in flight
        let mut held = self
            .in_flight
            .iter()
            .chain(other.in_flight.iter())
            .copied()
            .collect::<HashSet<Uuid>>();
        let mut cleared = Vec::with_capacity(self.cleared.len());
        for id in self.cleared.into_iter().chain(other.cleared) {
            if held.insert(id) {
                cleared.push(id);
            }
        }
        // keep each held event once, in the order the phases left them
        let mut in_flight = Vec::with_capacity(self.in_flight.len());
        let mut seen = HashSet::new();
        for id in self.in_flight.into_iter().chain(other.in_flight) {
            if seen.insert(id) {
                in_flight.push(id);
            }
        }
        // add the phases' stats and count each finished event once
        self.stats += other.stats;
        self.stats.events_processed = cleared.len();
        PhaseOutcome {
            cleared,
            in_flight,
            stats: self.stats,
        }
    }
}

/// Tell some users how a build ended, counting each notification sent or failed
///
/// A failed notification is logged and counted but never fails the build, since the instance
/// already exists and can still be found by polling.
///
/// # Arguments
///
/// * `api` - The API the notifications are written through
/// * `recipients` - The users to tell
/// * `note` - The notification
/// * `stats` - The stats to count in
pub async fn notify_all<A: InstanceApi + ?Sized>(
    api: &A,
    recipients: &[String],
    note: &NotificationRequest<User>,
    stats: &mut WorkflowsWorkerStats,
) {
    // write one notification per recipient
    for username in recipients {
        match api.notify(username, note).await {
            Ok(()) => stats.notifications_sent += 1,
            Err(error) => {
                event!(
                    Level::ERROR,
                    msg = "Failed to notify a user about a build",
                    user = username,
                    error = error.to_string()
                );
                stats.notification_failures += 1;
            }
        }
    }
}

/// Build the toolset and then the workflow instances a batch of workflow graph events calls for
///
/// The toolsets are built first, so a workflow built in the same batch already resolves its
/// toolsets to the instances this batch created. An event is finished only when both phases
/// finished it.
///
/// # Arguments
///
/// * `api` - The API reads and writes go through
/// * `limits` - The crawl limits workflows are resolved under
/// * `tool_set_limits` - The limits toolsets are resolved under
/// * `events` - The batch, oldest first
pub async fn process_batch<A: InstanceApi + ?Sized>(
    api: &A,
    limits: CrawlLimits,
    tool_set_limits: ToolSetLimits,
    events: &[Event],
) -> PhaseOutcome {
    // build the toolsets, then the workflows
    let tool_sets = Box::pin(ToolSetInstancer::new(api, tool_set_limits).process(events)).await;
    let workflows = Box::pin(WorkflowInstancer::new(api, limits).process(events)).await;
    tool_sets.merge(workflows)
}

/// Get when the events left in flight should be retried
///
/// The earliest retry time is kept, so later failures never push a retry back.
///
/// # Arguments
///
/// * `left_in_flight` - Whether this batch left any events in flight
/// * `current` - The retry time already set, if any
#[must_use]
pub fn retry_after(left_in_flight: bool, current: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    // keep a retry time that's already set, or set one when events were left in flight
    match current {
        Some(retry_ts) => Some(retry_ts),
        None if left_in_flight => Some(Utc::now() + chrono::Duration::minutes(RETRY_MINUTES)),
        None => None,
    }
}

/// The worker for the agentic event types
pub struct WorkflowsWorker {
    /// The event handler's admin client
    thorium: Arc<Thorium>,
    /// The crawl limits workflows are resolved under
    limits: CrawlLimits,
    /// The limits toolsets are resolved under
    tool_set_limits: ToolSetLimits,
    /// When to retry the events left in flight, if any were
    retry_ts: Option<DateTime<Utc>>,
    /// The clients refreshes act as, one per user
    clients: UserClients,
    /// The client model listings are sent with
    listing: reqwest::Client,
    /// Where listing sign-in requests go
    hosts: AuthHosts,
    /// The bounds model refreshes run under
    refresh_limits: RefreshLimits,
}

impl WorkflowsWorker {
    /// Build a workflows worker over an admin client
    ///
    /// # Arguments
    ///
    /// * `thorium` - The event handler's admin client
    /// * `limits` - The crawl limits workflows are resolved under
    /// * `tool_set_limits` - The limits toolsets are resolved under
    pub fn with_client(
        thorium: Arc<Thorium>,
        limits: CrawlLimits,
        tool_set_limits: ToolSetLimits,
    ) -> Result<Self, Error> {
        // listings are bounded by the listing client's timeouts
        let listing = providers::listing_client()?;
        // nothing is waiting to be retried yet and no user clients are built
        Ok(WorkflowsWorker {
            thorium,
            limits,
            tool_set_limits,
            retry_ts: None,
            clients: UserClients::default(),
            listing,
            hosts: AuthHosts::default(),
            refresh_limits: RefreshLimits::default(),
        })
    }

    /// Send listing sign-in requests to other hosts
    ///
    /// # Arguments
    ///
    /// * `hosts` - Where sign-in requests go
    #[must_use]
    pub fn auth_hosts(mut self, hosts: AuthHosts) -> Self {
        self.hosts = hosts;
        self
    }

    /// Run model refreshes under other bounds
    ///
    /// # Arguments
    ///
    /// * `limits` - The bounds model refreshes run under
    #[must_use]
    pub fn refresh_limits(mut self, limits: RefreshLimits) -> Self {
        self.refresh_limits = limits;
        self
    }

    /// Whether a client is cached for a user
    ///
    /// # Arguments
    ///
    /// * `username` - The user
    #[must_use]
    pub fn has_user_client(&self, username: &str) -> bool {
        self.clients.has_client(username)
    }
}

impl EventWorkerSupportCore for WorkflowsWorker {
    /// The stats for the workflows worker
    type Stats = WorkflowsWorkerStats;

    /// Build the workflows worker with a client for every user the event handler knows
    ///
    /// # Arguments
    ///
    /// * `cache` - The shared cache, which holds the Thorium config and the users
    /// * `thorium` - The event handler's admin client
    ///
    /// # Panics
    ///
    /// Panics if the listing client can't be built, which only happens when TLS can't start
    // the trait builds workers asynchronously, though this one has nothing to wait on
    #[allow(clippy::unused_async_trait_impl)]
    async fn new(cache: &EventWorkerCache, thorium: &Arc<Thorium>) -> Self {
        // resolve workflows and toolsets under the configured limits
        let mut worker = match Self::with_client(
            thorium.clone(),
            CrawlLimits::from(&cache.conf.thorium.events),
            ToolSetLimits::from(&cache.conf.thorium.events),
        ) {
            Ok(worker) => worker,
            Err(error) => panic!("Failed to build the workflows worker: {error}"),
        };
        // build a client for each user known at startup
        worker.clients.apply_users(thorium, cache.users.clone());
        worker
    }

    /// The types of events this worker handles, refreshes first so they never wait behind a
    /// large instancing batch
    #[inline]
    fn event_kinds() -> Vec<EventType> {
        vec![EventType::ModelRefresh, EventType::WorkflowGraph]
    }

    /// The workflows worker keeps a client for each user refreshes act as
    #[inline]
    fn cache_subscriptions() -> Vec<WorkerCacheUpdateKinds> {
        vec![WorkerCacheUpdateKinds::Users]
    }

    /// Rebuild the user clients from an update to the known users
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply
    fn apply_cache_update(&mut self, update: WorkerCacheUpdates) {
        // only user updates matter to the workflows worker
        if let WorkerCacheUpdates::Users(users) = update {
            self.clients.apply_users(&self.thorium, users);
        }
    }

    /// Report our latest stats in an event
    ///
    /// # Arguments
    ///
    /// * `stats` - The new stats to log
    /// * `_prior` - The previously reported stats
    #[instrument(name = "EventWorkerSupportCore::stats", skip_all)]
    fn stats(stats: &Self::Stats, _prior: &Self::Stats) {
        // the controller only reports stats that changed
        event!(
            Level::INFO,
            worker = "WorkflowsWorker",
            events_processed = stats.events_processed,
            instances_created = stats.instances_created,
            skipped = stats.skipped,
            invalid = stats.invalid,
            notifications_sent = stats.notifications_sent,
            notification_failures = stats.notification_failures,
            tag_lookups = stats.tag_lookups,
            graph_reads = stats.graph_reads,
            tool_set_instances_created = stats.tool_set_instances_created,
            tool_set_skipped = stats.tool_set_skipped,
            tool_set_invalid = stats.tool_set_invalid,
            models_added = stats.models_added,
            bans_added = stats.bans_added,
            bans_cleared = stats.bans_cleared,
            listing_failures = stats.listing_failures,
            user_lookups = stats.user_lookups,
        );
    }
}

impl EventWorkerMutableSupport for WorkflowsWorker {
    /// Refresh the models or build the instances a batch of events calls for, and clear the
    /// events that were finished
    ///
    /// Refreshes and instance builds run side by side, so a slow provider never holds up the
    /// workflow graph events popped with its refresh. Only a failure to clear events (a Redis
    /// queue operation) ends the worker; a failed build or refresh leaves its events in flight
    /// to be retried.
    ///
    /// # Arguments
    ///
    /// * `events` - The events to process
    /// * `stats` - The stats to update
    #[instrument(name = "WorkflowsWorker::process", skip_all, fields(events = events.len()), err(Debug))]
    async fn process(
        &mut self,
        events: Vec<Event>,
        stats: &mut WorkflowsWorkerStats,
    ) -> Result<(), Error> {
        // split the batch by event type
        let (refreshes, graph): (Vec<Event>, Vec<Event>) = events
            .into_iter()
            .filter(|event| {
                matches!(
                    event.data,
                    EventData::ModelRefresh(_) | EventData::WorkflowGraph(_)
                )
            })
            .partition(|event| matches!(event.data, EventData::ModelRefresh(_)));
        // refresh the models the refresh events name
        let model_refresher = ModelRefresher::new(
            &self.thorium,
            &self.listing,
            &self.hosts,
            self.refresh_limits,
        );
        let clients = &mut self.clients;
        let refresh_phase = async {
            if refreshes.is_empty() {
                return None;
            }
            Some(Box::pin(model_refresher.process(clients, &refreshes)).await)
        };
        // build the toolsets and then the workflows the graph events call for with the admin
        // client, alongside the refreshes
        let thorium = &*self.thorium;
        let (limits, tool_set_limits) = (self.limits, self.tool_set_limits);
        let build_phase = async {
            if graph.is_empty() {
                return None;
            }
            Some(Box::pin(process_batch(thorium, limits, tool_set_limits, &graph)).await)
        };
        let (refresh_outcome, build_outcome) = tokio::join!(refresh_phase, build_phase);
        // clear the finished refreshes and retry the rest later
        if let Some(outcome) = refresh_outcome {
            if !outcome.cleared.is_empty() {
                self.thorium
                    .events
                    .clear(EventType::ModelRefresh, &EventIds::from(outcome.cleared))
                    .await?;
            }
            self.retry_ts = retry_after(!outcome.in_flight.is_empty(), self.retry_ts);
            *stats += outcome.stats;
        }
        // clear the finished graph events and retry the rest later
        if let Some(outcome) = build_outcome {
            if !outcome.cleared.is_empty() {
                self.thorium
                    .events
                    .clear(EventType::WorkflowGraph, &EventIds::from(outcome.cleared))
                    .await?;
            }
            self.retry_ts = retry_after(!outcome.in_flight.is_empty(), self.retry_ts);
            *stats += outcome.stats;
        }
        Ok(())
    }

    /// Take the time to retry the events this worker left in flight
    // the trait reads the retry time asynchronously, though this worker holds it directly
    #[allow(clippy::unused_async_trait_impl)]
    async fn take_retry_ts(&mut self) -> Option<DateTime<Utc>> {
        self.retry_ts.take()
    }
}
