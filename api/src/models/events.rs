//! The events in Thorium for triggers and other things to act on

use chrono::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use uuid::Uuid;

use crate::models::{AssociationKind, ModelConfigRef, OutputKey, SigmaRuleAppliesTo};

use super::TagType;

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
)]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreAsStr))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum EventType {
    /// A trigger that may cause a reaction to be spawned
    ReactionTrigger,
    /// A result with new entities was added and can be scanned by a sigma rule
    SigmaScannableResults,
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
            EventData::WorkflowGraph(_) => EventType::WorkflowGraph,
            EventData::ModelRefresh(_) => EventType::ModelRefresh,
        }
    }
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
        visible: &Vec<String>,
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
        trigger_types: &Vec<TagType>,
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
pub struct EventPopOpts {
    /// The max number of events to pop and return
    #[serde(default = "default_event_pop_limit")]
    pub limit: usize,
}

impl Default for EventPopOpts {
    /// Create a default `EventPopOpts`
    fn default() -> Self {
        EventPopOpts {
            limit: default_event_pop_limit(),
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
}

/// Default the event peek limit to 1000
fn default_event_peek_limit() -> usize {
    1000
}

/// The params for peeking at queued events
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EventCacheStatus {
    /// Whether we need to refresh our triggers cache
    pub triggers: bool,
}

/// The query params for getting the event cache status
#[derive(Deserialize, Serialize, Debug, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
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
}
