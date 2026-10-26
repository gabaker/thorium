use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use tracing::instrument;
use uuid::Uuid;

use super::shared;
use crate::is_admin;
use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};
use crate::models::AuthedUser;
use crate::models::pipelines::{BannedImageBan, GenericBan};
use crate::models::{
    EventTrigger, Group, Notification, NotificationParams, NotificationRequest, Pipeline,
    PipelineBan, PipelineBanKind, PipelineBanUpdate, PipelineDetailsList, PipelineKey,
    PipelineList, PipelineListParams, PipelineRequest, PipelineUpdate, TagType,
};
use crate::utils::{ApiError, AppState};

/// A notification for a pipeline as the documents describe it
pub type PipelineNotification = Notification<Pipeline>;

/// Creates a new pipeline in a group
///
/// # Arguments
///
/// * `user` - The user that is creating this pipeline
/// * `request` - The pipeline to create
/// * `state` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/pipelines/",
    request_body = PipelineRequest,
    responses(
        (status = 204, description = "Pipeline created"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route. A run token is refused."),
        (status = 409, description = "A pipeline with this name already exists in the group"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::pipelines::create", skip_all, err(Debug))]
async fn create(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(request): Json<PipelineRequest>,
) -> Result<StatusCode, ApiError> {
    // a run token can't create a pipeline other runs could follow
    user.refuse_run_token("create a pipeline")?;
    // create pipeline object
    Pipeline::create(&user, request, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Gets details about a pipeline
///
/// # Arguments
///
/// * `user` - The user that is getting details about this pipeline
/// * `group` - The group this pipeline is in
/// * `pipeline` - The name of the pipeline to get details about
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/pipelines/data/{group}/{pipeline}",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline to get details about"),
    ),
    responses(
        (status = 200, description = "Pipeline details", body = Pipeline),
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
#[instrument(name = "routes::pipelines::get", skip_all, err(Debug))]
async fn get_pipeline(
    user: AuthedUser,
    Path((group, pipeline)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Pipeline>, ApiError> {
    // get pipeline data
    let (_, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    Ok(Json(pipeline))
}

/// Lists pipelines in a group
///
/// # Arguments
///
/// * `user` - The user that is listing pipelines
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/pipelines/list/{group}/",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        PipelineListParams,
    ),
    responses(
        (status = 200, description = "Pipeline details", body = PipelineList),
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
#[instrument(
    name = "routes::pipelines::list",
    skip(user, params, state),
    err(Debug)
)]
async fn list(
    user: AuthedUser,
    Path(group): Path<String>,
    Query(params): Query<PipelineListParams>,
    State(state): State<AppState>,
) -> Result<Json<PipelineList>, ApiError> {
    // authorize this user is apart of this group
    let group = Group::authorize(&user, &group, &state.shared).await?;
    // get list of pipelines in group
    let names = Pipeline::list(&group, params.cursor, params.limit, &state.shared).await?;
    Ok(Json(names))
}

/// Lists pipelines in a group with details
///
/// # Arguments
///
/// * `user` - The user that is listing pipelines
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/pipelines/list/{group}/details/",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        PipelineListParams,
    ),
    responses(
        (status = 200, description = "Pipeline details", body = PipelineDetailsList),
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
#[instrument(
    name = "routes::pipelines::list_details",
    skip(user, params, state),
    err(Debug)
)]
async fn list_details(
    user: AuthedUser,
    Path(group): Path<String>,
    Query(params): Query<PipelineListParams>,
    State(state): State<AppState>,
) -> Result<Json<PipelineDetailsList>, ApiError> {
    // authorize this user is apart of this group
    let group = Group::authorize(&user, &group, &state.shared).await?;
    // get list of pipelines in group
    let pipelines = Pipeline::list(&group, params.cursor, params.limit, &state.shared).await?;
    // get details these pipelines
    let details = pipelines.details(&group, &state.shared).await?;
    Ok(Json(details))
}

/// Updates a pipeline
///
/// # Arguments
///
/// * `user` - The user that is updating this pipeline
/// * `group` - The group this pipeline is in
/// * `pipeline` - The name of the pipeline to update
/// * `state` - Shared Thorium objects
/// * `update` - The update to apply
#[utoipa::path(
    patch,
    path = "/api/pipelines/{group}/{pipeline}",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline to update"),
    ),
    request_body = PipelineUpdate,
    responses(
        (status = 204, description = "Pipeline updated"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route. A run token is refused."),
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
#[instrument(name = "routes::pipelines::update", skip_all, err(Debug))]
async fn update(
    user: AuthedUser,
    Path((group, pipeline)): Path<(String, String)>,
    State(state): State<AppState>,
    Json(update): Json<PipelineUpdate>,
) -> Result<StatusCode, ApiError> {
    // a run token can't change a pipeline other runs could follow
    user.refuse_run_token("update a pipeline")?;
    // get pipeline and group
    let (group, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    // update pipelines
    pipeline
        .update(update, &user, &group, &state.shared)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a pipeline
///
/// This will also delete any reactions tied to this pipeline.
///
/// # Arguments
///
/// * `user` - The user that is deleting this pipeline
/// * `group` - The group this pipeline is in
/// * `pipeline` - The name of the pipeline to delete
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/pipelines/{group}/{pipeline}",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline to delete"),
    ),
    responses(
        (status = 204, description = "Pipeline deleted"),
        (status = 401, description = "This user is not authorized to access this route. A run token is refused."),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(
    name = "routes::pipelines::delete_pipeline",
    skip(user, state),
    err(Debug)
)]
async fn delete_pipeline(
    user: AuthedUser,
    Path((group, pipeline)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // a run token can't delete a pipeline or the reactions of other users that go with it
    user.refuse_run_token("delete a pipeline")?;
    // get pipeline
    let (group, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    // delete pipeline from backend
    pipeline.delete(&user, &group, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Create a notification for a pipeline in scylla
///
/// # Arguments
///
/// * `user` - The user sending the request
/// * `group` - The group the pipeline is in
/// * `pipeline` - The name of the pipeline the notification will be created for
/// * `state` - Shared Thorium objects
/// * `params` - The notification params sent with the request
/// * `req` - The notification request that was sent
#[utoipa::path(
    post,
    path = "/api/pipelines/notifications/{group}/{pipeline}",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline the notification will be created for"),
        NotificationParams,
    ),
    responses(
        (status = 204, description = "Pipeline notification created"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
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
#[instrument(name = "routes::pipelines::create_notification", skip_all, err(Debug))]
async fn create_notification(
    user: AuthedUser,
    Path((group, pipeline)): Path<(String, String)>,
    State(state): State<AppState>,
    params: NotificationParams,
    Json(req): Json<NotificationRequest<Pipeline>>,
) -> Result<StatusCode, ApiError> {
    // only admins can create pipeline notifications
    is_admin!(&user);
    // check that the pipeline exists
    let (_, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    // generate the pipeline's key
    let key = PipelineKey::from(&pipeline);
    // create the notification
    shared::notifications::create_notification(pipeline, key, req, params, &state.shared).await
}

/// Get all of the pipeline's notifications
///
/// # Arguments
///
/// * `user` - The user that is requesting the notifications
/// * `group` - The group the pipeline is in
/// * `pipeline` - The name of the pipeline whose notifications are being requested
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/pipelines/notifications/{group}/{pipeline}",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline whose notifications are being requested"),
    ),
    responses(
        (status = 200, description = "Pipeline notifications", body = Vec<PipelineNotification>),
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
#[instrument(name = "routes::pipelines::get_notifications", skip_all, err(Debug))]
async fn get_notifications(
    user: AuthedUser,
    Path((group, pipeline)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Notification<Pipeline>>>, ApiError> {
    // check that the pipeline exists and the user has access
    let (_, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    // generate the pipeline's key
    let key = PipelineKey::from(&pipeline);
    // get all of the pipeline's notifications
    shared::notifications::get_notifications(pipeline, key, &state.shared).await
}

/// Delete a specific notification from a pipeline
///
/// # Arguments
///
/// * `user` - The user that is requesting the deletion
/// * `group` - The group the pipeline is in
/// * `pipeline` - The name of the pipeline whose log is being deleted
/// * `id` - The notification's unique ID
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/pipelines/notifications/{group}/{pipeline}/{id}",
    params(
        ("group" = String, Path, description = "The group this pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline whose log is being deleted"),
        ("id" = Uuid, Path, description = "The notification's unique ID"),
    ),
    responses(
        (status = 204, description = "Pipeline notification deleted"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
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
#[instrument(name = "routes::pipelines::delete_notification", skip_all, err(Debug))]
async fn delete_notification(
    user: AuthedUser,
    Path((group, pipeline, id)): Path<(String, String, Uuid)>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // only admins can delete pipeline logs
    is_admin!(&user);
    // check that the pipeline exists
    let (_, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    // generate the pipeline's key
    let key = PipelineKey::from(&pipeline);
    // delete the notification
    shared::notifications::delete_notification(pipeline, key, None, id, &state.shared).await
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(create, get_pipeline, list, list_details, update, delete_pipeline, get_notifications, create_notification, delete_notification),
    components(schemas(BannedImageBan, EventTrigger, GenericBan, Pipeline, PipelineBan, PipelineBanKind, PipelineBanUpdate, PipelineDetailsList, PipelineList, PipelineListParams, PipelineRequest, PipelineUpdate, TagType)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct PipelineApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(PipelineApiDocs::openapi())
}

/// Add the file routes to our router
///
/// # Arguments
///
// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/pipelines/", post(create))
        .route("/pipelines/data/{group}/{pipeline}", get(get_pipeline))
        .route("/pipelines/list/{group}/", get(list))
        .route("/pipelines/list/{group}/details/", get(list_details))
        .route(
            "/pipelines/{group}/{pipeline}",
            patch(update).delete(delete_pipeline),
        )
        .route(
            "/pipelines/notifications/{group}/{pipeline}",
            get(get_notifications).post(create_notification),
        )
        .route(
            "/pipelines/notifications/{group}/{pipeline}/{id}",
            delete(delete_notification),
        )
}
