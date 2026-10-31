//! The backend functions for events
use aws_sdk_s3::primitives::ByteStream;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::prelude::*;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::db::{self};
use crate::models::backends::TagSupport;
use crate::models::{
    EventCacheStatus, EventDepths, EventPeekOpts, EventPopOpts, EventStats, HuntChunkEvent,
    HuntEngine, ModelRefreshEvent, ModelRefreshReason, ModelRefreshTarget, OutputKey, ReapSummary,
    ScanStatsSnapshot, SigmaRuleAppliesTo, SigmaScannableResultsEvent, StatsHistoryParams,
    WorkerStatsReport, WorkflowGraphChange, WorkflowGraphEvent, YaraSampleScanEvent,
    YaraScanTrigger,
};
use crate::{
    bad, is_admin,
    models::{Event, EventData, EventType, TagRequest},
};
use crate::{
    models::User,
    utils::{ApiError, Shared},
};

/// Make sure a yara ruleset cache path segment is safe to embed in an s3 key
///
/// Segments share the worker id charset (`^[A-Za-z0-9._-]{1,128}$`) since both
/// embed in flat key names, and additionally ban `..` to match the traversal
/// guards on every other s3 path in Thorium.
///
/// # Arguments
///
/// * `name` - The name of the path segment being validated
/// * `segment` - The path segment to validate
fn validate_ruleset_segment(name: &str, segment: &str) -> Result<(), ApiError> {
    // reject any segment with characters that could break out of an s3 key
    if !crate::models::valid_worker_id(segment) {
        return bad!(format!(
            "{name} must match ^[A-Za-z0-9._-]{{1,128}}$ but got '{segment}'"
        ));
    }
    // ban any segments that might contain traversal attacks
    if segment.contains("..") {
        return bad!(format!("{name} cannot contain '..'"));
    }
    Ok(())
}

/// Build the s3 key for a cached serialized yara ruleset
///
/// # Arguments
///
/// * `engine_version` - The yara engine version this ruleset was serialized with
/// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
fn yara_ruleset_key(engine_version: &str, ruleset_hash: &str) -> Result<String, ApiError> {
    // make sure both path segments are safe to embed in an s3 key
    validate_ruleset_segment("engine_version", engine_version)?;
    validate_ruleset_segment("ruleset_hash", ruleset_hash)?;
    // build the content addressed key for this cached ruleset
    Ok(format!("{engine_version}/{ruleset_hash}"))
}

/// Best-effort delete any cached rulesets for an engine version except one
///
/// A ruleset cache entry is content addressed so once a new hash is uploaded
/// its same-engine-version siblings will never be looked up again. Failures
/// here only leak storage until the bucket lifecycle reaps them, so they are
/// logged and swallowed instead of failing the upload that already succeeded.
///
/// # Arguments
///
/// * `engine_version` - The engine version whose siblings should be pruned
/// * `keep` - The s3 key of the freshly uploaded ruleset to keep
/// * `shared` - Shared Thorium objects
#[instrument(name = "events::prune_yara_ruleset_siblings", skip(shared))]
async fn prune_yara_ruleset_siblings(engine_version: &str, keep: &str, shared: &Shared) {
    // list the cached rulesets under this engine version
    let keys = match shared
        .s3
        .yara_rulesets
        .list_truncated(&format!("{engine_version}/"))
        .await
    {
        Ok(keys) => keys,
        // this prune is best-effort so just log any listing failure
        Err(error) => {
            event!(
                Level::WARN,
                msg = "Failed to list yara ruleset siblings",
                error = error.to_string()
            );
            return;
        }
    };
    // delete every sibling except the ruleset we just uploaded
    for key in keys.iter().filter(|key| key.as_str() != keep) {
        // this prune is best-effort so just log any delete failure
        if let Err(error) = shared.s3.yara_rulesets.delete(key).await {
            event!(
                Level::WARN,
                msg = "Failed to delete yara ruleset sibling",
                key,
                error = error.to_string()
            );
        }
    }
}

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
    /// * `user` - The user that is creating this event
    /// * `key` - The key to the output that can be scanned
    /// * `groups` - The groups these results were added too
    /// * `applies_to` - The kinds of rules that can be applied to these results
    /// * `tool` - The tool that created these results
    /// * `depth` - The trigger depth of the job that produced these results
    #[must_use]
    pub fn sigma_scannable_results(
        user: &User,
        key: OutputKey,
        groups: Vec<String>,
        applies_to: Vec<SigmaRuleAppliesTo>,
        tool: impl Into<String>,
        depth: u8,
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
            depth,
        }
    }

    /// Create a new yara scannable sample event
    ///
    /// Unlike sigma scannable results events the originating trigger depth is
    /// carried on this event so hit tags written by the yara scanner can stamp
    /// `depth + 1` and stay bounded by the max depth loop breaker.
    ///
    /// # Arguments
    ///
    /// * `user` - The user whose write made this sample newly visible
    /// * `groups` - The groups whose visibility of this sample just changed
    /// * `sha256` - The sha256 of the sample to scan
    /// * `depth` - The originating trigger depth for this event
    /// * `trigger` - What caused this scan event to be emitted
    #[must_use]
    pub fn yara_scannable_sample(
        user: &User,
        groups: Vec<String>,
        sha256: String,
        depth: u8,
        trigger: YaraScanTrigger,
    ) -> Self {
        // generate a random event id
        let id = Uuid::new_v4();
        // get the current timestamp
        let timestamp = Utc::now();
        // build our inner event data type
        let inner = YaraSampleScanEvent {
            sha256,
            groups,
            trigger,
        };
        // build our event data
        let data = EventData::YaraScannableSample(inner);
        // build our yara scan event
        Event {
            id,
            timestamp,
            parent: None,
            user: user.username.clone(),
            data,
            depth,
        }
    }

    /// Create a new hunt chunk event on a hunt engine's queue
    ///
    /// Chunk events emit at depth 0: hunts start trigger chains exactly like
    /// the other user initiated emission sites, so any event emitting tags an
    /// engine writes for hunt findings land at depth 1 and stay bounded by the
    /// max depth loop breaker.
    ///
    /// # Arguments
    ///
    /// * `submitter` - The user that created the hunt this chunk advances
    /// * `engine` - The hunt engine whose queue this chunk lands on
    /// * `hunt` - The hunt entity id this chunk is for
    /// * `lane` - The lane index this chunk advances
    /// * `seq` - The checkpoint sequence this chunk will try to commit
    #[must_use]
    pub fn hunt_chunk<S: Into<String>>(
        submitter: S,
        engine: HuntEngine,
        hunt: Uuid,
        lane: u32,
        seq: u64,
    ) -> Self {
        // generate a random event id
        let id = Uuid::new_v4();
        // get the current timestamp
        let timestamp = Utc::now();
        // build our inner event data type
        let inner = HuntChunkEvent { hunt, lane, seq };
        // wrap our payload in the engine's event data variant so the chunk
        // lands on that engine's queue
        let data = engine.chunk_event_data(inner);
        // build our hunt chunk event
        Event {
            id,
            timestamp,
            parent: None,
            user: submitter.into(),
            data,
            // hunts start trigger chains so chunks emit at depth 0
            depth: 0,
        }
    }

    /// Pop some events from a specific queue
    ///
    /// Pops with a worker set claim the popped events into that workers own in
    /// flight keys; owner-less pops keep the legacy shared in flight keys.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is popping events
    /// * `kind` - The kind of events to pop
    /// * `count` - The max number of events to pop
    /// * `worker` - The worker that will own the popped events if one was set
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::pop", skip(user, shared), err(Debug))]
    pub async fn pop(
        user: &User,
        kind: EventType,
        count: usize,
        worker: Option<&str>,
        shared: &Shared,
    ) -> Result<Vec<Event>, ApiError> {
        // only admins can pop events
        is_admin!(user);
        // try to pop some events from redis
        db::events::pop(kind, count, worker, shared).await
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
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is clearing events
    /// * `kind` - The kind of events to clear
    /// * `ids` - The ids of the events to clear
    /// * `worker` - The worker whose in flight keys these events live in if one was set
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::clear", skip(user, ids, shared), fields(count = ids.len()), err(Debug))]
    pub async fn clear(
        user: &User,
        kind: EventType,
        ids: &[Uuid],
        worker: Option<&str>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins can clear events
        is_admin!(user);
        // try to clear some events from the right in flight keys
        db::events::clear(kind, ids, worker, shared).await
    }

    /// Reset any in flight events
    #[instrument(name = "Event::reset_all", skip(user, shared), err(Debug))]
    pub async fn reset_all(user: &User, kind: EventType, shared: &Shared) -> Result<(), ApiError> {
        // only admins can reset all events
        is_admin!(user);
        // try to reset all in flight events
        db::events::reset_all(kind, shared).await
    }

    /// Reset a single workers in flight events back to the pending queue
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is resetting events
    /// * `kind` - The kind of events to reset
    /// * `worker` - The worker whose in flight events should be reset
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::reset_worker", skip(user, shared), err(Debug))]
    pub async fn reset_worker(
        user: &User,
        kind: EventType,
        worker: &str,
        shared: &Shared,
    ) -> Result<u64, ApiError> {
        // only admins can reset events
        is_admin!(user);
        // try to reset this workers in flight events
        db::events::reset_worker(kind, worker, shared).await
    }

    /// Upsert a workers heartbeat in the event handler worker registry
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is heartbeating
    /// * `worker` - The worker that is heartbeating
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::heartbeat", skip(user, shared), err(Debug))]
    pub async fn heartbeat(user: &User, worker: &str, shared: &Shared) -> Result<(), ApiError> {
        // only admins can heartbeat event handler workers
        is_admin!(user);
        // upsert this workers heartbeat
        db::events::heartbeat(worker, shared).await
    }

    /// Upsert one workers cumulative counter report for one event kind
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is publishing worker stats
    /// * `kind` - The kind of events these counters cover
    /// * `worker` - The worker that took this reading
    /// * `report` - The counter report to upsert
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "Event::publish_worker_stats",
        skip(user, report, shared),
        err(Debug)
    )]
    pub async fn publish_worker_stats(
        user: &User,
        kind: EventType,
        worker: &str,
        report: &WorkerStatsReport,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins can publish worker stats
        is_admin!(user);
        // upsert this workers TTL'd counter report
        db::events::publish_worker_stats(kind, worker, report, shared).await
    }

    /// Drain all of a workers in flight events and remove it from the registry
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is draining this worker
    /// * `worker` - The worker to drain and deregister
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::drain_worker", skip(user, shared), err(Debug))]
    pub async fn drain_worker(user: &User, worker: &str, shared: &Shared) -> Result<u64, ApiError> {
        // only admins can drain event handler workers
        is_admin!(user);
        // drain this workers in flight events and deregister it
        db::events::drain_worker(worker, shared).await
    }

    /// Reap dead workers and over-aged in flight events back to the pending queues
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reaping dead workers
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::reap", skip_all, err(Debug))]
    pub async fn reap(user: &User, shared: &Shared) -> Result<ReapSummary, ApiError> {
        // only admins can reap event handler workers
        is_admin!(user);
        // reap dead workers and over-aged in flight events
        db::events::reap(shared).await
    }

    /// Get the queue depths for every kind of event plus the live worker count
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting queue depths
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::depths", skip_all, err(Debug))]
    pub async fn depths(user: &User, shared: &Shared) -> Result<EventDepths, ApiError> {
        // only admins can get event queue depths
        is_admin!(user);
        // get the current event queue depths
        db::events::depths(shared).await
    }

    /// Assemble the live scan stats aggregate for the admin status page
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting scan stats
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::stats", skip_all, err(Debug))]
    pub async fn stats(user: &User, shared: &Shared) -> Result<EventStats, ApiError> {
        // only admins can get scan stats
        is_admin!(user);
        // assemble the live scan stats aggregate
        db::events::stats(shared).await
    }

    /// Persist one quantized scan stats snapshot unless a peer already has
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is triggering a snapshot
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::snapshot_stats", skip_all, err(Debug))]
    pub async fn snapshot_stats(
        user: &User,
        shared: &Shared,
    ) -> Result<Option<ScanStatsSnapshot>, ApiError> {
        // only admins can trigger scan stats snapshots
        is_admin!(user);
        // persist this windows snapshot unless a peer already has
        db::events::snapshot_stats(shared).await
    }

    /// Read a ranged, downsampled slice of the scan stats history
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reading scan stats history
    /// * `params` - The half open range and stride to read
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::stats_history", skip_all, err(Debug))]
    pub async fn stats_history(
        user: &User,
        params: &StatsHistoryParams,
        shared: &Shared,
    ) -> Result<Vec<ScanStatsSnapshot>, ApiError> {
        // only admins can read scan stats history
        is_admin!(user);
        // read the requested slice of the scan stats history
        db::events::stats_history(params, shared).await
    }

    /// Get a serialized compiled yara ruleset from the cache bucket if one exists
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting this cached ruleset
    /// * `engine_version` - The yara engine version this ruleset was serialized with
    /// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Event::get_yara_ruleset", skip(user, shared), err(Debug))]
    pub async fn get_yara_ruleset(
        user: &User,
        engine_version: &str,
        ruleset_hash: &str,
        shared: &Shared,
    ) -> Result<Option<ByteStream>, ApiError> {
        // only admins can read the serialized ruleset cache
        is_admin!(user);
        // build the s3 key for this cached ruleset
        let key = yara_ruleset_key(engine_version, ruleset_hash)?;
        // try to download this cached ruleset
        shared.s3.yara_rulesets.download_opt(&key).await
    }

    /// Upload a serialized compiled yara ruleset to the cache bucket
    ///
    /// After a successful upload any sibling rulesets under the same engine
    /// version are best-effort deleted since only the newest content hash will
    /// ever be looked up again.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is uploading this cached ruleset
    /// * `engine_version` - The yara engine version this ruleset was serialized with
    /// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
    /// * `body` - The serialized ruleset to stream to s3
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "Event::upload_yara_ruleset",
        skip(user, body, shared),
        err(Debug)
    )]
    pub async fn upload_yara_ruleset(
        user: &User,
        engine_version: &str,
        ruleset_hash: &str,
        body: axum::body::Body,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins can write to the serialized ruleset cache
        is_admin!(user);
        // build the s3 key for this cached ruleset
        let key = yara_ruleset_key(engine_version, ruleset_hash)?;
        // stream this serialized ruleset to s3
        shared.s3.yara_rulesets.stream_body(&key, body).await?;
        // best-effort delete any sibling rulesets under the same engine version
        prune_yara_ruleset_siblings(engine_version, &key, shared).await;
        Ok(())
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

impl<S> FromRequestParts<S> for StatsHistoryParams
where
    S: Send + Sync,
{
    /// Bad history params are rejected with an [`ApiError`]
    type Rejection = ApiError;

    /// Extract scan stats history params from a requests query string
    ///
    /// The range endpoints are required so a request without a query string is
    /// a 400 rather than a default.
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract params from
    /// * `_state` - The shared state for this extractor (unused)
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // get this requests query string if one was given
        let query = parts.uri.query().unwrap_or_default();
        // try to deserialize our query string
        Ok(serde_qs::Config::new()
            .max_depth(5)
            .deserialize_str(query)?)
    }
}
