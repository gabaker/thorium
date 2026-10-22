//! Routes for entities

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use axum_extra::body::AsyncReadBody;
use tracing::instrument;
use uuid::Uuid;

use super::shared::{self, graphics};
use crate::models::AuthedUser;
use crate::models::backends::entities::{is_definition_kind, models, toolsets, workflows};
use crate::models::backends::{self, GraphicSupport, TagSupport};
use crate::models::{
    ApiCursor, Association, AssociationListParams, AssociationTargetColumn, Entity, EntityListLine,
    EntityListParams, EntityMetadata, EntityResponse, GrepMatches, GrepQueryParams, GrepRequest,
    InstancedAgentToolset, ModelCredentialEntity, ModelEndpointEntity, Notification, RedactSecrets,
    TagDeleteRequest, TagRequest, ToolInstanceSummary,
};
use crate::utils::{ApiError, AppState};
use crate::{bad, not_found};

/// Refuse a run token a write to a definition
///
/// A run token is handed to every worker of a run, so it can't change the tools, toolsets, agents,
/// workflows, or model settings that other runs follow. Entities that hold data, tags, and
/// associations are not checked here, because harnesses write those about their inputs.
///
/// # Arguments
///
/// * `user` - The user making the request
/// * `entity` - The entity the request would write to
/// * `action` - What the request would do to it, used in the refusal message
fn refuse_run_token_on_definitions(
    user: &AuthedUser,
    entity: &Entity,
    action: &str,
) -> Result<(), ApiError> {
    // only writes to definitions are refused
    if is_definition_kind(&entity.kind) {
        user.refuse_run_token(&format!("{action} a {}", entity.kind))?;
    }
    Ok(())
}

/// Creates a new entity
///
/// # Arguments
///
/// * `user` - The user that is creating this entity
/// * `state` - Shared Thorium objects
/// * `form` - The multipart entity request
#[utoipa::path(
    post,
    path = "/api/entities/",
    responses(
        (status = 200, description = "Entity created", body = EntityResponse),
        (status = 401, description = "This user is not authorized to create entities in all given groups"),
        (status = 404, description = "One or more of the given groups does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::create", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn create(
    user: AuthedUser,
    State(state): State<AppState>,
    form: Multipart,
) -> Result<Json<EntityResponse>, ApiError> {
    // create the entity
    let resp = Box::pin(Entity::create(
        &user,
        user.run().is_some(),
        user.is_scoped(),
        form,
        &state.shared,
    ))
    .await?;
    Ok(Json(resp))
}

/// Gets an entity by id
///
/// The entity is returned without its secrets and without any snapshot groups the user can't see,
/// and a tool instance carries its portability report.
///
/// # Arguments
///
/// * `user` - The user that is getting this entity
/// * `state` - Shared Thorium objects
/// * `id` - The id of the entity to get
#[utoipa::path(
    get,
    path = "/api/entities/:id",
    params(
        ("id" = Uuid, Path, description = "The entity's id"),
    ),
    responses(
        (status = 200, description = "Entity details", body = Entity),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A entity with the given name (and ID) does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::get", skip_all, err(Debug))]
async fn get(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Entity>, ApiError> {
    // get the entity from the backend
    let mut entity = Entity::get(&user, id, &state.shared).await?;
    // never return a secret
    entity.redact_secrets();
    // never name a group this user can't see
    entity.hide_snapshot_groups(&user);
    // an agentic tool instance carries its computed portability report
    if let crate::models::EntityMetadata::ToolInstance(instance) = &mut entity.metadata {
        instance.fill_portability();
    }
    // return the entity as JSON
    Ok(Json(entity))
}

/// Lists entities by the given parameters
///
/// # Arguments
///
/// * `user` - The user that is listing entities
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
//#[utoipa::path(
//get,
//path = "/api/entities/",
//params(
//("params" = EntityListParams, description = "Query params to use for this entity list request"),
//),
//responses(
//(status = 200, description = "JSON-formatted cursor response containing the names and ID's of entities as well as their upload timestamps", body = ApiCursor<EntityListLine>),
//(status = 401, description = "This user is not authorized to access this route"),
//),
//security(
//("basic" = []),
//)
//)]
#[instrument(name = "routes::entities::list", skip_all, err(Debug))]
async fn list(
    user: AuthedUser,
    params: EntityListParams,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<EntityListLine>>, ApiError> {
    // list entities
    let cursor = Entity::list(&user, params, false, &state.shared).await?;
    // return the cursor
    Ok(Json(cursor))
}

/// Lists entities and their details by the given parameters
///
/// # Arguments
///
/// * `user` - The user that is listing entity details
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/details",
    params(
        ("params" = EntityListParams, description = "Query params to use for this entity list request"),
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing the entities' details", body = ApiCursor<Entity>),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::list_details", skip_all, err(Debug))]
async fn list_details(
    user: AuthedUser,
    params: EntityListParams,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<Entity>>, ApiError> {
    // list entities
    let list = Entity::list(&user, params, false, &state.shared).await?;
    // convert the list to a details list
    let mut cursor = list.details(&user, &state.shared).await?;
    // never return a secret or name a group this user can't see
    for entity in &mut cursor.data {
        entity.redact_secrets();
        entity.hide_snapshot_groups(&user);
    }
    // return the cursor
    Ok(Json(cursor))
}

/// Update an entity
///
/// # Arguments
///
/// * `user` - The user that is updating an entity
/// * `state` - Shared Thorium objects
/// * `id` - The id of the entity to update
/// * `form` - The multipart entity update request
#[utoipa::path(
    patch,
    path = "/api/entities/:id",
    params(
        ("id" = Uuid, Path, description = "The entity id"),
    ),
    responses(
        (status = 204, description = "Entity updated"),
        (status = 401, description = "This user is not authorized to edit this entity or the group does not allow editing entities"),
        (status = 404, description = "One or more of the given groups does not exist or the entity does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::update", skip_all, err(Debug))]
async fn update(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    form: Multipart,
) -> Result<StatusCode, ApiError> {
    // get the entity from the backend
    let entity = Entity::get(&user, id, &state.shared).await?;
    // a run token can't change a definition that other runs could follow
    refuse_run_token_on_definitions(&user, &entity, "change")?;
    // update the entity
    crate::utils::helpers::heap_future(|| {
        entity.update(&user, user.is_scoped(), form, &state.shared)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an entity by id
///
/// # Arguments
///
/// * `user` - The user that is updating an entity
/// * `request` - The entity update request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/entities/:id",
    params(
        ("id" = Uuid, Path, description = "The entity id"),
    ),
    responses(
        (status = 204, description = "Entity deleted"),
        (status = 401, description = "This user is not authorized to delete this entity or the group does not allow editing entities"),
        (status = 404, description = "One or more of the given groups does not exist or the entity does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::delete", skip_all, err(Debug))]
async fn delete(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // get the entity from the backend
    let entity = Entity::get(&user, id, &state.shared).await?;
    // a run token can't delete a tool or an instance that other runs could follow
    refuse_run_token_on_definitions(&user, &entity, "delete")?;
    // update the entity
    entity
        .delete(&user, user.is_scoped(), &state.shared)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Adds new tags to a entity
///
/// The tags Thorium sets on instances can't be set by users.
///
/// # Arguments
///
/// * `user` - The user that is adding new tags
/// * `id` - The id of the entity to add tags too
/// * `state` - Shared Thorium objects
/// * `tags` - The new tags to apply
#[utoipa::path(
    post,
    path = "/api/entities/tags/:id",
    params(
        ("id" = Uuid, Path, description = "The ID of the entity to add tags too"),
        ("tags" = TagRequest<Entity>, description = "JSON-formatted tags to apply to entity")
    ),
    responses(
        (status = 204, description = "entity tags updated"),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::tag", skip_all, err(Debug))]
async fn tag(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(tags): Json<TagRequest<Entity>>,
) -> Result<StatusCode, ApiError> {
    // get the entity we are adding tags too
    let entity = Entity::get(&user, id, &state.shared).await?;
    // a run token can't tag a definition, which could change the tools a toolset selects
    refuse_run_token_on_definitions(&user, &entity, "tag")?;
    // users can't set the tags Thorium sets on instances
    Entity::refuse_intrinsic_tag_keys(entity.kind, tags.tags.keys())?;
    // try to add the new tags for this entity
    entity.tag(&user, tags, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes tags from a entity
///
/// The tags Thorium sets on shared definitions can't be deleted by users.
///
/// # Arguments
///
/// * `user` - The user that is deleting tags
/// * `id` - The id of the entity to delete tags from
/// * `state` - Shared Thorium objects
/// * `tags_del` - The tags to delete and the groups to delete them from
#[utoipa::path(
    delete,
    path = "/api/entities/tags/:id",
    params(
        ("Id" = Uuid, Path, description = "The id for the entity to delete tags from"),
        ("tags_del" = TagDeleteRequest<Entity>, description = "JSON-formatted tags to delete")
    ),
    responses(
        (status = 204, description = "entity tags deleted"),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::files::delete_tags", skip_all, err(Debug))]
async fn delete_tags(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(tags_del): Json<TagDeleteRequest<Entity>>,
) -> Result<StatusCode, ApiError> {
    // get the entity we are deleting tags from
    let entity = Entity::get(&user, id, &state.shared).await?;
    // a run token can't untag a definition, which could change the tools a toolset selects
    refuse_run_token_on_definitions(&user, &entity, "untag")?;
    // tags Thorium sets on shared definitions can't be removed by users
    entity.protect_intrinsic_tags(&tags_del)?;
    // try to delete the tags for this entity
    entity.delete_tags(&user, tags_del, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Get an entity's image
///
/// # Arguments
///
/// * `user` - The user that is updating an entity
/// * `request` - The entity update request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    patch,
    path = "/api/entities/:id/image",
    params(
        ("id" = Uuid, Path, description = "The entity's ID")
    ),
    responses(
        (status = 200, description = "The image was successfully retrieved"),
        (status = 404, description = "The entity does not exist or the entity does not have an image"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(
    name = "routes::entities::get_image",
    skip(user, state),
    fields(user = user.username),
    err(Debug)
)]
#[axum_macros::debug_handler]
async fn get_image(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    // get our entity by id
    let entity = Entity::get(&user, id, &state.shared).await?;
    // check if this entity has a graphic
    match &entity.image {
        Some(image_path) => {
            // get our
            let get_object = entity.download_graphic(image_path, &state.shared).await?;
            // get headers for this image
            let headers = graphics::get_headers(&get_object, image_path);
            // convert the output body to a streamable body
            let body = AsyncReadBody::new(get_object.body.into_async_read());
            // stream our body with its headers back
            Ok((headers, body))
        }
        None => not_found!(format!("Entity with id '{id}' has no image")),
    }
}

/// List the associations for an entity
///
/// # Arguments
///
/// * `user` - The user that is listing associations
/// * `params` - The query params to use for this request
/// * `id` - The id of the entity to list associations for
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/associations/{id}",
    params(
        ("id" = Uuid, Path, description = "The id of the entity to list associations for"),
        ("params" = AssociationListParams, description = "Query params to use for this association list request"),
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing the associations for an entity", body = ApiCursor<Association>),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The entity does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::list_associations", skip_all, err(Debug))]
async fn list_associations(
    user: AuthedUser,
    params: AssociationListParams,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<Association>>, ApiError> {
    // make sure this user can access this entity
    Entity::get(&user, id, &state.shared).await?;
    // create the source to list associations from
    let source = AssociationTargetColumn::Entity(id);
    // list the associations for this entity
    let cursor = Association::list(&user, params, &source, &state.shared).await?;
    Ok(Json(cursor))
}

/// Search a page of entities with grep
///
/// This searches every entity this user can see, so it returns a cursor that can be
/// continued if more entities still need to be searched.
///
/// # Arguments
///
/// * `user` - The user that is grepping these entities
/// * `params` - The query params for this grep search
/// * `state` - Shared Thorium objects
/// * `request` - The grep request body containing the regex pattern
#[instrument(name = "routes::entities::grep", skip_all, err(Debug))]
async fn grep(
    user: AuthedUser,
    params: GrepQueryParams,
    State(state): State<AppState>,
    Json(request): Json<GrepRequest>,
) -> Result<Json<ApiCursor<GrepMatches>>, ApiError> {
    // combine our query params and pattern into the full grep params
    let params = params.into_params(request);
    // grep a page of entities
    let matches = backends::grep::grep_many::<Entity>(&user, params, &state.shared).await?;
    Ok(Json(matches))
}

/// Search a single entity with grep
///
/// # Arguments
///
/// * `user` - The user that is grepping this entity
/// * `id` - The id of the entity to grep
/// * `params` - The query params for this grep search
/// * `state` - Shared Thorium objects
/// * `request` - The grep request body containing the regex pattern
#[instrument(name = "routes::entities::grep_entity", skip_all, err(Debug))]
async fn grep_entity(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    params: GrepQueryParams,
    State(state): State<AppState>,
    Json(request): Json<GrepRequest>,
) -> Result<Json<GrepMatches>, ApiError> {
    // combine our query params and pattern into the full grep params
    let params = params.into_params(request);
    // grep this single entity
    let matches =
        backends::grep::grep_one::<Entity>(&user, &id.to_string(), params, &state.shared).await?;
    Ok(Json(matches))
}

/// List every association of an entity in both directions
///
/// Only admins can list an entity's associations this way, and this is meant for tests.
///
/// # Arguments
///
/// * `user` - The user that is listing associations
/// * `id` - The id of the entity to list associations for
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/{id}/associations",
    params(
        ("id" = Uuid, Path, description = "The entity id"),
    ),
    responses(
        (status = 200, description = "Every association of the entity", body = Vec<Association>),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The entity does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::associations", skip_all, err(Debug))]
async fn associations(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Association>>, ApiError> {
    // list every association of this entity as an admin
    let associations = Entity::admin_associations(&user, id, &state.shared).await?;
    Ok(Json(associations))
}

/// Get the ban notifications of a model entity
///
/// Entities of other kinds have no ban notifications, so they return an empty list.
///
/// # Arguments
///
/// * `user` - The user that is reading the notifications
/// * `id` - The id of the entity
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/{id}/notifications",
    params(
        ("id" = Uuid, Path, description = "The entity's id"),
    ),
    responses(
        (status = 200, description = "The entity's ban notifications"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The entity does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::notifications", skip_all, err(Debug))]
async fn notifications(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    // the entity must exist and be visible to this user
    let entity = Entity::get(&user, id, &state.shared).await?;
    // notifications are kept under the entity's id
    let key = entity.id.to_string();
    let shared = &state.shared;
    // read them under the entity's kind
    let response = match entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => {
            shared::notifications::get_notifications(meta, key, shared)
                .await?
                .into_response()
        }
        EntityMetadata::ModelProfile(meta) => {
            shared::notifications::get_notifications(meta, key, shared)
                .await?
                .into_response()
        }
        EntityMetadata::ModelCredential(meta) => {
            shared::notifications::get_notifications(meta, key, shared)
                .await?
                .into_response()
        }
        _ => Json(Vec::<Notification<ModelEndpointEntity>>::new()).into_response(),
    };
    Ok(response)
}

/// List a tool's instances, newest first, including deprecated instances
///
/// # Arguments
///
/// * `user` - The user that is listing the instances
/// * `id` - The id of the tool
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/tools/{id}/instances",
    params(
        ("id" = Uuid, Path, description = "The tool's id"),
    ),
    responses(
        (status = 200, description = "The tool's instances, newest first", body = Vec<ToolInstanceSummary>),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The tool does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::tool_instances", skip_all, err(Debug))]
async fn tool_instances(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<ToolInstanceSummary>>, ApiError> {
    // get the tool, which fails with a 404 if the user can't see it
    let tool = Entity::get(&user, id, &state.shared).await?;
    // a tool lists its instances when it's read
    match tool.metadata {
        crate::models::EntityMetadata::Tool(meta) => Ok(Json(meta.instances)),
        _ => not_found!(format!("Tool {id} not found")),
    }
}

/// The query params of a workflow instance's toolset read
#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
struct ToolsetParams {
    /// The agent in the instance whose toolset to read
    agent: Option<Uuid>,
}

/// Get one agent of a workflow instance and the stored spec of each tool it was given
///
/// # Arguments
///
/// * `user` - The user reading the toolset
/// * `id` - The id of the workflow instance
/// * `params` - The agent whose toolset to read
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/workflows/instances/{id}/toolset",
    params(
        ("id" = Uuid, Path, description = "The workflow instance's id"),
        ToolsetParams,
    ),
    responses(
        (status = 200, description = "The agent and its tools' stored specs", body = InstancedAgentToolset),
        (status = 400, description = "No agent was named"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The workflow instance or the agent in it does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::workflow_toolset", skip_all, err(Debug))]
async fn workflow_toolset(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ToolsetParams>,
    State(state): State<AppState>,
) -> Result<Json<InstancedAgentToolset>, ApiError> {
    // the agent must be named
    let Some(agent) = params.agent else {
        return bad!("a toolset read needs an agent".to_owned());
    };
    // read the agent's snapshot and its tools' stored specs
    let toolset = workflows::agent_toolset(&user, id, agent, &state.shared).await?;
    Ok(Json(toolset))
}

/// Request a new instance of a workflow, which the event handler builds
///
/// # Arguments
///
/// * `user` - The user requesting the instance
/// * `id` - The id of the workflow
/// * `state` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/entities/workflows/{id}/instance",
    params(
        ("id" = Uuid, Path, description = "The workflow's id"),
    ),
    responses(
        (status = 202, description = "The instance request was queued"),
        (status = 401, description = "This user can't edit the workflow in every one of its groups"),
        (status = 404, description = "The workflow does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::entities::request_instance", skip_all, err(Debug))]
async fn request_instance(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // a run token can't make the workflow instance that later runs follow
    user.refuse_run_token("request a workflow instance")?;
    // queue the request for an instance
    workflows::request_instance(&user, id, &state.shared).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Request a new instance of a toolset, which the event handler builds
///
/// # Arguments
///
/// * `user` - The user requesting the instance
/// * `id` - The id of the toolset
/// * `state` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/entities/toolsets/{id}/instance",
    params(
        ("id" = Uuid, Path, description = "The toolset's id"),
    ),
    responses(
        (status = 202, description = "The instance request was queued"),
        (status = 401, description = "This user can't edit the toolset in every one of its groups"),
        (status = 404, description = "The toolset does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(
    name = "routes::entities::request_tool_set_instance",
    skip_all,
    err(Debug)
)]
async fn request_tool_set_instance(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // a run token can't make the toolset instance that later runs follow
    user.refuse_run_token("request a toolset instance")?;
    // queue the request for an instance
    toolsets::request_instance(&user, id, &state.shared).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Request a refresh of a model endpoint's catalog, which the event handler lists
///
/// # Arguments
///
/// * `user` - The user requesting the refresh
/// * `id` - The id of the model endpoint
/// * `state` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/entities/models/endpoints/{id}/refresh",
    params(
        ("id" = Uuid, Path, description = "The model endpoint's id"),
    ),
    responses(
        (status = 202, description = "The refresh was queued"),
        (status = 400, description = "The endpoint isn't on the deployment's allowed endpoint list"),
        (status = 401, description = "This user doesn't own or manage any of the endpoint's groups"),
        (status = 404, description = "The model endpoint does not exist in the user's groups"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(
    name = "routes::entities::refresh_model_endpoint",
    skip_all,
    err(Debug)
)]
async fn refresh_model_endpoint(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // a run token can't make the event handler call a provider
    user.refuse_run_token("refresh a model endpoint")?;
    // mark the endpoint pending and queue its refresh
    models::request_refresh(&user, id, &state.shared).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Read a group model credential with its secrets, which only admins may do
///
/// # Arguments
///
/// * `user` - The user reading the credential
/// * `id` - The id of the model credential
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/models/credentials/{id}/secret",
    params(
        ("id" = Uuid, Path, description = "The model credential's id"),
    ),
    responses(
        (status = 200, description = "The credential with its secrets", body = ModelCredentialEntity),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The model credential does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(
    name = "routes::entities::model_credential_secret",
    skip_all,
    err(Debug)
)]
async fn model_credential_secret(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<ModelCredentialEntity>, ApiError> {
    // only a fully authed admin reads secrets
    let user = user.require_full()?;
    let credential = models::credential_secret(&user, id, &state.shared).await?;
    Ok(Json(credential))
}

/// Add the entities routes to our router
///
/// # Arguments
///
/// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/entities/", post(create))
        .route("/entities/grep/", post(grep))
        .route("/entities/grep/{id}", post(grep_entity))
        .route("/entities/", axum::routing::get(list))
        .route("/entities/details/", axum::routing::get(list_details))
        .route(
            "/entities/{id}",
            axum::routing::get(get).patch(update).delete(delete),
        )
        .route("/entities/{id}/image", axum::routing::get(get_image))
        .route(
            "/entities/{id}/associations",
            axum::routing::get(associations),
        )
        .route(
            "/entities/{id}/notifications",
            axum::routing::get(notifications),
        )
        .route("/entities/tags/{id}", post(tag).delete(delete_tags))
        .route(
            "/entities/associations/{id}",
            axum::routing::get(list_associations),
        )
        .route(
            "/entities/tools/{id}/instances",
            axum::routing::get(tool_instances),
        )
        .route(
            "/entities/workflows/instances/{id}/toolset",
            axum::routing::get(workflow_toolset),
        )
        .route("/entities/workflows/{id}/instance", post(request_instance))
        .route(
            "/entities/toolsets/{id}/instance",
            post(request_tool_set_instance),
        )
        .route(
            "/entities/models/endpoints/{id}/refresh",
            post(refresh_model_endpoint),
        )
        .route(
            "/entities/models/credentials/{id}/secret",
            axum::routing::get(model_credential_secret),
        )
}
