//! The route for building a relationship tree out of data in Thorium

use axum::Router;
use axum::extract::{Json, Path, State};
use axum::routing::{get, post};
use tracing::instrument;
use utoipa::OpenApi;
use uuid::Uuid;

use super::{OpenApiParameterExamples, OpenApiSecurity};

use crate::models::AuthedUser;
use crate::models::{Tree, TreeGrowQuery, TreeParams, TreeQuery};
use crate::utils::{ApiError, AppState};

/// Start building a tree of data in Thorium from some starting points
///
/// # Arguments
///
/// * `user` - The user that is building a tree
/// * `params` - The params for building this tree
/// * `state` - Shared Thorium objects
/// * `query` - The query to use to start this tree
#[utoipa::path(
    post,
    path = "/api/trees/",
    params(
        TreeParams,
    ),
    request_body = TreeQuery,
    responses(
        (status = 200, description = "A new tree", body = Tree),
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
#[instrument(name = "routes::trees::start_tree", skip_all, err(Debug))]
async fn start_tree(
    user: AuthedUser,
    params: TreeParams,
    State(state): State<AppState>,
    Json(query): Json<TreeQuery>,
) -> Result<Json<Tree>, ApiError> {
    // build a tree from our params
    let (mut tree, bounds) = Tree::from_query(&user, query, &state.shared).await?;
    // grow this tree to the desired depth
    tree.grow(&user, params, bounds, &state.shared).await?;
    // save this tree
    tree.save(&user, &state.shared).await?;
    // clear any non user facing info
    tree.clear_non_user_facing();
    // return our built tree
    Ok(Json(tree))
}

/// Continue to grow a tree based on some growable nodes
///
/// # Arguments
///
/// * `user` - The user that is building a tree
/// * `params` - The params for building this tree
/// * `state` - Shared Thorium objects
/// * `query` - The growable nodes and bounds to grow this tree with
#[utoipa::path(
    patch,
    path = "/api/trees/{cursor}",
    params(
        ("cursor" = Uuid, Path, description = "The id of an existing tree to grow"),
        TreeParams,
    ),
    request_body = TreeGrowQuery,
    responses(
        (status = 200, description = "A tree grown from an existing tree", body = Tree),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation; a path parameter has an invalid value; a query parameter has an invalid value"),
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
#[instrument(name = "routes::trees::grow_tree", skip_all, err(Debug))]
async fn grow_tree(
    user: AuthedUser,
    params: TreeParams,
    Path(cursor): Path<Uuid>,
    State(state): State<AppState>,
    Json(query): Json<TreeGrowQuery>,
) -> Result<Json<Tree>, ApiError> {
    // load our existing tree
    let mut tree = Tree::load(&user, cursor, &state.shared).await?;
    // set our growable nodes
    tree.growable.clone_from(&query.growable);
    // grow this tree
    let added = tree
        .grow(&user, params, query.bounds, &state.shared)
        .await?;
    // save the latest info on this tree
    tree.save(&user, &state.shared).await?;
    // trim to only the new info for this tree
    tree.trim(&query.growable, &added).await;
    // clear any non user facing info
    tree.clear_non_user_facing();
    Ok(Json(tree))
}

/// Get an existing tree
///
/// # Arguments
///
/// * `user` - The user that is getting a tree
/// * `id` - The ID of the tree to get
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/trees/{cursor}",
    params(
        ("cursor" = Uuid, Path, description = "The id of the tree to get in its entirety"),
    ),
    responses(
        (status = 200, description = "A tree", body = Tree),
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
#[instrument(name = "routes::trees::get", skip_all, err(Debug))]
async fn get_tree(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Tree>, ApiError> {
    // load our existing tree
    let mut tree = Tree::load(&user, id, &state.shared).await?;
    // clear any non user facing info
    tree.clear_non_user_facing();
    // return our tree
    Ok(Json(tree))
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(start_tree, grow_tree, get_tree),
    components(schemas(Tree, TreeGrowQuery, TreeQuery)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct TreeApiDocs;

/// Add the tree routes to our router
///
/// # Arguments
///
/// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/trees/", post(start_tree))
        .route("/trees/{cursor}", get(get_tree).patch(grow_tree))
}
