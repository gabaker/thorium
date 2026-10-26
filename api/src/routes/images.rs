use super::shared;
use crate::is_admin;
use axum::Router;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use tracing::instrument;
use utoipa::openapi::Deprecated;
use utoipa::{Modify, OpenApi};
use uuid::Uuid;

use super::{OpenApiParameterExamples, OpenApiSecurity};
// our imports
use crate::models::AuthedUser;
use crate::models::images::{GenericBan, InvalidHostPathBan, InvalidUrlBan};
use crate::models::{
    ArgStrategy, AutoTag, AutoTagLogic, AutoTagUpdate, ChildFilters, ChildFiltersUpdate,
    ChildrenDependencySettings, ChildrenDependencySettingsUpdate, Cleanup, CleanupUpdate,
    ConfigMap, Dependencies, DependenciesUpdate, DependencyPassStrategy,
    EphemeralDependencySettings, EphemeralDependencySettingsUpdate, FileSystemDependencySettings,
    FileSystemDependencySettingsUpdate, FilesHandler, FilesHandlerUpdate, Group, HostPath,
    HostPathTypes, Image, ImageArgs, ImageArgsUpdate, ImageBan, ImageBanKind, ImageBanUpdate,
    ImageDetailsList, ImageKey, ImageLifetime, ImageList, ImageListParams,
    ImageNetworkPolicyUpdate, ImageRequest, ImageScaler, ImageUpdate, Kvm, KvmUpdate,
    KwargDependency, NFS, Notification, NotificationLevel, NotificationParams, NotificationRequest,
    OutputCollection, OutputCollectionUpdate, OutputDisplayType, OutputHandler,
    RepoDependencySettings, RepoDependencySettingsUpdate, Resources, ResourcesRequest,
    ResourcesUpdate, ResultDependencySettings, ResultDependencySettingsUpdate,
    SampleDependencySettings, SampleDependencySettingsUpdate, Secret, SecurityContext,
    SecurityContextUpdate, SpawnLimits, TagDependencySettings, TagDependencySettingsUpdate,
    ToolVersion, Volume, VolumeTypes,
};
use crate::utils::{ApiError, AppState};

/// A notification for an image as the documents describe it
pub type ImageNotification = Notification<Image>;

/// Why image creates and updates are refused
const IMAGE_WRITES_GONE: &str = "Images can no longer be created or updated; Thorium runs Tools \
    instead. Create and update tools with the Tool routes (/api/entities/) or with \
    `thorctl tools`, and set pipeline stages with `thorctl pipelines stage-tool`.";

/// Refuse an image create or update with `410 Gone`
///
/// Tests that seed stored images opt their groups back in to image writes.
///
/// # Arguments
///
/// * `group` - The group the image would be written in
#[cfg_attr(not(feature = "test-utilities"), allow(unused_variables))]
fn image_writes_gone(group: &str) -> Result<(), ApiError> {
    // let tests seed images in the groups they opted in
    #[cfg(feature = "test-utilities")]
    if crate::test_utilities::faults::image_writes_allowed(group) {
        return Ok(());
    }
    // every other image write is gone
    Err(ApiError::new(
        StatusCode::GONE,
        Some(IMAGE_WRITES_GONE.to_owned()),
    ))
}

/// Refuses to create a new image, since images are replaced by Tools
///
/// # Arguments
///
/// * `user` - The user that is creating this image
/// * `state` - Shared Thorium objects
/// * `body` - The image that would have been created
#[utoipa::path(
    post,
    path = "/api/images/",
    request_body = ImageRequest,
    responses(
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 410, description = "Images can no longer be created; create a Tool instead"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::images::create", skip_all, err(Debug))]
async fn create(
    user: AuthedUser,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    // find the group the image would be created in, if the body names one
    let image_request = serde_json::from_slice::<ImageRequest>(&body).ok();
    let group = image_request
        .as_ref()
        .map_or_else(String::new, |req| req.group.clone());
    // image creates are gone
    image_writes_gone(&group)?;
    // create the image for tests that seed stored images
    let Some(image_request) = image_request else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            Some("invalid image request".to_owned()),
        ));
    };
    Image::create(&user, image_request, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Gets details about a specific image
///
/// # Arguments
///
/// * `user` - The user that is getting this images info
/// * `group` - The group this image is in
/// * `image` - The name of the image to get details about
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/images/data/{group}/{image}",
    params(
        ("group" = String, Path, description = "The group this image is in"),
        ("image" = String, Path, description = "The name of the image to get details about")
    ),
    responses(
        (status = 200, description = "Image details", body = Image),
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
#[instrument(name = "routes::images::get_image", skip_all, err(Debug))]
async fn get_image(
    user: AuthedUser,
    Path((group, image)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Image>, ApiError> {
    // get image
    let (_, image) = Image::get(&user, &group, &image, &state.shared).await?;
    Ok(Json(image))
}

/// Lists images in a group
///
/// # Arguments
///
/// * `user` - The user that is listing images
/// * `group` - The group to list images from
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/images/{group}/",
    params(
        ("group" = String, Path, description = "The group to list images from"),
        ImageListParams,
    ),
    responses(
        (status = 200, description = "Group images details", body = ImageList),
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
#[instrument(name = "routes::images::list", skip_all, err(Debug))]
async fn list(
    user: AuthedUser,
    Path(group): Path<String>,
    params: ImageListParams,
    State(state): State<AppState>,
) -> Result<Json<ImageList>, ApiError> {
    // authorize this user is apart of this group
    let group = Group::authorize(&user, &group, &state.shared).await?;
    // get image names
    let names = Image::list(&group, params.cursor, params.limit, &state.shared).await?;
    Ok(Json(names))
}

/// Lists images in a group with details
///
/// # Arguments
///
/// * `user` - The user that is listing images
/// * `group` - The group to list images from
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/images/{group}/details/",
    params(
        ("group" = String, Path, description = "The group to list images from"),
        ImageListParams,
    ),
    responses(
        (status = 200, description = "Group images details", body = ImageDetailsList),
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
#[axum_macros::debug_handler]
#[instrument(name = "routes::images::list_details", skip_all, err(Debug))]
async fn list_details(
    user: AuthedUser,
    Path(group): Path<String>,
    params: ImageListParams,
    State(state): State<AppState>,
) -> Result<Json<ImageDetailsList>, ApiError> {
    // authorize this user is apart of this group
    let group = Group::authorize(&user, &group, &state.shared).await?;
    // get a list of images
    let images = Image::list(&group, params.cursor, params.limit, &state.shared).await?;
    // transform this list into a details list
    let details = images.details(&group, &state.shared).await?;
    Ok(Json(details))
}

/// Refuses to update an image, since images are replaced by Tools
///
/// # Arguments
///
/// * `user` - The user that is updating this image
/// * `group` - The group to update an image from
/// * `image` - The name of the image to update
/// * `state` - Shared Thorium objects
/// * `body` - The update that would have been applied
#[utoipa::path(
    patch,
    path = "/api/images/{group}/{image}",
    params(
        ("group" = String, Path, description = "The group to update an image from"),
        ("image" = String, Path, description = "The name of the image to update"),
    ),
    request_body = ImageUpdate,
    responses(
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 410, description = "Images can no longer be updated; update a Tool instead"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::images::update", skip_all, err(Debug))]
async fn update(
    user: AuthedUser,
    Path((group, image)): Path<(String, String)>,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    // image updates are gone, including ones that only change bans
    image_writes_gone(&group)?;
    // parse the update for tests that seed stored images
    let update = serde_json::from_slice::<ImageUpdate>(&body)?;
    // get image
    let (group, image) = Image::get(&user, &group, &image, &state.shared).await?;
    // update the image in the backend
    image.update(update, &user, &group, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes an image
///
/// An image cannot be in use by any pipelines when it is deleted.
///
/// # Arguments
///
/// * `user` - The user that is deleting this image
/// * `group` - The group to delete an image from
/// * `image` - The name of the image to delete
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/images/{group}/{image}",
    params(
        ("group" = String, Path, description = "The group to delete an image from"),
        ("image" = String, Path, description = "The name of the image to delete")
    ),
    responses(
        (status = 204, description = "Image deleted"),
        (status = 401, description = "This user is not authorized to access this route. A run token is refused."),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 409, description = "The image is still used by one or more pipelines"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::images::delete_image", skip_all, err(Debug))]
async fn delete_image(
    user: AuthedUser,
    Path((group, image)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // a run token can't delete an image that other runs could use
    user.refuse_run_token("delete an image")?;
    // get image
    let (group, image) = Image::get(&user, &group, &image, &state.shared).await?;
    // delete image from backend
    image.delete(&user, &group, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Update the average runtimes for all images
///
/// # Arguments
///
/// * `user` - The user that is telling Thorium to update all image runtimes
/// * `state` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/images/runtimes/update",
    params(),
    responses(
        (status = 204, description = "Runtimes updated"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::images::runtimes_update", skip_all, err(Debug))]
async fn runtimes_update(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // crawl groups and calculate and update their images average runtimes
    Image::update_runtimes(&user, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Create a notification for an image in scylla
///
/// # Arguments
///
/// * `user` - The user sending the request
/// * `group` - The group the image is in
/// * `image` - The name of the image the notification will be created for
/// * `state` - Shared Thorium objects
/// * `params` - The notification params sent with the request
/// * `req` - The notification request that was sent
#[utoipa::path(
    post,
    path = "/api/images/notifications/{group}/{image}",
    params(
        ("group" = String, Path, description = "The group the image is in"),
        ("image" = String, Path, description = "The name of the image the notification will be created for"),
        NotificationParams,
    ),
    request_body = NotificationRequest<Image>,
    responses(
        (status = 204, description = "Notification created"),
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
#[instrument(name = "routes::images::create_notification", skip_all, err(Debug))]
async fn create_notification(
    user: AuthedUser,
    Path((group, image)): Path<(String, String)>,
    State(state): State<AppState>,
    params: NotificationParams,
    Json(req): Json<NotificationRequest<Image>>,
) -> Result<StatusCode, ApiError> {
    // only admins can create image notifications
    is_admin!(&user);
    // check that the image exists
    let (_, image) = Image::get(&user, &group, &image, &state.shared).await?;
    // generate the image's key
    let key = ImageKey::from(&image);
    // create the notification
    shared::notifications::create_notification(image, key, req, params, &state.shared).await
}

/// Get all of the image's notifications
///
/// # Arguments
///
/// * `user` - The user that is requesting the notifications
/// * `group` - The group the image is in
/// * `image` - The name of the image whose notifications are being requested
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/images/notifications/{group}/{image}",
    params(
        ("group" = String, Path, description = "The group the image is in"),
        ("image" = String, Path, description = "The name of the image whose notifications are being requested"),
    ),
    responses(
        (status = 200, description = "Notifications returned for image", body = Vec<ImageNotification>),
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
#[instrument(name = "routes::images::get_notifications", skip_all, err(Debug))]
async fn get_notifications(
    user: AuthedUser,
    Path((group, image)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Notification<Image>>>, ApiError> {
    // check that the image exists and the user has access
    let (_, image) = Image::get(&user, &group, &image, &state.shared).await?;
    // generate the image's key
    let key = ImageKey::from(&image);
    // get all of the image's notifications
    shared::notifications::get_notifications(image, key, &state.shared).await
}

/// Delete a specific notification from an image
///
/// # Arguments
///
/// * `user` - The user that is requesting the deletion
/// * `group` - The group the image is in
/// * `image` - The name of the image whose log is being deleted
/// * `id` - The notification's unique ID
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/images/notifications/{group}/{image}/{id}",
    params(
        ("group" = String, Path, description = "The group the image is in"),
        ("image" = String, Path, description = "The name of the image whose log is being deleted"),
        ("id" = Uuid, Path, description = "The notification's unique ID"),
    ),
    responses(
        (status = 204, description = "Notification deleted"),
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
#[instrument(name = "routes::images::delete_notification", skip_all, err(Debug))]
async fn delete_notification(
    user: AuthedUser,
    Path((group, image, id)): Path<(String, String, Uuid)>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // only admins can delete image logs
    is_admin!(&user);
    // check that the image exists
    let (_, image) = Image::get(&user, &group, &image, &state.shared).await?;
    // generate the image's key
    let key = ImageKey::from(&image);
    // delete the notification
    shared::notifications::delete_notification(image, key, None, id, &state.shared).await
}

/// The struct that marks the image routes which no longer write images as deprecated
///
/// Creating and updating an image always answers `410 Gone`, so the Swagger UI shows both routes as
/// removed instead of as usable routes.
pub struct RemovedImageWrites;

impl Modify for RemovedImageWrites {
    /// Mark the image create and update operations as deprecated
    ///
    /// # Arguments
    ///
    /// * `openapi` - The document to mark the removed operations in
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // the create route answers 410 for every request
        if let Some(operation) = openapi
            .paths
            .paths
            .get_mut("/api/images/")
            .and_then(|item| item.post.as_mut())
        {
            operation.deprecated = Some(Deprecated::True);
        }
        // the update route answers 410 for every request too
        if let Some(operation) = openapi
            .paths
            .paths
            .get_mut("/api/images/{group}/{image}")
            .and_then(|item| item.patch.as_mut())
        {
            operation.deprecated = Some(Deprecated::True);
        }
    }
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(create, get_image, list, list_details, update, delete_image, runtimes_update, get_notifications, create_notification, delete_notification),
    components(schemas(ArgStrategy, AutoTag, AutoTagLogic, AutoTagUpdate, ChildFilters, ChildFiltersUpdate, ChildrenDependencySettings, ChildrenDependencySettingsUpdate, Cleanup, CleanupUpdate, ConfigMap, Dependencies, DependenciesUpdate, DependencyPassStrategy, SampleDependencySettingsUpdate, RepoDependencySettingsUpdate, EphemeralDependencySettings, EphemeralDependencySettingsUpdate, FileSystemDependencySettings, FileSystemDependencySettingsUpdate, FilesHandler, FilesHandlerUpdate, GenericBan, HostPath, HostPathTypes, Image, ImageArgs, ImageArgsUpdate, ImageBan, ImageBanKind, ImageBanUpdate, ImageDetailsList, ImageLifetime, ImageList, ImageListParams, ImageNetworkPolicyUpdate, ImageRequest, ImageScaler, ImageUpdate, ToolVersion, InvalidHostPathBan, InvalidUrlBan, Kvm, KvmUpdate, KwargDependency, NFS, Notification<Image>, NotificationLevel, NotificationParams, NotificationRequest<Image>, OutputCollection, OutputCollectionUpdate, OutputDisplayType, OutputHandler, RepoDependencySettings, Resources, ResourcesRequest, ResourcesUpdate, ResultDependencySettings, ResultDependencySettingsUpdate, SampleDependencySettings, Secret, SecurityContext, SecurityContextUpdate, SpawnLimits, TagDependencySettings, TagDependencySettingsUpdate, Volume, VolumeTypes)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples, &RemovedImageWrites),
)]
pub struct ImageApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ImageApiDocs::openapi())
}

/// Add the results routes to our router
///
/// # Arguments
///
// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/images/", post(create))
        .route("/images/data/{group}/{image}", get(get_image))
        .route("/images/{group}/", get(list))
        .route("/images/{group}/details/", get(list_details))
        .route(
            "/images/{group}/{image}",
            patch(update).delete(delete_image),
        )
        .route("/images/runtimes/update", post(runtimes_update))
        .route(
            "/images/notifications/{group}/{image}",
            get(get_notifications).post(create_notification),
        )
        .route(
            "/images/notifications/{group}/{image}/{id}",
            delete(delete_notification),
        )
}
