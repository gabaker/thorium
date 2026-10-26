//! The search routes for Thorium

use axum::Router;
use axum::extract::{Json, State};
use axum::routing::get;
use tracing::instrument;
use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};
use crate::models::AuthedUser;
use crate::models::ElasticSearchParams;
use crate::models::backends;
use crate::models::elastic::ElasticIndex;
use crate::models::{ApiCursor, ElasticDoc};
use crate::utils::{ApiError, AppState};

pub mod events;

/// Search results in elastic and return a list of sha256s
///
/// # Arguments
///
/// * `user` - The user that is listing submissions
/// * `params` - The query params to use with this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/search/",
    params(
        ElasticSearchParams,
    ),
    responses(
        (status = 200, description = "Returned Elasticsearch docs matching search", body = ApiCursor<ElasticDoc>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A search index does not exist yet (the search streamer creates them), or a service Thorium depends on, such as Redis, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::search::search", skip_all, err(Debug))]
async fn search(
    user: AuthedUser,
    params: ElasticSearchParams,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<ElasticDoc>>, ApiError> {
    // get a list of elastic documents in the given indexes based on the given search
    let cursor = backends::search::search(&user, params, &state.shared).await?;
    Ok(Json(cursor))
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(search),
    components(schemas(ApiCursor<ElasticDoc>, ElasticDoc, ElasticIndex, ElasticSearchParams)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct SearchApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(SearchApiDocs::openapi())
}

// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    let router = router.route("/search/", get(search));
    // mount search events routes
    events::mount(router)
}
