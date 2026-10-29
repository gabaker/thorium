//! The events in Thorium for triggers and other things to act on

use chrono::prelude::*;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use std::sync::LazyLock;
use uuid::Uuid;

use crate::models::{AssociationKind, ModelConfigRef, OutputKey, SigmaRuleAppliesTo};

use super::TagType;

/// A [`Regex`] matching only valid event handler worker ids
///
/// Worker ids are embedded in Redis key names so they are restricted to a
/// short list of safe characters and a bounded length.
static WORKER_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[A-Za-z0-9._-]{1,128}\z").unwrap());

/// Check whether an event handler worker id is valid
///
/// Worker ids must fully match `^[A-Za-z0-9._-]{1,128}$` since they are
/// embedded in Redis key names.
///
/// # Arguments
///
/// * `worker` - The worker id to validate
pub fn valid_worker_id(worker: &str) -> bool {
    // make sure this worker id only contains key safe characters
    WORKER_ID_REGEX.is_match(worker)
}

/// The different types of events
#[derive(
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
    strum::EnumIter,
)]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreAsStr))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum EventType {
    /// A trigger that may cause a reaction to be spawned
    ReactionTrigger,
    /// A result with new entities was added and can be scanned by a sigma rule
    SigmaScannableResults,
    /// A samples bytes became visible to new groups and can be scanned by yara rules
    YaraScannableSample,
    /// A chunk of a yara hunt is ready to advance one lane
    ///
    /// Each engine gets its own chunk event kind so engines keep separate
    /// queues, workers, budgets, and depth gauges while sharing one chassis.
    YaraHuntChunk,
    /// A change that may require new workflow or toolset instances to be created
    WorkflowGraph,
    /// A model endpoint or credential whose models should be listed and checked
    ModelRefresh,
}

impl EventType {
    /// Cast our event type to a str
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// Whether this event kind carries paced hunt chunk work
    ///
    /// Today exactly `YaraHuntChunk`; a future engines chunk kind joins the
    /// hunt backlog views for free by matching here.
    #[must_use]
    pub fn is_hunt_chunk(&self) -> bool {
        matches!(self, EventType::YaraHuntChunk)
    }
}

impl From<&EventData> for EventType {
    /// Get the event type for some event data
    ///
    /// # Arguments
    ///
    /// * `data` - The event data to get the event type of
    fn from(data: &EventData) -> Self {
        match data {
            EventData::NewSample { .. } => EventType::ReactionTrigger,
            EventData::NewTags { .. } => EventType::ReactionTrigger,
            EventData::SigmaScannableResults { .. } => EventType::SigmaScannableResults,
            EventData::YaraScannableSample { .. } => EventType::YaraScannableSample,
            EventData::YaraHuntChunk { .. } => EventType::YaraHuntChunk,
            EventData::WorkflowGraph(_) => EventType::WorkflowGraph,
            EventData::ModelRefresh(_) => EventType::ModelRefresh,
        }
    }
}

/// What caused a yara scan event to be emitted
///
/// Historical scans are covered by user-created hunts on their own chunk
/// queue, so there is deliberately no backscan trigger variant here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum YaraScanTrigger {
    /// A new submission made this sample visible to a set of groups
    NewSubmission,
    /// Groups were added to an existing submission
    GroupsAdded,
    /// Groups were removed from a submission or a submission was deleted
    GroupsRemoved,
}

/// An event for a sample that yara rules can scan
///
/// The originating trigger depth rides on the enclosing [`Event`]'s `depth`
/// field so the loop breaking max depth guard applies to tags written for
/// hits on this sample.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub struct YaraSampleScanEvent {
    /// The sha256 of the sample to scan
    pub sha256: String,
    /// The groups whose visibility of this sample just changed
    pub groups: Vec<String>,
    /// What caused this scan event to be emitted
    pub trigger: YaraScanTrigger,
}

/// An event for a sigma scannable result
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub struct SigmaScannableResultsEvent {
    /// The key to the output we want to scan
    pub key: OutputKey,
    /// The groups these results/entities were added too
    pub groups: Vec<String>,
    /// What rules we can apply to these entities
    pub applies_to: Vec<SigmaRuleAppliesTo>,
    /// The tool that created these results/entities
    pub tool: String,
}

/// An event advancing one lane of a hunt by one chunk
///
/// Each engine's [`EventType`] variant carries this same payload (M4 ships
/// `EventType::YaraHuntChunk`), so engines keep separate queues, workers,
/// budgets, and depth gauges while sharing one chassis.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub struct HuntChunkEvent {
    /// The hunt entity id
    pub hunt: Uuid,
    /// The lane index this chunk advances
    pub lane: u32,
    /// The checkpoint sequence this chunk will try to commit
    pub seq: u64,
}

/// The data for an event
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub enum EventData {
    /// A new sample was uploaded
    NewSample { groups: Vec<String>, sample: String },
    /// Some new tags were added
    NewTags {
        /// The type of tags that were created
        tag_type: TagType,
        /// The item that tags were added too
        item: String,
        /// The groups these tags were added too
        groups: Vec<String>,
        /// The new tags that were added
        tags: HashMap<String, HashSet<String>>,
    },
    /// A result with sigma scannable entities was added
    SigmaScannableResults(SigmaScannableResultsEvent),
    /// A samples bytes became visible to new groups and can be scanned by yara rules
    YaraScannableSample(YaraSampleScanEvent),
    /// A chunk of a yara hunt is ready to advance one lane
    YaraHuntChunk(HuntChunkEvent),
    /// A change that may require new workflow or toolset instances to be created
    WorkflowGraph(WorkflowGraphEvent),
    /// A model endpoint or credential whose models should be listed and checked
    ModelRefresh(ModelRefreshEvent),
}

/// A change that may require new workflow or toolset instances to be created
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkflowGraphEvent {
    /// Every group the changed entity is in (the event handler's crawl stays inside them)
    pub groups: Vec<String>,
    /// What changed
    pub change: WorkflowGraphChange,
}

/// The kinds of changes that can affect workflow and toolset instances
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum WorkflowGraphChange {
    /// A workflow catalog was created or updated
    WorkflowUpdated {
        /// The workflow catalog that changed
        workflow: Uuid,
    },
    /// A tool got a new latest instance (created, or deprecation moved latest)
    ToolUpdated {
        /// The tool that changed
        tool: Uuid,
        /// The tool's new latest instance, or the nil id when every instance is deprecated
        tool_instance: Uuid,
    },
    /// An agent's configuration changed
    AgentUpdated {
        /// The agent that changed
        agent: Uuid,
    },
    /// A graph association was created or deleted
    EdgeChanged {
        /// The source of the association
        source: Uuid,
        /// The target of the association
        target: Uuid,
        /// The kind of association that changed
        kind: AssociationKind,
    },
    /// A user requested a new instance of a workflow
    ManualInstance {
        /// The workflow to instance
        workflow: Uuid,
    },
    /// A toolset's own fields changed (created, its read only flag, selector, or auto instance
    /// edited, or its groups changed)
    ToolSetChanged {
        /// The toolset that changed
        tool_set: Uuid,
    },
    /// A toolset got a new ready instance
    ToolSetUpdated {
        /// The toolset that changed
        tool_set: Uuid,
        /// The toolset's new ready instance
        tool_set_instance: Uuid,
    },
    /// A user requested a new instance of a toolset
    ManualToolSetInstance {
        /// The toolset to instance
        tool_set: Uuid,
    },
    /// User tags were added to or removed from a tool (emitted only for Tool entities)
    ToolTagsChanged {
        /// The tool whose tags changed
        tool: Uuid,
        /// The tag keys and values that were added or removed
        tags: BTreeMap<String, BTreeSet<String>>,
    },
}

/// A model endpoint or credential whose models should be listed and checked
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelRefreshEvent {
    /// What to list and check
    pub target: ModelRefreshTarget,
    /// Why the refresh was requested
    pub reason: ModelRefreshReason,
}

/// What a model refresh lists and checks
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelRefreshTarget {
    /// A group endpoint entity, with its group credential (or a member's own credential when it
    /// has none)
    GroupEndpoint {
        /// Every group the endpoint is in
        groups: Vec<String>,
        /// The endpoint entity
        endpoint: Uuid,
    },
    /// One endpoint with one user's own credential (one of the user's endpoints, or a group
    /// endpoint with no group credential)
    UserEndpoint {
        /// The user whose credential is used
        user: String,
        /// The endpoint (a user endpoint id or a group endpoint entity id)
        endpoint: Uuid,
    },
    /// Every endpoint and credential in one user's model settings
    User {
        /// The user to check
        user: String,
    },
}

impl ModelRefreshTarget {
    /// Get the endpoint this target names, if it names one
    #[must_use]
    pub fn endpoint(&self) -> Option<Uuid> {
        // only the endpoint targets name one endpoint
        match self {
            ModelRefreshTarget::GroupEndpoint { endpoint, .. }
            | ModelRefreshTarget::UserEndpoint { endpoint, .. } => Some(*endpoint),
            ModelRefreshTarget::User { .. } => None,
        }
    }
}

/// Why a model refresh was requested
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelRefreshReason {
    /// A group owner, manager, or admin asked for it through the refresh route
    Manual,
    /// The endpoint was created or its provider connection changed
    EndpointChanged,
    /// A credential the target uses was created or rotated
    CredentialChanged {
        /// The credential that changed
        credential: ModelConfigRef,
    },
}

/// An request for a new event in Thorium
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventRequest {
    /// The parent for this event
    pub parent: Uuid,
    /// this events data
    pub data: EventData,
}

/// Whether an trigger is, could be, or can not be triggered
pub enum TriggerPotential {
    /// The triggers conditions are met by an event
    Confirmed,
    /// This trigger conditions could potentially be met by an event
    Potentially,
    /// This triggers conditions can not be met by an event
    CanNot,
}

/// An event in Thorium
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct Event {
    /// The id for this event
    pub id: Uuid,
    /// The timestamp for this event
    pub timestamp: DateTime<Utc>,
    /// The parent for this event
    pub parent: Option<Uuid>,
    /// The user that caused this trigger
    pub user: String,
    /// this events data
    pub data: EventData,
    /// This events current trigger depth,
    pub depth: u8,
}

impl Event {
    /// Check if all conditions of this tag trigger have been met or not
    ///
    /// Unlike `check_tag_trigger`, this returns a plain yes or no instead of a trigger potential
    #[must_use]
    pub fn check_all_tag_trigger(
        visible: &[String],
        tags: &HashMap<String, HashMap<String, HashSet<String>>>,
        required: &HashMap<String, Vec<String>>,
        not: &HashMap<String, Vec<String>>,
    ) -> bool {
        // check if any of our not values were set
        for (not_key, not_values) in not {
            // check if this not key was set
            if let Some(values) = tags.get(not_key) {
                // check if any of our not values were set
                if not_values
                    .iter()
                    .filter_map(|not_value| values.get(not_value))
                    .any(|groups| groups.iter().any(|group| visible.contains(group)))
                {
                    // this trigger will not trigger as it contains a not value
                    return false;
                }
            }
        }
        // check any of our required values were set
        for (req_key, req_values) in required {
            // check if this required key was set
            match tags.get(req_key) {
                // A required tag key exists
                Some(values) => {
                    // check that every required value is set in a group we can see; a required
                    // value that isn't set at all fails this check
                    if !req_values.iter().all(|req| {
                        values.get(req).is_some_and(|groups| {
                            groups.iter().any(|group| visible.contains(group))
                        })
                    }) {
                        // this trigger will not trigger as its missing a required tag value
                        return false;
                    }
                }
                // this trigger will not trigger as its missing a required tag key
                None => return false,
            }
        }
        // This triggers conditions have been fully met
        true
    }

    /// Check if a new tag event trigger occured
    fn check_tag_trigger(
        new_type: &TagType,
        new_tags: &HashMap<String, HashSet<String>>,
        trigger_types: &[TagType],
        required: &HashMap<String, Vec<String>>,
        not: &HashMap<String, Vec<String>>,
    ) -> TriggerPotential {
        // make sure our tag types match
        if trigger_types.contains(new_type) {
            // check if any of our not values were set
            for (not_key, not_values) in not {
                // check if this not key was set
                if let Some(values) = new_tags.get(not_key) {
                    // check if any of our not values were set
                    if not_values
                        .iter()
                        .any(|not_value| values.contains(not_value))
                    {
                        // this trigger will not trigger
                        return TriggerPotential::CanNot;
                    }
                }
            }
            // check any of our required values were set
            for (req_key, req_values) in required {
                // check if this required key was set
                if let Some(values) = new_tags.get(req_key) {
                    // check if any of our required values were set
                    if req_values.iter().any(|req| values.contains(req)) {
                        // this trigger may potentially trigger
                        return TriggerPotential::Potentially;
                    }
                }
            }
        }
        // default to this trigger will not trigger
        TriggerPotential::CanNot
    }
    /// Check if this event could potentially trigger a trigger
    ///
    /// # Arguments
    ///
    /// * `trigger` - The trigger to check this event against
    #[must_use]
    pub fn could_trigger(&self, trigger: &EventTrigger) -> TriggerPotential {
        match (&self.data, trigger) {
            (EventData::NewSample { .. }, EventTrigger::NewSample) => TriggerPotential::Confirmed,
            (
                EventData::NewTags { tag_type, tags, .. },
                EventTrigger::Tag {
                    tag_types,
                    required,
                    not,
                },
            ) => Self::check_tag_trigger(tag_type, tags, tag_types, required, not),
            (EventData::NewSample { .. }, EventTrigger::Tag { .. }) => TriggerPotential::CanNot,
            (EventData::NewTags { .. }, EventTrigger::NewSample) => TriggerPotential::CanNot,
            (EventData::SigmaScannableResults { .. }, _) => TriggerPotential::CanNot,
            // yara scan events are consumed by the yara scanner, never by reaction triggers
            (EventData::YaraScannableSample { .. }, _) => TriggerPotential::CanNot,
            // hunt chunk events are consumed by hunt workers, never by reaction triggers
            (EventData::YaraHuntChunk { .. }, _) => TriggerPotential::CanNot,
            // workflow graph changes only rebuild instances and never start reactions
            (EventData::WorkflowGraph(_), _) => TriggerPotential::CanNot,
            // model refreshes only update model catalogs and never start reactions
            (EventData::ModelRefresh(_), _) => TriggerPotential::CanNot,
        }
    }
}

/// A list of event ids
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EventIds {
    /// A list of event ids
    pub ids: Vec<Uuid>,
}

impl From<Vec<Uuid>> for EventIds {
    fn from(ids: Vec<Uuid>) -> Self {
        EventIds { ids }
    }
}

impl FromIterator<Uuid> for EventIds {
    fn from_iter<T: IntoIterator<Item = Uuid>>(iter: T) -> Self {
        // get our ids as a list
        let ids = iter.into_iter().collect();
        // build our event id list
        EventIds { ids }
    }
}

impl From<std::vec::Drain<'_, Uuid>> for EventIds {
    fn from(drain: std::vec::Drain<'_, Uuid>) -> Self {
        // get our ids as a list
        let ids = drain.collect();
        // build our event id list
        EventIds { ids }
    }
}

/// Default the event pop limit to 50
fn default_event_pop_limit() -> usize {
    50
}

/// The params for popping events
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct EventPopOpts {
    /// The max number of events to pop and return
    #[serde(default = "default_event_pop_limit")]
    pub limit: usize,
    /// The worker that will own the popped events
    ///
    /// Owned pops land in this workers own in flight keys so its events can be
    /// reclaimed if it dies; owner-less pops keep the legacy shared in flight
    /// keys for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

impl Default for EventPopOpts {
    /// Create a default `EventPopOpts`
    fn default() -> Self {
        EventPopOpts {
            limit: default_event_pop_limit(),
            worker: None,
        }
    }
}

impl EventPopOpts {
    /// Set the maximum number of events to pop
    ///
    /// # Arguments
    ///
    /// * `limit` - The limit to set
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the worker that will own the popped events
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker id to own the popped events
    #[must_use]
    pub fn worker<T: Into<String>>(mut self, worker: T) -> Self {
        // set the worker that will own the popped events
        self.worker = Some(worker.into());
        self
    }
}

/// Default the event peek limit to 1000
fn default_event_peek_limit() -> usize {
    1000
}

/// The params for peeking at queued events
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct EventPeekOpts {
    /// The max number of queued events to return
    #[serde(default = "default_event_peek_limit")]
    pub limit: usize,
}

impl Default for EventPeekOpts {
    /// Create a default `EventPeekOpts`
    fn default() -> Self {
        EventPeekOpts {
            limit: default_event_peek_limit(),
        }
    }
}

impl EventPeekOpts {
    /// Set the maximum number of queued events to return
    ///
    /// # Arguments
    ///
    /// * `limit` - The limit to set
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        // set our new limit
        self.limit = limit;
        self
    }
}

/// The different kind of event triggers
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum EventTrigger {
    /// A trigger based on an event
    Tag {
        /// The types of tags we can trigger on
        tag_types: Vec<TagType>,
        /// The tags to require to be set
        required: HashMap<String, Vec<String>>,
        /// The tags to not run on if set
        not: HashMap<String, Vec<String>>,
    },
    /// A trigger based on a new sample
    NewSample,
}

/// The current event marks
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EventMarks {
    /// The timestamp for oldest event to act on
    pub watermark: DateTime<Utc>,
    /// The timestamp for the currently newest event to act on
    pub tidemark: Option<DateTime<Utc>>,
}

/// The current status of our event cache
///
/// Each component of the event handlers cache has its own staleness flag so a
/// change to one component (e.g. a new yara rule) only rebuilds that component.
/// All fields default to false so an older API that only wrote the legacy
/// single flag deserializes safely during a rolling upgrade.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EventCacheStatus {
    /// Whether we need to refresh our triggers cache
    #[serde(default)]
    pub triggers: bool,
    /// Whether we need to refresh our users cache
    #[serde(default)]
    pub users: bool,
    /// Whether we need to refresh our sigma rules cache
    #[serde(default)]
    pub sigma_rules: bool,
    /// Whether we need to refresh our yara rules cache
    #[serde(default)]
    pub yara_rules: bool,
    /// The monotonic generation counter for the triggers cache
    ///
    /// Unlike the staleness flags generation counters are never cleared so any
    /// number of replicas can poll them without consuming each others updates.
    #[serde(default)]
    pub triggers_gen: u64,
    /// The monotonic generation counter for the users cache
    #[serde(default)]
    pub users_gen: u64,
    /// The monotonic generation counter for the sigma rules cache
    #[serde(default)]
    pub sigma_rules_gen: u64,
    /// The monotonic generation counter for the yara rules cache
    #[serde(default)]
    pub yara_rules_gen: u64,
}

/// The queue depths for a single kind of event
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct KindDepth {
    /// The number of events waiting in the pending queue
    pub queued: u64,
    /// The number of events currently in flight across all owners and the
    /// legacy un-owned in flight queue
    pub in_flight: u64,
}

/// The queue depths for every kind of event plus the live worker count
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EventDepths {
    /// The queue depths for each kind of event
    pub kinds: HashMap<EventType, KindDepth>,
    /// The number of event handler workers with a live heartbeat
    pub workers: u64,
}

/// A summary of a single reap pass over the event handler worker registry
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReapSummary {
    /// The number of dead workers that were removed from the registry
    pub dead_workers: u64,
    /// The number of events reclaimed from dead workers
    pub reclaimed: u64,
    /// The number of events reclaimed from live workers by the age backstop
    pub backstopped: u64,
    /// The number of events reclaimed from the legacy un-owned in flight keys
    pub legacy_reclaimed: u64,
}

/// One workers cumulative counters for one event kind
///
/// Counters are cumulative since process start: a worker restart resets them
/// to zero and event replay after a crash can inflate them, so consumers
/// derive rates between readings and clamp negative deltas to zero. Flat
/// stats key by field name while nested stats flatten to dotted paths
/// (`total.scanned`, `by_entity.{kind}.scanned`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkerStatsReport {
    /// Flat counter map (stats-struct field name -> cumulative count)
    pub counters: BTreeMap<String, u64>,
    /// When the worker took this reading
    pub reported_at: DateTime<Utc>,
}

impl WorkerStatsReport {
    /// The max number of counters one report may carry
    pub const MAX_COUNTERS: usize = 256;
    /// The max length of a single counter name in bytes
    pub const MAX_COUNTER_NAME_LEN: usize = 128;
}

/// Point-in-time stats for one kind of event
///
/// Counters are cumulative sums across this kinds live workers so they inherit
/// the [`WorkerStatsReport`] caveats: a worker restart drops its contribution
/// back to zero and event replay after a crash can inflate them. Consumers
/// derive rates between readings and clamp negative deltas to zero.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct KindStats {
    /// The number of events waiting in the pending queue
    pub queued: u64,
    /// The number of events currently in flight across all owners and the
    /// legacy un-owned in flight queue
    pub in_flight: u64,
    /// Cumulative counters summed across this kinds live workers
    pub counters: BTreeMap<String, u64>,
}

/// One persisted row of the scan stats time series
///
/// Snapshots persist only anonymous aggregates: numeric counters and gauges
/// keyed by [`EventType`] plus a live worker count — never the per worker
/// breakdown, worker ids, usernames, hunt names or ids, or group names. The
/// rows are still admin-only telemetry though, since what the numbers reveal
/// (system-wide activity levels) is admin-only data today.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ScanStatsSnapshot {
    /// The quantized instant this row covers (the clustering key)
    pub snapshot_at: DateTime<Utc>,
    /// How many live workers were registered when this snapshot was taken
    pub workers: u64,
    /// The per kind depths and summed counters
    pub kinds: HashMap<EventType, KindStats>,
}

/// The live scan stats aggregate (`GET /api/events/stats/`)
///
/// Assembled at read time by intersecting the worker registry with the TTL'd
/// per worker counter reports plus the queue depths; nothing here is ever
/// persisted. Every event kind is always present so consumers see a stable
/// key set even when no worker has published counters for a kind yet.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EventStats {
    /// When this aggregate was assembled
    pub taken_at: DateTime<Utc>,
    /// The number of event handler workers with a live heartbeat
    pub workers: u64,
    /// The per kind depths and summed counters
    pub kinds: HashMap<EventType, KindStats>,
    /// The per worker reports behind the per kind sums
    ///
    /// A wedged-but-still-beating worker shows up here with frozen counters,
    /// which is exactly the case this breakdown exists to make debuggable.
    pub per_worker: HashMap<String, HashMap<EventType, WorkerStatsReport>>,
}

/// The query params for reading the scan stats history
///
/// The range is half open (`start <= snapshot_at < end`) so pages tile with
/// zero overlap. Responses are downsampled to the newest row per `step_secs`
/// stride, capped around 5,000 points, and returned oldest first.
#[derive(Deserialize, Serialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct StatsHistoryParams {
    /// The oldest instant to return rows from (inclusive)
    pub start: DateTime<Utc>,
    /// The newest instant to return rows up to (exclusive)
    pub end: DateTime<Utc>,
    /// The stride to downsample rows to in seconds
    ///
    /// Defaults to roughly 1,000 points across the range; floored at the
    /// snapshot cadence and clamped so a response never exceeds the point cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_secs: Option<u64>,
}

/// The query params for scoping an event queue operation to a single worker
///
/// Clears and resets with a worker set act only on that workers own in flight
/// keys; without one they keep acting on the legacy shared in flight keys.
#[derive(Deserialize, Serialize, Debug, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct EventWorkerOpts {
    /// The worker whose in flight events this operation applies to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

impl EventWorkerOpts {
    /// Set the worker this operation applies to
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker id this operation applies to
    #[must_use]
    pub fn worker<T: Into<String>>(mut self, worker: T) -> Self {
        // set the worker this operation applies to
        self.worker = Some(worker.into());
        self
    }
}

/// The query params for getting the event cache status
#[derive(Deserialize, Serialize, Debug, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct EventCacheStatusOpts {
    /// Whether to reset any cache statuses
    #[serde(default)]
    pub reset: bool,
}

impl EventCacheStatusOpts {
    /// Set the rest flag to true
    #[must_use]
    pub fn reset(mut self) -> Self {
        // set our reset flag to true
        self.reset = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tag map with a single group visible under one key/value
    ///
    /// # Arguments
    ///
    /// * `key` - The tag key to set
    /// * `value` - The tag value to set under the key
    /// * `group` - The group the value is visible in
    fn tag(
        key: &str,
        value: &str,
        group: &str,
    ) -> HashMap<String, HashMap<String, HashSet<String>>> {
        // build the innermost set of groups the value is visible in
        let mut groups = HashSet::new();
        groups.insert(group.to_owned());
        // map the value to its visible groups
        let mut values = HashMap::new();
        values.insert(value.to_owned(), groups);
        // map the key to its values
        let mut tags = HashMap::new();
        tags.insert(key.to_owned(), values);
        tags
    }

    /// Build a model refresh event of every target and every reason
    fn refresh_events() -> Vec<Event> {
        // one of each target
        let targets = vec![
            ModelRefreshTarget::GroupEndpoint {
                groups: vec!["minerva".to_owned(), "mars".to_owned()],
                endpoint: Uuid::new_v4(),
            },
            ModelRefreshTarget::UserEndpoint {
                user: "europa".to_owned(),
                endpoint: Uuid::new_v4(),
            },
            ModelRefreshTarget::User {
                user: "europa".to_owned(),
            },
        ];
        // one of each reason
        let reasons = vec![
            ModelRefreshReason::Manual,
            ModelRefreshReason::EndpointChanged,
            ModelRefreshReason::CredentialChanged {
                credential: crate::models::ModelConfigRef::Group(Uuid::new_v4()),
            },
            ModelRefreshReason::CredentialChanged {
                credential: crate::models::ModelConfigRef::User(Uuid::new_v4()),
            },
        ];
        // pair every target with every reason
        let mut events = Vec::new();
        for target in &targets {
            for reason in &reasons {
                events.push(Event {
                    id: Uuid::new_v4(),
                    timestamp: Utc::now(),
                    parent: None,
                    user: "europa".to_owned(),
                    data: EventData::ModelRefresh(ModelRefreshEvent {
                        target: target.clone(),
                        reason: reason.clone(),
                    }),
                    depth: 0,
                });
            }
        }
        events
    }

    /// Every model refresh target and reason survives a serde round trip and maps to the model
    /// refresh event type
    #[test]
    fn model_refresh_round_trip() {
        for event in refresh_events() {
            // serialize the event the way it's stored in its queue and read it back
            let serialized = serde_json::to_string(&event).expect("serialize event");
            let read: Event = serde_json::from_str(&serialized).expect("deserialize event");
            // the target and reason are unchanged
            let (EventData::ModelRefresh(sent), EventData::ModelRefresh(got)) =
                (&event.data, &read.data)
            else {
                panic!("expected a model refresh event, got {:?}", read.data);
            };
            assert_eq!(sent, got);
            // the event goes to the model refresh queue
            assert_eq!(EventType::from(&read.data), EventType::ModelRefresh);
        }
    }

    /// The model refresh event type converts to and from its name
    #[test]
    fn model_refresh_type_names() {
        // the type's name is its queue's name
        assert_eq!(EventType::ModelRefresh.as_str(), "ModelRefresh");
        assert_eq!(EventType::ModelRefresh.to_string(), "ModelRefresh");
        // the name parses back to the type
        assert_eq!(
            EventType::from_str("ModelRefresh").expect("parse event type"),
            EventType::ModelRefresh
        );
    }

    /// Asserts that no event of a kind can ever meet a tag trigger or a new sample trigger
    ///
    /// # Arguments
    ///
    /// * `events` - The events of one kind, covering every variant of its data
    fn assert_never_triggers(events: Vec<Event>) {
        // a tag trigger and a new sample trigger
        let tag_trigger = EventTrigger::Tag {
            tag_types: vec![TagType::Files],
            required: HashMap::from([("family".to_owned(), vec!["parser".to_owned()])]),
            not: HashMap::new(),
        };
        let triggers = [tag_trigger, EventTrigger::NewSample];
        // no event of this kind can meet either trigger
        for event in events {
            for trigger in &triggers {
                assert!(
                    matches!(event.could_trigger(trigger), TriggerPotential::CanNot),
                    "{:?} must not be able to trigger {trigger:?}",
                    event.data
                );
            }
        }
    }

    /// Model refresh events can never trigger a reaction, whatever the trigger
    #[test]
    fn model_refresh_never_triggers() {
        // no refresh of any target can meet a trigger
        assert_never_triggers(refresh_events());
    }

    /// Build a workflow graph event of every kind of change, each in two groups
    fn graph_events() -> Vec<Event> {
        // the groups every change is in
        let groups = vec!["minerva".to_owned(), "mars".to_owned()];
        // a changed pair of user tags
        let mut tags = BTreeMap::new();
        tags.insert("family".to_owned(), BTreeSet::from(["parser".to_owned()]));
        // one change of each kind
        let changes = vec![
            WorkflowGraphChange::WorkflowUpdated {
                workflow: Uuid::new_v4(),
            },
            WorkflowGraphChange::ToolUpdated {
                tool: Uuid::new_v4(),
                tool_instance: Uuid::new_v4(),
            },
            WorkflowGraphChange::AgentUpdated {
                agent: Uuid::new_v4(),
            },
            WorkflowGraphChange::EdgeChanged {
                source: Uuid::new_v4(),
                target: Uuid::new_v4(),
                kind: crate::models::AssociationKind::Calls,
            },
            WorkflowGraphChange::ManualInstance {
                workflow: Uuid::new_v4(),
            },
            WorkflowGraphChange::ToolSetChanged {
                tool_set: Uuid::new_v4(),
            },
            WorkflowGraphChange::ToolSetUpdated {
                tool_set: Uuid::new_v4(),
                tool_set_instance: Uuid::new_v4(),
            },
            WorkflowGraphChange::ManualToolSetInstance {
                tool_set: Uuid::new_v4(),
            },
            WorkflowGraphChange::ToolTagsChanged {
                tool: Uuid::new_v4(),
                tags,
            },
        ];
        // wrap each change in an event
        changes
            .into_iter()
            .map(|change| Event {
                id: Uuid::new_v4(),
                timestamp: Utc::now(),
                parent: None,
                user: "europa".to_owned(),
                data: EventData::WorkflowGraph(WorkflowGraphEvent {
                    groups: groups.clone(),
                    change,
                }),
                depth: 0,
            })
            .collect()
    }

    /// Every kind of workflow graph change survives a serde round trip with its groups, and maps
    /// to the workflow graph event type
    #[test]
    fn workflow_graph_round_trip() {
        for event in graph_events() {
            // serialize the event the way it's stored in its queue and read it back
            let serialized = serde_json::to_string(&event).expect("serialize event");
            let read: Event = serde_json::from_str(&serialized).expect("deserialize event");
            // the change and its groups are unchanged
            let (EventData::WorkflowGraph(sent), EventData::WorkflowGraph(got)) =
                (&event.data, &read.data)
            else {
                panic!("expected a workflow graph event, got {:?}", read.data);
            };
            assert_eq!(sent, got);
            assert_eq!(got.groups, vec!["minerva".to_owned(), "mars".to_owned()]);
            // the event goes to the workflow graph queue
            assert_eq!(EventType::from(&read.data), EventType::WorkflowGraph);
        }
    }

    /// The workflow graph event type converts to and from its name
    #[test]
    fn workflow_graph_type_names() {
        // the type's name is its queue's name
        assert_eq!(EventType::WorkflowGraph.as_str(), "WorkflowGraph");
        assert_eq!(EventType::WorkflowGraph.to_string(), "WorkflowGraph");
        // the name parses back to the type
        assert_eq!(
            EventType::from_str("WorkflowGraph").expect("parse event type"),
            EventType::WorkflowGraph
        );
    }

    /// Workflow graph events can never trigger a reaction, whatever the trigger
    #[test]
    fn workflow_graph_never_triggers() {
        // no change of any kind can meet a trigger
        assert_never_triggers(graph_events());
    }

    /// `check_all_tag_trigger`'s required-value check requires every required value to be
    /// present, so it is not vacuously satisfied when a required KEY exists but none of its
    /// required VALUES are present. A trigger requiring `family=[emotet]` does NOT fire on a
    /// sample whose only family tag is `zeus`.
    #[test]
    fn required_key_with_wrong_value_does_not_fire() {
        // the sample carries family=zeus, visible in group "corn"
        let tags = tag("family", "zeus", "corn");
        // the trigger requires family=emotet, which the sample does not have
        let mut required = HashMap::new();
        required.insert("family".to_owned(), vec!["emotet".to_owned()]);
        // nothing is forbidden by this trigger
        let not = HashMap::new();
        // the group the requesting user can see
        let visible = vec!["corn".to_owned()];
        // the required value is absent, so this trigger must not fire
        assert!(
            !Event::check_all_tag_trigger(&visible, &tags, &required, &not),
            "a trigger requiring family=emotet must not fire on a sample tagged only family=zeus"
        );
    }

    /// Build a yara scan event for trigger tests
    ///
    /// # Arguments
    ///
    /// * `depth` - The trigger depth to set on this event
    fn yara_event(depth: u8) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            parent: None,
            user: "mcarson".to_owned(),
            data: EventData::YaraScannableSample(YaraSampleScanEvent {
                sha256: "a".repeat(64),
                groups: vec!["corn".to_owned()],
                trigger: YaraScanTrigger::NewSubmission,
            }),
            depth,
        }
    }

    /// Yara scan events must never fire reaction triggers of any kind
    #[test]
    fn yara_event_could_trigger_cannot() {
        // build a yara scan event
        let event = yara_event(0);
        // a yara scan event can never confirm a new sample trigger
        assert!(matches!(
            event.could_trigger(&EventTrigger::NewSample),
            TriggerPotential::CanNot
        ));
        // a yara scan event can never confirm a tag trigger either
        let tag_trigger = EventTrigger::Tag {
            tag_types: vec![TagType::Files],
            required: HashMap::default(),
            not: HashMap::default(),
        };
        assert!(matches!(
            event.could_trigger(&tag_trigger),
            TriggerPotential::CanNot
        ));
    }

    /// Yara scan event data must route to the yara event queue
    #[test]
    fn event_type_from_yara_data() {
        // build a yara scan event
        let event = yara_event(3);
        // make sure this events data maps to the yara scannable sample queue
        assert_eq!(EventType::from(&event.data), EventType::YaraScannableSample);
        // make sure the originating trigger depth rides on the event itself
        assert_eq!(event.depth, 3);
    }

    /// Build a yara hunt chunk event for hunt chunk tests
    ///
    /// # Arguments
    ///
    /// * `lane` - The lane index to set on this chunk event
    fn hunt_chunk_event(lane: u32) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            parent: None,
            user: "mcarson".to_owned(),
            data: EventData::YaraHuntChunk(HuntChunkEvent {
                hunt: Uuid::new_v4(),
                lane,
                seq: 0,
            }),
            depth: 0,
        }
    }

    /// Yara hunt chunk event data must route to the yara hunt chunk queue
    #[test]
    fn event_type_from_hunt_chunk_data() {
        // build a yara hunt chunk event
        let event = hunt_chunk_event(2);
        // make sure this events data maps to the yara hunt chunk queue
        assert_eq!(EventType::from(&event.data), EventType::YaraHuntChunk);
    }

    /// Hunt chunk events must never fire reaction triggers
    #[test]
    fn hunt_chunk_could_trigger_cannot() {
        // build a yara hunt chunk event
        let event = hunt_chunk_event(0);
        // a hunt chunk event can never confirm a new sample trigger
        assert!(matches!(
            event.could_trigger(&EventTrigger::NewSample),
            TriggerPotential::CanNot
        ));
        // a hunt chunk event can never confirm a tag trigger either
        let tag_trigger = EventTrigger::Tag {
            tag_types: vec![TagType::Files],
            required: HashMap::default(),
            not: HashMap::default(),
        };
        assert!(matches!(
            event.could_trigger(&tag_trigger),
            TriggerPotential::CanNot
        ));
    }

    /// Hunt chunk event payloads must round trip through serde
    #[test]
    fn hunt_chunk_serde_round_trip() {
        // build a hunt chunk payload with distinctive values
        let chunk = HuntChunkEvent {
            hunt: Uuid::new_v4(),
            lane: 3,
            seq: 42,
        };
        // serialize and deserialize the payload
        let raw = serde_json::to_string(&chunk).expect("failed to serialize hunt chunk");
        let round: HuntChunkEvent =
            serde_json::from_str(&raw).expect("failed to deserialize hunt chunk");
        // the payload must survive the round trip unchanged
        assert_eq!(chunk, round);
    }

    /// Missing cache status fields must deserialize to false
    ///
    /// This guards rolling upgrades where an older API only wrote some of the
    /// per component staleness flags or none of the generation counters.
    #[test]
    fn cache_status_serde_defaults() {
        // deserialize a cache status that only carries the legacy triggers flag
        let status: EventCacheStatus = serde_json::from_str(r#"{"triggers":true}"#)
            .expect("failed to deserialize a partial cache status");
        // the triggers flag must be set
        assert!(status.triggers);
        // every missing per component flag must default to false
        assert!(!status.users);
        assert!(!status.sigma_rules);
        assert!(!status.yara_rules);
        // every missing generation counter must default to 0
        assert_eq!(status.triggers_gen, 0);
        assert_eq!(status.users_gen, 0);
        assert_eq!(status.sigma_rules_gen, 0);
        assert_eq!(status.yara_rules_gen, 0);
        // a cache status with generation counters must round trip them
        let status: EventCacheStatus =
            serde_json::from_str(r#"{"yara_rules":true,"yara_rules_gen":7,"users_gen":2}"#)
                .expect("failed to deserialize a cache status with generations");
        // the set generation counters must be preserved
        assert_eq!(status.yara_rules_gen, 7);
        assert_eq!(status.users_gen, 2);
        // the unset generation counters must still default to 0
        assert_eq!(status.triggers_gen, 0);
        assert_eq!(status.sigma_rules_gen, 0);
    }

    /// Worker ids that only use key safe characters must be accepted
    #[test]
    fn worker_id_validation_accepts_safe_ids() {
        // every id below only uses key safe characters within the length bound
        for worker in [
            "event-handler-7f3a9b2c",
            "pod.name_0",
            "A",
            &"a".repeat(128),
        ] {
            assert!(valid_worker_id(worker), "{worker} should be valid");
        }
    }

    /// Worker ids that could break out of a Redis key must be rejected
    #[test]
    fn worker_id_validation_rejects_unsafe_ids() {
        // every id below is either empty, too long, or carries unsafe characters
        for worker in [
            "",
            &"a".repeat(129) as &str,
            "with:colon",
            "with space",
            "with/slash",
            "with*glob",
            "newline\nin-id",
            "tab\tin-id",
            "unicode-𝕏",
        ] {
            assert!(!valid_worker_id(worker), "{worker:?} should be invalid");
        }
    }

    /// Reap summaries must round trip through serde unchanged
    #[test]
    fn reap_summary_serde_round_trip() {
        // build a reap summary with every field set to a distinct value
        let summary = ReapSummary {
            dead_workers: 1,
            reclaimed: 2,
            backstopped: 3,
            legacy_reclaimed: 4,
        };
        // serialize and deserialize this summary
        let serialized = serde_json::to_string(&summary).expect("failed to serialize summary");
        let round_tripped: ReapSummary =
            serde_json::from_str(&serialized).expect("failed to deserialize summary");
        // the round tripped summary must match the original
        assert_eq!(summary, round_tripped);
    }

    /// Event depths must round trip through serde unchanged
    #[test]
    fn event_depths_serde_round_trip() {
        // build the per kind depths for every event type
        let mut kinds = HashMap::default();
        kinds.insert(
            EventType::ReactionTrigger,
            KindDepth {
                queued: 5,
                in_flight: 1,
            },
        );
        kinds.insert(
            EventType::YaraScannableSample,
            KindDepth {
                queued: 0,
                in_flight: 9,
            },
        );
        // build the full depths object
        let depths = EventDepths { kinds, workers: 2 };
        // serialize and deserialize these depths
        let serialized = serde_json::to_string(&depths).expect("failed to serialize depths");
        let round_tripped: EventDepths =
            serde_json::from_str(&serialized).expect("failed to deserialize depths");
        // the round tripped depths must match the original
        assert_eq!(depths, round_tripped);
    }

    /// When the required value IS present and visible, the trigger fires. This pins the positive
    /// case so the required-value check can't regress into never firing.
    #[test]
    fn required_key_with_matching_value_fires() {
        // the sample carries family=emotet, visible in group "corn"
        let tags = tag("family", "emotet", "corn");
        // the trigger requires family=emotet, which the sample has
        let mut required = HashMap::new();
        required.insert("family".to_owned(), vec!["emotet".to_owned()]);
        // nothing is forbidden by this trigger
        let not = HashMap::new();
        // the group the requesting user can see
        let visible = vec!["corn".to_owned()];
        // the required value is present and visible, so this trigger must fire
        assert!(
            Event::check_all_tag_trigger(&visible, &tags, &required, &not),
            "a trigger requiring family=emotet must fire on a sample tagged family=emotet"
        );
    }

    /// Recursively collect every object key a json value carries
    ///
    /// # Arguments
    ///
    /// * `value` - The json value to walk
    /// * `keys` - The set collecting every object key seen
    fn collect_keys(value: &serde_json::Value, keys: &mut std::collections::BTreeSet<String>) {
        match value {
            // collect this objects keys then recurse into its values
            serde_json::Value::Object(map) => {
                for (key, nested) in map {
                    keys.insert(key.clone());
                    collect_keys(nested, keys);
                }
            }
            // recurse into array items
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_keys(item, keys);
                }
            }
            // scalars carry no keys
            _ => (),
        }
    }

    /// Pin that a persisted snapshot serializes with no identifier fields
    ///
    /// `scan_stats` rows are the exact serialization of [`ScanStatsSnapshot`],
    /// so pinning the recursive key set here pins what the time series can
    /// ever leak: numeric counters and gauges keyed by [`EventType`] plus a
    /// worker count — never the per worker breakdown, worker ids, usernames,
    /// hunt names or ids, or group names. Growing this struct means
    /// consciously editing this pin.
    #[test]
    fn scan_stats_snapshot_pins_no_identifiers() {
        // build a populated snapshot like the writer persists
        let kind_stats = KindStats {
            queued: 3,
            in_flight: 1,
            counters: BTreeMap::from([("scanned".to_owned(), 9), ("total.scanned".to_owned(), 2)]),
        };
        let snapshot = ScanStatsSnapshot {
            snapshot_at: Utc::now(),
            workers: 2,
            kinds: HashMap::from([(EventType::YaraScannableSample, kind_stats)]),
        };
        // serialize exactly like the scan_stats writer does
        let serialized = serde_json::to_value(&snapshot).expect("snapshot must serialize");
        // collect every object key the persisted shape carries
        let mut keys = std::collections::BTreeSet::new();
        collect_keys(&serialized, &mut keys);
        // the full key set is pinned: struct fields, the kind, and counter names
        let expected: std::collections::BTreeSet<String> = [
            "snapshot_at",
            "workers",
            "kinds",
            "YaraScannableSample",
            "queued",
            "in_flight",
            "counters",
            "scanned",
            "total.scanned",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(
            keys, expected,
            "persisted snapshots grew a field; make sure it carries no identifiers"
        );
        // belt and braces: the fields a snapshot must never carry stay absent
        for forbidden in [
            "per_worker",
            "worker",
            "submitter",
            "username",
            "name",
            "groups",
        ] {
            assert!(
                !keys.contains(forbidden),
                "persisted snapshots must never carry {forbidden}"
            );
        }
    }
}
