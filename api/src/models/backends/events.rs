//! The backend functions for events
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::prelude::*;
use tracing::instrument;
use uuid::Uuid;

use super::db::{self};
use crate::models::backends::TagSupport;
use crate::models::{
    EventCacheStatus, EventPeekOpts, EventPopOpts, ModelRefreshEvent, ModelRefreshReason,
    ModelRefreshTarget, OutputKey, SigmaRuleAppliesTo, SigmaScannableResultsEvent,
    WorkflowGraphChange, WorkflowGraphEvent,
};
use crate::{
    is_admin,
    models::{Event, EventData, EventType, TagRequest},
};
use crate::{
    models::User,
    utils::{ApiError, Shared},
};

impl Event {
    /// Create a new tag event
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating new tags
    /// * `key` - The key to save this tag too in the db
    /// * `req` - The tag request we are making events for
    #[must_use]
    pub fn new_tag<T: TagSupport>(user: &User, key: String, req: TagRequest<T>) -> Self {
        // generate a random event id
        let id = Uuid::new_v4();
        // get the current timestamp
        let timestamp = Utc::now();
        // build our event data
        let data = EventData::NewTags {
            tag_type: T::tag_kind(),
            item: key,
            groups: req.groups,
            tags: req.tags,
        };
        // build our tag event
        Event {
            id,
            timestamp,
            parent: None,
            user: user.username.clone(),
            data,
            depth: req.trigger_depth,
        }
    }

    /// Create a new sample event
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating new tags
    /// * `groups` - The groups this new sample was added too
    /// * `sample` - The sha256 of the new sample
    /// * `depth` - This new samples trigger's depth
    #[must_use]
    pub fn new_sample(user: &User, groups: Vec<String>, sample: String, depth: u8) -> Self {
        // generate a random event id
        let id = Uuid::new_v4();
        // get the current timestamp
        let timestamp = Utc::now();
        // build our event data
        let data = EventData::NewSample { groups, sample };
        // build our tag event
        Event {
            id,
            timestamp,
            parent: None,
            user: user.username.clone(),
            data,
            depth,
        }
    }

    /// Create a workflow graph event for a change that may require new workflow or toolset
    /// instances
    ///
    /// # Arguments
    ///
    /// * `user` - The user that made the change
    /// * `groups` - Every group of the changed entity
    /// * `change` - What changed
    #[must_use]
    pub fn workflow_graph(user: &User, groups: &[String], change: WorkflowGraphChange) -> Self {
        // list each group once, in a stable order
        let mut groups = groups.to_vec();
        groups.sort_unstable();
        groups.dedup();
        // build our event data
        let data = EventData::WorkflowGraph(WorkflowGraphEvent { groups, change });
        // build our workflow graph event
        Event {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            parent: None,
            user: user.username.clone(),
            data,
            // depth has no meaning for workflow graph changes
            depth: 0,
        }
    }

    /// Queue the workflow graph events for several changes in one round trip
    ///
    /// # Arguments
    ///
    /// * `user` - The user that made the changes
    /// * `changes` - Each change with every group of its changed entity
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "Event::queue_workflow_graphs",
        skip_all,
        fields(changes = changes.len()),
        err(Debug)
    )]
    pub(crate) async fn queue_workflow_graphs(
        user: &User,
        changes: Vec<(Vec<String>, WorkflowGraphChange)>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // build every event and add them all to the workflow graph queue together
        let events = changes
            .into_iter()
            .map(|(groups, change)| Self::workflow_graph(user, &groups, change))
            .collect::<Vec<Event>>();
        db::events::create_many(&events, shared).await
    }

    /// Queue a workflow graph event for a change that may require new workflow or toolset
    /// instances
    ///
    /// # Arguments
    ///
    /// * `user` - The user that made the change
    /// * `groups` - Every group of the changed entity
    /// * `change` - What changed
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "Event::queue_workflow_graph",
        skip(user, groups, shared),
        err(Debug)
    )]
    pub(crate) async fn queue_workflow_graph(
        user: &User,
        groups: &[String],
        change: WorkflowGraphChange,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // build the event and add it to the workflow graph queue
        db::events::create(&Self::workflow_graph(user, groups, change), shared).await
    }

    /// Create a model refresh event
    ///
    /// # Arguments
    ///
    /// * `user` - The user that caused the refresh
    /// * `target` - What to list and check
    /// * `reason` - Why the refresh was requested
    #[must_use]
    pub fn model_refresh(
        user: &User,
        mut target: ModelRefreshTarget,
        reason: ModelRefreshReason,
    ) -> Self {
        // list an endpoint's groups once each, in a stable order
        if let ModelRefreshTarget::GroupEndpoint { groups, .. } = &mut target {
            groups.sort_unstable();
            groups.dedup();
        }
        // build our model refresh event
        Event {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            parent: None,
            user: user.username.clone(),
            data: EventData::ModelRefresh(ModelRefreshEvent { target, reason }),
            // depth has no meaning for model refreshes
            depth: 0,
        }
    }

    /// Queue a model refresh event
    ///
    /// # Arguments
    ///
    /// * `user` - The user that caused the refresh
    /// * `target` - What to list and check
    /// * `reason` - Why the refresh was requested
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::queue_model_refresh", skip(user, shared), err(Debug))]
    pub(crate) async fn queue_model_refresh(
        user: &User,
        target: ModelRefreshTarget,
        reason: ModelRefreshReason,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // build the event and add it to the model refresh queue
        db::events::create(&Self::model_refresh(user, target, reason), shared).await
    }

    /// Create a new scannable by sigma event
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating
    #[must_use]
    pub fn sigma_scannable_results(
        user: &User,
        key: OutputKey,
        groups: Vec<String>,
        applies_to: Vec<SigmaRuleAppliesTo>,
        tool: impl Into<String>,
    ) -> Self {
        // generate a random event id
        let id = Uuid::new_v4();
        // get the current timestamp
        let timestamp = Utc::now();
        // build our inner event data type
        let inner = SigmaScannableResultsEvent {
            key,
            groups,
            applies_to,
            tool: tool.into(),
        };
        // build our event data
        let data = EventData::SigmaScannableResults(inner);
        // build our tag event
        Event {
            id,
            timestamp,
            parent: None,
            user: user.username.clone(),
            data,
            // depth has no meaning for sigma scannable results
            depth: 0,
        }
    }

    /// Pop some events from a specific queue
    #[instrument(name = "Event::pop", skip(user, shared), err(Debug))]
    pub async fn pop(
        user: &User,
        kind: EventType,
        count: usize,
        shared: &Shared,
    ) -> Result<Vec<Event>, ApiError> {
        // only admins can pop events
        is_admin!(user);
        // try to pop some events from redis
        db::events::pop(kind, count, shared).await
    }

    /// Read the queued events of a specific queue without moving them in flight
    ///
    /// Only admins can peek at events.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is peeking at events
    /// * `kind` - The kind of events to peek at
    /// * `limit` - The number of events to return at most
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::peek", skip(user, shared), err(Debug))]
    pub async fn peek(
        user: &User,
        kind: EventType,
        limit: usize,
        shared: &Shared,
    ) -> Result<Vec<Event>, ApiError> {
        // only admins can peek at events
        is_admin!(user);
        // read the queued events from redis
        db::events::peek(kind, limit, shared).await
    }

    /// Clear some events from a specific queue
    #[instrument(name = "Event::clear", skip(user, ids, shared), fields(count = ids.len()), err(Debug))]
    pub async fn clear(
        user: &User,
        kind: EventType,
        ids: &Vec<Uuid>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins can clear events
        is_admin!(user);
        // try to clear some events
        db::events::clear(kind, ids, shared).await
    }

    /// Reset any in flight events
    #[instrument(name = "Event::reset_all", skip(user, shared), err(Debug))]
    pub async fn reset_all(user: &User, kind: EventType, shared: &Shared) -> Result<(), ApiError> {
        // only admins can reset all events
        is_admin!(user);
        // try to reset all in flight events
        db::events::reset_all(kind, shared).await
    }

    /// Get our current event cache status
    ///
    /// Only admins can get the event cache status.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting the event cache status
    /// * `clear` - Whether to clear the event cache statuses or not
    /// * `shared` - Shared Thorium objectds
    #[instrument(name = "Event::get_cache_status", skip(user, shared), err(Debug))]
    pub async fn get_cache_status(
        user: &User,
        clear: bool,
        shared: &Shared,
    ) -> Result<EventCacheStatus, ApiError> {
        // only admins can get the event cache status
        is_admin!(user);
        // get our event cache status
        db::events::get_cache_status(clear, shared).await
    }
}

impl<S> FromRequestParts<S> for EventPopOpts
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

impl<S> FromRequestParts<S> for EventPeekOpts
where
    S: Send + Sync,
{
    /// Bad peek options are rejected with an [`ApiError`]
    type Rejection = ApiError;

    /// Extract the event peek options from a request's query string
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract from
    /// * `_state` - The shared state (unused)
    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        // deserialize our query string if one was sent, or use the defaults
        let opts = match parts.uri.query() {
            Some(query) => serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)
                .map_err(ApiError::from),
            None => Ok(Self::default()),
        };
        // these options are ready without waiting on anything
        std::future::ready(opts)
    }
}
