//! The routes related to associations

use axum::Router;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::routing::post;
use tracing::instrument;
use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};

use crate::models::AuthedUser;
use crate::models::{AssociationDeleteRequest, AssociationKind, AssociationRequest};
use crate::utils::{ApiError, AppState};

/// Refuse a run token a write to the edges that make up the agentic graph
///
/// The edges between agents, tools, and toolsets decide what the workflows other runs follow are
/// built from, so a run token can't write them. Every other kind of association stays open,
/// because harnesses write those about their inputs.
///
/// # Arguments
///
/// * `user` - The user making the request
/// * `kind` - The kind of association the request writes
/// * `action` - What the request would do to it, used in the refusal message
fn refuse_run_token_on_graph_edges(
    user: &AuthedUser,
    kind: AssociationKind,
    action: &str,
) -> Result<(), ApiError> {
    // only writes to graph edges are refused
    if kind.is_graph_edge() {
        user.refuse_run_token(&format!("{action} a {kind:?} association"))?;
    }
    Ok(())
}

/// Associate an entity or object with another entity/object
///
/// # Arguments
///
/// * `user` - The user that is creating associations
/// * `state` - Shared Thorium objects
/// * `req` - The association request to apply
#[utoipa::path(
    post,
    path = "/api/associations/",
    request_body = AssociationRequest,
    responses(
        (status = 204, description = "Associations created"),
        (status = 400, description = "The request was invalid, such as a kind that must be created through its own field"),
        (status = 401, description = "This user is not authorized to create these associations. A run token is refused for the graph-edge kinds"),
        (status = 404, description = "The source or a target does not exist"),
        (status = 409, description = "The association would break a graph or integrity rule"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::associations::create", skip_all, err(Debug))]
async fn create(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(req): Json<AssociationRequest>,
) -> Result<StatusCode, ApiError> {
    // a run token can't change the graph that workflows are built from
    refuse_run_token_on_graph_edges(&user, req.kind, "create")?;
    // apply this association request to the requested data
    req.apply(&user, &state.shared).await?;
    // if this request was successful then always return a 204
    Ok(StatusCode::NO_CONTENT)
}

/// Delete associations between an entity or object and other entities/objects
///
/// # Arguments
///
/// * `user` - The user that is deleting associations
/// * `state` - Shared Thorium objects
/// * `req` - The association delete request to apply
#[utoipa::path(
    delete,
    path = "/api/associations/",
    request_body = AssociationDeleteRequest,
    responses(
        (status = 204, description = "Associations deleted"),
        (status = 400, description = "No targets were set"),
        (status = 401, description = "This user is not authorized to delete these associations. A run token is refused for the graph-edge kinds"),
        (status = 404, description = "The source does not exist or no matching associations were found"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::associations::delete", skip_all, err(Debug))]
async fn delete(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(req): Json<AssociationDeleteRequest>,
) -> Result<StatusCode, ApiError> {
    // a run token can't change the graph that workflows are built from
    refuse_run_token_on_graph_edges(&user, req.kind, "delete")?;
    // delete the associations matching this request
    req.apply(&user, &state.shared).await?;
    // if this request was successful then always return a 204
    Ok(StatusCode::NO_CONTENT)
}

/// Add the associations routes to our router
///
/// # Arguments
///
// * `router` - The router to add routes too
/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(create, delete),
    components(schemas(AssociationDeleteRequest, AssociationRequest)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct AssociationApiDocs;

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route("/associations/", post(create).delete(delete))
}
