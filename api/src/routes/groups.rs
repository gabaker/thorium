use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use tracing::instrument;

use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};
// our imports
use crate::is_admin;
use crate::models::AuthedUser;
use crate::models::{
    Group, GroupAllowAction, GroupAllowed, GroupAllowedUpdate, GroupDetailsList, GroupList,
    GroupListParams, GroupMap, GroupRequest, GroupStats, GroupUpdate, GroupUsers,
    GroupUsersRequest, GroupUsersUpdate, PipelineStats, Roles, StageStats,
};
use crate::utils::{ApiError, AppState};

/// Creates a new group
///
/// Scoped tokens cannot create groups.
///
/// # Arguments
///
/// * `user` - The user that is creating this group
/// * `state` - Shared Thorium objects
/// * `group` - The group to create
#[utoipa::path(
    post,
    path = "/api/groups/",
    request_body = GroupRequest,
    responses(
        (status = 204, description = "Group created"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route. Only a full token may use this route: a scoped token, including a run token, is refused."),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::groups::create", skip_all, err(Debug))]
async fn create(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(group): Json<GroupRequest>,
) -> Result<StatusCode, ApiError> {
    // require a fully authed user since scoped tokens cannot create groups
    let user = user.require_full()?;
    // create group
    Group::create(&user, group, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Gets details on a specific group
///
/// # Arguments
///
/// * `group` - The group to get details on
/// * `user` - The user that is creating this group
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/groups/{group}/details",
    params(
        ("group" = String, Path, description = "The group to get details on")
    ),
    responses(
        (status = 200, description = "Group details", body = Group),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::groups::get_group", skip_all, err(Debug))]
async fn get_group(
    user: AuthedUser,
    Path(group): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Group>, ApiError> {
    // get this groups info
    let group = Group::get(&user, &group, &state.shared).await?;
    Ok(Json(group))
}

/// List groups
///
/// # Arguments
///
/// * `cursor` - The cursor value to determine where start listing groups at
/// * `limit` - The max number of groups to list at once
/// * `user` - The user that is creating this group
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/groups/",
    params(
        GroupListParams,
    ),
    responses(
        (status = 200, description = "Group list", body = GroupList),
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
#[instrument(name = "routes::groups::list", skip_all, err(Debug))]
async fn list(
    user: AuthedUser,
    Query(params): Query<GroupListParams>,
    State(state): State<AppState>,
) -> Result<Json<GroupList>, ApiError> {
    // get vector of group names
    let groups = Group::list(&user, params.cursor, params.limit, &state.shared).await?;
    Ok(Json(groups))
}

/// List groups with details
///
/// # Arguments
///
/// * `cursor` - The cursor value to determine where start listing groups at
/// * `limit` - The max number of groups to list at once
/// * `user` - The user that is creating this group
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/groups/details/",
    params(
        GroupListParams,
    ),
    responses(
        (status = 200, description = "Group details list", body = GroupDetailsList),
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
#[instrument(name = "routes::groups::list_details", skip_all, err(Debug))]
async fn list_details(
    user: AuthedUser,
    Query(params): Query<GroupListParams>,
    State(state): State<AppState>,
) -> Result<Json<GroupDetailsList>, ApiError> {
    // get vector of group names
    let groups = Group::list_details(&user, params.cursor, params.limit, &state.shared).await?;
    Ok(Json(groups))
}

/// Updates a group
///
/// Scoped tokens cannot change group membership or settings.
///
/// # Arguments
///
/// * `user` - The user that is updating this group
/// * `group` - The name of the group to update
/// * `state` - Shared Thorium objects
/// * `update` - The update to apply to this group
#[utoipa::path(
    patch,
    path = "/api/groups/{group}",
    params(
        ("group" = String, Path, description = "The name of the group to update"),
    ),
    request_body = GroupUpdate,
    responses(
        (status = 204, description = "Group updated"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route. Only a full token may use this route: a scoped token, including a run token, is refused."),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 409, description = "The update would remove the requesting user as an owner or leave the group with no owners"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::groups::update", skip(user, state, update), err(Debug))]
async fn update(
    user: AuthedUser,
    Path(group): Path<String>,
    State(state): State<AppState>,
    Json(update): Json<GroupUpdate>,
) -> Result<StatusCode, ApiError> {
    // require a fully authed user since scoped tokens cannot change group membership or settings
    let user = user.require_full()?;
    // get group
    let group = Group::get(&user, &group, &state.shared).await?;
    // update group
    group.update(update, &user, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete group
///
/// Scoped tokens cannot delete groups.
///
/// # Arguments
///
/// * `user` - The user that is deleting this group
/// * `group` - The name of the group to delete
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/groups/{group}",
    params(
        ("group" = String, Path, description = "The name of the group to delete")
    ),
    responses(
        (status = 204, description = "Group deleted"),
        (status = 401, description = "This user is not authorized to access this route. Only a full token may use this route: a scoped token, including a run token, is refused."),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::groups::delete_group", skip(user, state), err(Debug))]
async fn delete_group(
    user: AuthedUser,
    Path(group): Path<String>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // require a fully authed user since scoped tokens cannot delete groups
    let user = user.require_full()?;
    // get group
    let group = Group::get(&user, &group, &state.shared).await?;
    // delete group
    group.delete(&user, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Syncs all ldap metagroups and their users
///
/// # Arguments
///
/// * `user` - The user that is telling Thorium to sync with ldap
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/groups/sync/ldap",
    params(),
    responses(
        (status = 204, description = "Ldap metagroups and users sync'd"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::groups::sync_ldap", skip(user, state), err(Debug))]
async fn sync_ldap(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // make sure we are an admin
    is_admin!(user);
    // sync all groups with ldap
    Group::sync_ldap(&state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Gets statistics on a specific group
///
/// # Arguments
///
/// * `user` - The user that is getting these statistics
/// * `_params` - The group list query params (unused)
/// * `group` - The group to get statistics on
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/groups/{group}/stats",
    params(
        ("group" = String, Path, description = "The group to get details on"),
        GroupListParams,
    ),
    responses(
        (status = 200, description = "Group stats", body = GroupStats),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::groups::get_stats", skip_all, err(Debug))]
async fn get_stats(
    user: AuthedUser,
    Query(_params): Query<GroupListParams>,
    Path(group): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<GroupStats>, ApiError> {
    // get the group we are getting workload statuses for
    let group = Group::get(&user, &group, &state.shared).await?;
    // get the status object for this group
    let status = group.stats(&state.shared).await?;
    Ok(Json(status))
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(create, list, get_group, list_details, update, delete_group, sync_ldap, get_stats),
    components(schemas(Group, GroupAllowed, GroupAllowedUpdate, GroupAllowAction, GroupDetailsList, GroupList, GroupListParams, GroupMap, GroupRequest, GroupStats, GroupUpdate, GroupUsersRequest, GroupUsers, GroupUsersUpdate, PipelineStats, Roles, StageStats)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct GroupApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(GroupApiDocs::openapi())
}

/// Add the groups routes to our router
///
/// # Arguments
///
// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/groups/", post(create).get(list))
        .route("/groups/{group}/details", get(get_group))
        .route("/groups/details/", get(list_details))
        .route("/groups/{group}", patch(update).delete(delete_group))
        .route("/groups/sync/ldap", post(sync_ldap))
        .route("/groups/{group}/stats", get(get_stats))
}
