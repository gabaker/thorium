//! The routes supporting events in Thorium
use axum::Router;
use axum::body::Body;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum_extra::body::AsyncReadBody;
use tracing::instrument;
use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};
use crate::models::AuthedUser;
use crate::models::{
    Event, EventCacheStatus, EventCacheStatusOpts, EventDepths, EventIds, EventPeekOpts,
    EventPopOpts, EventStats, EventType, EventWorkerOpts, HuntChunkEvent, KindDepth, KindStats,
    ReapSummary, ScanStatsSnapshot, StatsHistoryParams, WorkerStatsReport, WorkflowGraphChange,
    WorkflowGraphEvent, YaraSampleScanEvent, YaraScanTrigger,
};
use crate::not_found;
use crate::utils::{ApiError, AppState};

/// Pop and handle some events
///
/// # Arguments
//
///
/// * `user` - The user that is popping events
/// * `params` - The query params to use with this request
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::pop", skip_all, err(Debug))]
#[utoipa::path(
    patch,
    path = "/api/events/pop/{kind}/",
    params(
        ("kind" = EventType, Path, description = "The type of events to pop"),
        EventPopOpts,
    ),
    responses(
        (status = 200, description = "A List of popped events in Thorium", body = Vec<Event>),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn pop(
    user: AuthedUser,
    Path(kind): Path<EventType>,
    params: EventPopOpts,
    State(state): State<AppState>,
) -> Result<Json<Vec<Event>>, ApiError> {
    // pop some events, claiming them for this worker if one was set
    let events = Event::pop(
        &user,
        kind,
        params.limit,
        params.worker.as_deref(),
        &state.shared,
    )
    .await?;
    Ok(Json(events))
}

/// Peek at the queued events of one kind without moving them in flight
///
/// Only admins can peek at events, and this is meant for tests.
///
/// # Arguments
///
/// * `user` - The user that is peeking at events
/// * `kind` - The kind of events to peek at
/// * `params` - The query params to use with this request
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::peek", skip_all, err(Debug))]
#[utoipa::path(
    get,
    path = "/api/events/peek/{kind}/",
    params(
        ("kind" = EventType, Path, description = "The type of events to peek at"),
        EventPeekOpts,
    ),
    responses(
        (status = 200, description = "The queued events of this kind, oldest first", body = Vec<Event>),
        (status = 400, description = "The event kind is unknown"),
        (status = 401, description = "This user is not an admin"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn peek(
    user: AuthedUser,
    Path(kind): Path<EventType>,
    params: EventPeekOpts,
    State(state): State<AppState>,
) -> Result<Json<Vec<Event>>, ApiError> {
    // peek at the queued events
    let events = Event::peek(&user, kind, params.limit, &state.shared).await?;
    Ok(Json(events))
}

/// clear some events
///
/// # Arguments
//
///
/// * `user` - The user that is clearing events
/// * `kind` - The kind of events to clear
/// * `params` - The query params scoping this clear to a single worker
/// * `id_list` - The events to clear
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::clear", skip_all, err(Debug))]
#[utoipa::path(
    delete,
    path = "/api/events/clear/{kind}/",
    params(
        ("kind" = EventType, Path, description = "The type of events to clear"),
        EventWorkerOpts,
    ),
    request_body = EventIds,
    responses(
        (status = 204, description = "The requested events were cleared"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation; a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn clear(
    user: AuthedUser,
    Path(kind): Path<EventType>,
    Query(params): Query<EventWorkerOpts>,
    State(state): State<AppState>,
    Json(id_list): Json<EventIds>,
) -> Result<StatusCode, ApiError> {
    // clear these events from the right workers in flight keys
    Event::clear(
        &user,
        kind,
        &id_list.ids,
        params.worker.as_deref(),
        &state.shared,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reset in flight events for a kind of event
///
/// Without a worker query param every event in the legacy shared in flight
/// queue is reset; with one only that workers own in flight events are reset.
///
/// # Arguments
//
///
/// * `user` - The user that is resetting events
/// * `kind` - The kind of events to reset
/// * `params` - The query params scoping this reset to a single worker
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::rest_all", skip_all, err(Debug))]
#[utoipa::path(
    patch,
    path = "/api/events/reset/{kind}/",
    params(
        ("kind" = EventType, Path, description = "The type of events to reset"),
        EventWorkerOpts,
    ),
    responses(
        (status = 204, description = "All events currently in process were cleared"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn reset_all(
    user: AuthedUser,
    Path(kind): Path<EventType>,
    Query(params): Query<EventWorkerOpts>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // reset either a single workers in flight events or the legacy shared queue
    match params.worker.as_deref() {
        // reset only this workers own in flight events
        Some(worker) => {
            Event::reset_worker(&user, kind, worker, &state.shared).await?;
        }
        // reset every event in the legacy shared in flight queue
        None => Event::reset_all(&user, kind, &state.shared).await?,
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Reset a single workers in flight events for one kind of event
///
/// This exists apart from the legacy reset route (which also accepts a worker
/// query param) so new event handlers get a clean 404 from an old API instead
/// of the old route silently ignoring the worker param and resetting the whole
/// kinds legacy in flight queue out from under every peer replica.
///
/// # Arguments
//
///
/// * `user` - The user that is resetting events
/// * `worker` - The worker whose in flight events to reset
/// * `kind` - The kind of events to reset
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::reset_worker", skip_all, err(Debug))]
#[utoipa::path(
    patch,
    path = "/api/events/workers/reset/{worker}/{kind}/",
    params(
        ("worker" = String, Path, description = "The worker whose in flight events to reset"),
        ("kind" = EventType, Path, description = "The type of events to reset"),
    ),
    responses(
        (status = 204, description = "This workers in flight events were reset"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn reset_worker(
    user: AuthedUser,
    Path((worker, kind)): Path<(String, EventType)>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // reset only this workers own in flight events
    Event::reset_worker(&user, kind, &worker, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reap dead event handler workers and over-aged in flight events
///
/// # Arguments
//
///
/// * `user` - The user that is reaping dead workers
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::reap", skip_all, err(Debug))]
#[utoipa::path(
    post,
    path = "/api/events/workers/reap/",
    responses(
        (status = 200, description = "A summary of the reap pass that ran", body = ReapSummary),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn reap(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<Json<ReapSummary>, ApiError> {
    // reap dead workers and over-aged in flight events
    let summary = Event::reap(&user, &state.shared).await?;
    Ok(Json(summary))
}

/// Upsert an event handler workers heartbeat
///
/// # Arguments
//
///
/// * `user` - The user that is heartbeating
/// * `worker` - The worker that is heartbeating
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::heartbeat", skip_all, err(Debug))]
#[utoipa::path(
    patch,
    path = "/api/events/workers/heartbeat/{worker}/",
    params(
        ("worker" = String, Path, description = "The worker that is heartbeating"),
    ),
    responses(
        (status = 204, description = "This workers heartbeat was recorded"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn heartbeat(
    user: AuthedUser,
    Path(worker): Path<String>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // upsert this workers heartbeat
    Event::heartbeat(&user, &worker, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Drain an event handler workers in flight events and deregister it
///
/// # Arguments
//
///
/// * `user` - The user that is draining this worker
/// * `worker` - The worker to drain and deregister
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::drain_worker", skip_all, err(Debug))]
#[utoipa::path(
    delete,
    path = "/api/events/workers/{worker}/",
    params(
        ("worker" = String, Path, description = "The worker to drain and deregister"),
    ),
    responses(
        (status = 200, description = "The number of events drained back to the pending queues", body = u64),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn drain_worker(
    user: AuthedUser,
    Path(worker): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<u64>, ApiError> {
    // drain this workers in flight events and deregister it
    let drained = Event::drain_worker(&user, &worker, &state.shared).await?;
    Ok(Json(drained))
}

/// Get the queue depths for every kind of event plus the live worker count
///
/// # Arguments
//
///
/// * `user` - The user that is getting queue depths
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::depths", skip_all, err(Debug))]
#[utoipa::path(
    get,
    path = "/api/events/depths/",
    responses(
        (status = 200, description = "The current per kind event queue depths", body = EventDepths),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn depths(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<Json<EventDepths>, ApiError> {
    // get the current event queue depths
    let depths = Event::depths(&user, &state.shared).await?;
    Ok(Json(depths))
}

/// Get the live scan stats aggregate
///
/// # Arguments
//
///
/// * `user` - The user that is getting scan stats
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::stats", skip_all, err(Debug))]
#[utoipa::path(
    get,
    path = "/api/events/stats/",
    responses(
        (status = 200, description = "The live per kind and per worker scan stats", body = EventStats),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn stats(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<Json<EventStats>, ApiError> {
    // assemble the live scan stats aggregate
    let stats = Event::stats(&user, &state.shared).await?;
    Ok(Json(stats))
}

/// Get a ranged, downsampled slice of the scan stats history
///
/// # Arguments
//
///
/// * `user` - The user that is reading scan stats history
/// * `params` - The half open range and stride to read
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::stats_history", skip_all, err(Debug))]
#[utoipa::path(
    get,
    path = "/api/events/stats/history/",
    params(
        StatsHistoryParams,
    ),
    responses(
        (status = 200, description = "The downsampled scan stats history oldest first (newest row per stride, capped around 5,000 points)", body = Vec<ScanStatsSnapshot>),
        (status = 400, description = "The range is empty, inverted, or wider than the retention window"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn stats_history(
    user: AuthedUser,
    params: StatsHistoryParams,
    State(state): State<AppState>,
) -> Result<Json<Vec<ScanStatsSnapshot>>, ApiError> {
    // read the requested slice of the scan stats history
    let history = Event::stats_history(&user, &params, &state.shared).await?;
    Ok(Json(history))
}

/// Trigger a quantized scan stats snapshot
///
/// # Arguments
//
///
/// * `user` - The user that is triggering a snapshot
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::snapshot_stats", skip_all, err(Debug))]
#[utoipa::path(
    post,
    path = "/api/events/stats/snapshot/",
    responses(
        (status = 200, description = "The scan stats snapshot this trigger persisted", body = ScanStatsSnapshot),
        (status = 204, description = "A peer already persisted this quantized window"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn snapshot_stats(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    // persist this windows snapshot unless a peer already has
    match Event::snapshot_stats(&user, &state.shared).await? {
        // this trigger won its window and persisted a row
        Some(snapshot) => Ok((StatusCode::OK, Json(snapshot)).into_response()),
        // a peer already wrote this window
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// Publish one workers cumulative counters for one kind of event
///
/// # Arguments
//
///
/// * `user` - The user that is publishing worker stats
/// * `worker` - The worker that took this reading
/// * `kind` - The kind of events these counters cover
/// * `state` - Shared Thorium objects
/// * `report` - The counter report to upsert
#[instrument(name = "routes::events::publish_worker_stats", skip_all, err(Debug))]
#[utoipa::path(
    post,
    path = "/api/events/stats/workers/{worker}/{kind}/",
    params(
        ("worker" = String, Path, description = "The worker that took this reading"),
        ("kind" = EventType, Path, description = "The kind of events these counters cover"),
    ),
    request_body(description = "The counter report to upsert", content = WorkerStatsReport),
    responses(
        (status = 204, description = "This workers counter report was upserted"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation; a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn publish_worker_stats(
    user: AuthedUser,
    Path((worker, kind)): Path<(String, EventType)>,
    State(state): State<AppState>,
    Json(report): Json<WorkerStatsReport>,
) -> Result<StatusCode, ApiError> {
    // upsert this workers counter report
    Event::publish_worker_stats(&user, kind, &worker, &report, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Download a cached serialized yara ruleset
///
/// # Arguments
//
///
/// * `user` - The user that is getting this cached ruleset
/// * `engine_version` - The yara engine version this ruleset was serialized with
/// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::get_yara_ruleset", skip_all, err(Debug))]
#[utoipa::path(
    get,
    path = "/api/events/yara/ruleset/{engine_version}/{ruleset_hash}/",
    params(
        ("engine_version" = String, Path, description = "The yara engine version this ruleset was serialized with"),
        ("ruleset_hash" = String, Path, description = "The content hash of the rules this ruleset was compiled from"),
    ),
    responses(
        (status = 200, description = "The serialized ruleset byte stream", body = Vec<u8>),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "No cached ruleset exists for this engine version and hash"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn get_yara_ruleset(
    user: AuthedUser,
    Path((engine_version, ruleset_hash)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<AsyncReadBody, ApiError> {
    // try to get this cached ruleset from s3
    match Event::get_yara_ruleset(&user, &engine_version, &ruleset_hash, &state.shared).await? {
        // stream this cached ruleset back
        Some(stream) => Ok(AsyncReadBody::new(stream.into_async_read())),
        // this cache entry does not exist so return a clean miss
        None => not_found!(format!(
            "No cached yara ruleset for engine version '{engine_version}' and hash '{ruleset_hash}'"
        )),
    }
}

/// Upload a serialized yara ruleset to the cache
///
/// # Arguments
//
///
/// * `user` - The user that is uploading this cached ruleset
/// * `engine_version` - The yara engine version this ruleset was serialized with
/// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
/// * `state` - Shared Thorium objects
/// * `body` - The serialized ruleset to stream to s3
#[instrument(name = "routes::events::upload_yara_ruleset", skip_all, err(Debug))]
#[utoipa::path(
    post,
    path = "/api/events/yara/ruleset/{engine_version}/{ruleset_hash}/",
    params(
        ("engine_version" = String, Path, description = "The yara engine version this ruleset was serialized with"),
        ("ruleset_hash" = String, Path, description = "The content hash of the rules this ruleset was compiled from"),
    ),
    request_body(description = "The serialized ruleset bytes to cache", content_type = "application/octet-stream"),
    responses(
        (status = 204, description = "The serialized ruleset was cached"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
async fn upload_yara_ruleset(
    user: AuthedUser,
    Path((engine_version, ruleset_hash)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    // stream this serialized ruleset to s3
    Event::upload_yara_ruleset(&user, &engine_version, &ruleset_hash, body, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// get the status of our event handler cache
///
/// This is used to determine if our local cache needs to be refreshed
#[utoipa::path(
    get,
    path = "/api/events/cache/status/",
    params(
        EventCacheStatusOpts,
    ),
    responses(
        (status = 200, description = "Event handler cache status", body = EventCacheStatus),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::events::get_cache_status", skip_all, err(Debug))]
async fn get_cache_status(
    user: AuthedUser,
    Query(params): Query<EventCacheStatusOpts>,
    State(state): State<AppState>,
) -> Result<Json<EventCacheStatus>, ApiError> {
    // get our event watermark
    let status = Event::get_cache_status(&user, params.reset, &state.shared).await?;
    Ok(Json(status))
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(
        pop,
        peek,
        clear,
        reset_all,
        reset_worker,
        get_cache_status,
        reap,
        heartbeat,
        drain_worker,
        depths,
        stats,
        stats_history,
        snapshot_stats,
        publish_worker_stats,
        get_yara_ruleset,
        upload_yara_ruleset
    ),
    components(schemas(
        Event,
        EventCacheStatus,
        EventCacheStatusOpts,
        EventDepths,
        EventIds,
        EventPeekOpts,
        EventPopOpts,
        EventStats,
        EventType,
        EventWorkerOpts,
        HuntChunkEvent,
        KindDepth,
        KindStats,
        ReapSummary,
        ScanStatsSnapshot,
        StatsHistoryParams,
        WorkerStatsReport,
        WorkflowGraphEvent,
        WorkflowGraphChange,
        YaraSampleScanEvent,
        YaraScanTrigger
    )),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct EventApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(EventApiDocs::openapi())
}

/// Add the events routes to our router
///
/// # Arguments
///
/// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/events/pop/{kind}/", patch(pop))
        .route("/events/peek/{kind}/", get(peek))
        .route("/events/clear/{kind}/", delete(clear))
        .route("/events/reset/{kind}/", patch(reset_all))
        .route("/events/cache/status/", get(get_cache_status))
        .route("/events/workers/reap/", post(reap))
        .route(
            "/events/workers/reset/{worker}/{kind}/",
            patch(reset_worker),
        )
        .route("/events/workers/heartbeat/{worker}/", patch(heartbeat))
        .route("/events/workers/{worker}/", delete(drain_worker))
        .route("/events/depths/", get(depths))
        .route("/events/stats/", get(stats))
        .route("/events/stats/history/", get(stats_history))
        .route("/events/stats/snapshot/", post(snapshot_stats))
        .route(
            "/events/stats/workers/{worker}/{kind}/",
            post(publish_worker_stats),
        )
        .route(
            "/events/yara/ruleset/{engine_version}/{ruleset_hash}/",
            get(get_yara_ruleset).post(upload_yara_ruleset),
        )
}
