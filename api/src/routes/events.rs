//! The routes supporting events in Thorium
use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch};
use tracing::instrument;
use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};
use crate::models::AuthedUser;
use crate::models::{
    Event, EventCacheStatus, EventCacheStatusOpts, EventIds, EventPeekOpts, EventPopOpts,
    EventType, WorkflowGraphChange, WorkflowGraphEvent,
};
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
    // pop some events
    let events = Event::pop(&user, kind, params.limit, &state.shared).await?;
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
/// * `id_list` - The events to clear
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::clear", skip_all, err(Debug))]
#[utoipa::path(
    delete,
    path = "/api/events/clear/{kind}/",
    params(
        ("kind" = EventType, Path, description = "The type of events to clear"),
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
    State(state): State<AppState>,
    Json(id_list): Json<EventIds>,
) -> Result<StatusCode, ApiError> {
    // clear these events
    Event::clear(&user, kind, &id_list.ids, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reset all in flight events
///
/// # Arguments
//
///
/// * `user` - The user that is resetting events
/// * `kind` - The kind of events to reset
/// * `state` - Shared Thorium objects
#[instrument(name = "routes::events::rest_all", skip_all, err(Debug))]
#[utoipa::path(
    patch,
    path = "/api/events/reset/{kind}/",
    params(
        ("kind" = EventType, Path, description = "The type of events to reset"),
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
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // reset all in flight events events
    Event::reset_all(&user, kind, &state.shared).await?;
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
    paths(pop, peek, clear, reset_all, get_cache_status),
    components(schemas(Event, EventCacheStatus, EventCacheStatusOpts, EventType, EventPeekOpts, EventPopOpts, WorkflowGraphEvent, WorkflowGraphChange)),
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
}
