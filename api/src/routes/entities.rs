//! Routes for entities

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use axum_extra::body::AsyncReadBody;
use tracing::instrument;
use utoipa::OpenApi;
use uuid::Uuid;

use super::shared::{self, graphics};
use super::{OpenApiParameterExamples, OpenApiSecurity};
use crate::models::AuthedUser;
use crate::models::backends::entities::{is_definition_kind, models, toolsets, workflows};
use crate::models::backends::{self, GraphicSupport, TagSupport};
use crate::models::{
    ApiCursor, Association, AssociationListParams, AssociationTargetColumn, Entity,
    EntityByNameParams, EntityListLine, EntityListParams, EntityMetadata, EntityResponse,
    GrepDocumentMatches, GrepMatch, GrepMatches, GrepQueryParams, GrepRequest, GrepSource,
    GrepStats, HuntCheckpoint, HuntCheckpointResponse, HuntCreateRequest, HuntRuleLine,
    HuntRuleListParams, HuntStatusParams, HuntSystemStatus, InstancedAgentToolset,
    ModelCredentialEntity, ModelEndpointEntity, Notification, NotificationParams,
    NotificationRequest, RedactSecrets, TagDeleteRequest, TagRequest, ToolInstanceSummary,
    TruncationReason, YaraHitWriteRequest, YaraHitWriteResponse,
};
use crate::utils::{ApiError, AppState};
use crate::{bad, is_admin, not_found};

/// A notification for an entity as the documents describe it
pub type EntityNotification = Notification<Entity>;

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
    request_body(
        content = Vec<u8>,
        content_type = "multipart/form-data",
        description = "The multipart form describing the new entity: its name, kind, groups, description, tags, a `metadata` section for the kind's own fields, and an optional image",
    ),
    responses(
        (status = 200, description = "Entity created", body = EntityResponse),
        (status = 400, description = "The request is invalid: the multipart form is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to create entities in all given groups. A run token is refused when the entity is a definition."),
        (status = 404, description = "One or more of the given groups does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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

/// Writes one yara rule hit from authoritative server side state
///
/// The server recomputes the hit write groups from fresh database state (the
/// rules groups, the samples groups, and group kill switches) and performs the
/// whole partitioned write — entity, associations, depth stamped tags, and
/// auto flag actions — in one audited idempotent call. A request carrying a
/// hunt id also decorates the written hit with a `HuntHit` association and a
/// `HuntId` entity tag; a vanished hunt is a reported skip, never a 404.
///
/// # Arguments
///
/// * `user` - The user that is writing this hit
/// * `state` - Shared Thorium objects
/// * `req` - The hit write request
#[utoipa::path(
    post,
    path = "/api/entities/yara/hit/",
    responses(
        (status = 200, description = "Yara hit write outcome", body = YaraHitWriteResponse),
        (status = 400, description = "The hit write request was malformed"),
        (status = 401, description = "This user is not authorized to write yara hits"),
        (status = 404, description = "The target yara rule does not exist"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::entities::create_yara_hit", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn create_yara_hit(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(req): Json<YaraHitWriteRequest>,
) -> Result<Json<YaraHitWriteResponse>, ApiError> {
    // write this hit from authoritative server side state
    let resp = Entity::create_yara_hit(&user, req, &state.shared).await?;
    Ok(Json(resp))
}

/// Creates a hunt: freeze its rules, split its lanes, and start its chains
///
/// Any user may create hunts. The selector resolves and freezes under the
/// creator's authorization through the engine's snapshot filter, the corpus
/// clamps to the creation instant, best-effort per user and global active
/// hunt quotas apply, and one depth 0 chunk event per lane lands on the
/// engine's queue.
///
/// # Arguments
///
/// * `user` - The user that is creating this hunt
/// * `state` - Shared Thorium objects
/// * `req` - The hunt create request
#[utoipa::path(
    post,
    path = "/api/entities/hunt/",
    responses(
        (status = 200, description = "The created hunt entity", body = Entity),
        (status = 400, description = "The hunt create request was invalid (unshipped engine, Repos corpus, bad groups, quota, selector resolution)"),
        (status = 401, description = "This user is not authorized to create entities in all given groups"),
        (status = 404, description = "A selected rule or rule set does not exist"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::entities::create_hunt", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn create_hunt(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(req): Json<HuntCreateRequest>,
) -> Result<Json<Entity>, ApiError> {
    // create this hunt and start its lane chains
    let entity = Entity::create_hunt(&user, req, &state.shared).await?;
    Ok(Json(entity))
}

/// Cancels a hunt
///
/// Only the hunt's submitter or an admin may cancel it. Running chunks observe
/// the terminal status lazily within one chunk. Cancelling an already
/// cancelled hunt is an idempotent no-op; a hunt that completed or failed on
/// its own is a 400.
///
/// # Arguments
///
/// * `user` - The user that is cancelling this hunt
/// * `state` - Shared Thorium objects
/// * `id` - The id of the hunt to cancel
#[utoipa::path(
    post,
    path = "/api/entities/hunt/{id}/cancel",
    params(
        ("id" = Uuid, Path, description = "The id of the hunt to cancel"),
    ),
    responses(
        (status = 204, description = "Hunt cancelled"),
        (status = 400, description = "The entity is not a hunt or is already terminal"),
        (status = 401, description = "This user is not the hunts submitter or an admin"),
        (status = 404, description = "The hunt does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::entities::cancel_hunt", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn cancel_hunt(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // cancel this hunt
    Entity::cancel_hunt(&user, id, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Commits one processed hunt chunk
///
/// Only the admin service account may checkpoint hunts; this is the single
/// audited place hunt progress commits. The lane sequence compare-and-set and
/// the next chunk emission run in one atomic step so duplicated checkpoints
/// collapse instead of forking the lane chain; a lost compare-and-set is a 409
/// telling the caller to drop its stale chain.
///
/// # Arguments
///
/// * `user` - The user committing this checkpoint
/// * `state` - Shared Thorium objects
/// * `id` - The id of the hunt this checkpoint advances
/// * `req` - The checkpoint to commit
#[utoipa::path(
    post,
    path = "/api/entities/hunt/{id}/checkpoint",
    params(
        ("id" = Uuid, Path, description = "The id of the hunt this checkpoint advances"),
    ),
    responses(
        (status = 200, description = "The checkpoint committed", body = HuntCheckpointResponse),
        (status = 400, description = "The entity is not a hunt or the lane index is out of range"),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The hunt does not exist"),
        (status = 409, description = "The checkpoint lost its sequence compare-and-set"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::entities::checkpoint_hunt", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn checkpoint_hunt(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<HuntCheckpoint>,
) -> Result<Json<HuntCheckpointResponse>, ApiError> {
    // commit this hunt chunk checkpoint
    let resp = Entity::checkpoint_hunt(&user, id, req, &state.shared).await?;
    Ok(Json(resp))
}

/// List a page of a hunt's frozen rule snapshot
///
/// Any user who can see the hunt may list its frozen rules. Rules that were
/// deleted after the freeze, or that the caller cannot see, come back as id
/// only lines; the two cases are indistinguishable on purpose. The cursor is
/// an index into the immutable frozen rule vec encoded as a Uuid, so cursors
/// never expire.
///
/// # Arguments
///
/// * `user` - The user listing this hunts frozen rules
/// * `params` - The query params to use for this request
/// * `id` - The id of the hunt to list frozen rules for
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/hunt/{id}/rules",
    params(
        ("id" = Uuid, Path, description = "The id of the hunt to list frozen rules for"),
        HuntRuleListParams,
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing one page of the hunt's frozen rules", body = ApiCursor<HuntRuleLine>),
        (status = 400, description = "The entity is not a hunt or a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The hunt does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::entities::list_hunt_rules", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn list_hunt_rules(
    user: AuthedUser,
    params: HuntRuleListParams,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<HuntRuleLine>>, ApiError> {
    // list a page of this hunts frozen rules
    let cursor = Entity::list_hunt_rules(&user, id, params, &state.shared).await?;
    Ok(Json(cursor))
}

/// Get the admin hunt census for the status page
///
/// The census is cross-group, cross-user data (hunt names, submitters, and
/// per-user usage) so it is admin only; the gate runs in the backend before
/// any read.
///
/// # Arguments
///
/// * `user` - The user that is reading the census
/// * `params` - The query params bounding the census
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/hunt/status/",
    params(
        HuntStatusParams,
    ),
    responses(
        (status = 200, description = "The admin hunt census", body = HuntSystemStatus),
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
#[instrument(name = "routes::entities::hunt_system_status", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn hunt_system_status(
    user: AuthedUser,
    params: HuntStatusParams,
    State(state): State<AppState>,
) -> Result<Json<HuntSystemStatus>, ApiError> {
    // assemble the admin hunt census
    let status = Entity::hunt_system_status(&user, params, &state.shared).await?;
    Ok(Json(status))
}

/// Exempt a yara rule from noisy/slow quarantine
///
/// Only admins may manage exemptions. Setting an exemption also deletes any
/// Noisy/Slow `YaraRuleStatus` values so the rule rejoins scanning in the
/// same action; Broken tagging is unaffected since a rule that no longer
/// compiles cannot scan regardless.
///
/// # Arguments
///
/// * `user` - The user exempting this rule
/// * `state` - Shared Thorium objects
/// * `id` - The id of the yara rule to exempt
#[utoipa::path(
    post,
    path = "/api/entities/rules/{id}/quarantine-exemption",
    params(
        ("id" = Uuid, Path, description = "The id of the yara rule to exempt from quarantine"),
    ),
    responses(
        (status = 204, description = "The rule is now exempt from quarantine"),
        (status = 400, description = "The entity is not a yara rule"),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The rule does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(
    name = "routes::entities::set_quarantine_exemption",
    skip_all,
    err(Debug)
)]
#[axum_macros::debug_handler]
async fn set_quarantine_exemption(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // get this rule under the requesting users visibility
    let entity = Entity::get(&user, id, &state.shared).await?;
    // set the exemption flag (admin only, enforced in the backend)
    entity
        .set_quarantine_exemption(&user, true, &state.shared)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Clear a yara rules quarantine exemption
///
/// Only admins may manage exemptions. Clearing an exemption only removes the
/// rule's protection from future quarantine; it never adds or removes any
/// `YaraRuleStatus` values.
///
/// # Arguments
///
/// * `user` - The user clearing this rules exemption
/// * `state` - Shared Thorium objects
/// * `id` - The id of the yara rule to clear the exemption from
#[utoipa::path(
    delete,
    path = "/api/entities/rules/{id}/quarantine-exemption",
    params(
        ("id" = Uuid, Path, description = "The id of the yara rule to clear the exemption from"),
    ),
    responses(
        (status = 204, description = "The rule is no longer exempt from quarantine"),
        (status = 400, description = "The entity is not a yara rule"),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The rule does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(
    name = "routes::entities::clear_quarantine_exemption",
    skip_all,
    err(Debug)
)]
#[axum_macros::debug_handler]
async fn clear_quarantine_exemption(
    user: AuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // get this rule under the requesting users visibility
    let entity = Entity::get(&user, id, &state.shared).await?;
    // clear the exemption flag (admin only, enforced in the backend)
    entity
        .set_quarantine_exemption(&user, false, &state.shared)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Gets an entity by its id
///
/// The entity is returned without its secrets and without any snapshot groups the user can't see,
/// and a tool instance carries its portability report.
///
/// # Arguments
///
/// * `user` - The user that is getting this entity
/// * `id` - The id of the entity to get
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/{id}",
    params(
        ("id" = Uuid, Path, description = "The entity's id"),
    ),
    responses(
        (status = 200, description = "Entity details", body = Entity),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A entity with the given name (and ID) does not exist in the user's groups"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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

/// Get every entity sharing an exact name that this user can see
///
/// Matches come back ordered oldest-created first and an unknown name is just
/// an empty list rather than a 404 since clients distinguish the zero, one,
/// and many match cases themselves. More than 500 entities sharing one name is
/// rejected as too ambiguous to resolve by name.
///
/// # Arguments
///
/// * `user` - The user that is getting entities by name
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/by-name/",
    params(
        EntityByNameParams,
    ),
    responses(
        (status = 200, description = "JSON-formatted list of every visible entity sharing the given name ordered oldest-created first", body = Vec<Entity>),
        (status = 400, description = "The name param is missing or more than 500 entities share the name"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::entities::get_by_name", skip_all, err(Debug))]
#[axum_macros::debug_handler]
async fn get_by_name(
    user: AuthedUser,
    params: EntityByNameParams,
    State(state): State<AppState>,
) -> Result<Json<Vec<Entity>>, ApiError> {
    // get every visible entity sharing this exact name
    let mut entities =
        Entity::get_by_name(&user, &params.name, &params.kinds, &state.shared).await?;
    // never return a secret or name a group this user can't see
    for entity in &mut entities {
        entity.redact_secrets();
        entity.hide_snapshot_groups(&user);
    }
    // return the matched entities as JSON
    Ok(Json(entities))
}

/// Lists entities by the given parameters
///
/// # Arguments
///
/// * `user` - The user that is listing entities
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/",
    params(
        EntityListParams,
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing the names and ids of entities as well as their upload timestamps", body = ApiCursor<EntityListLine>),
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
    path = "/api/entities/details/",
    params(
        EntityListParams,
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing the entities' details", body = ApiCursor<Entity>),
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
    path = "/api/entities/{id}",
    params(
        ("id" = Uuid, Path, description = "The entity id"),
    ),
    request_body(
        content = Vec<u8>,
        content_type = "multipart/form-data",
        description = "The multipart form with the fields to change: name, description, metadata, groups to add or remove, `clear_*` flags, and an optional image",
    ),
    responses(
        (status = 204, description = "Entity updated"),
        (status = 400, description = "The request is invalid: the multipart form is malformed or fails validation; a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to edit this entity or the group does not allow editing entities. A run token is refused when the entity is a definition."),
        (status = 404, description = "One or more of the given groups does not exist or the entity does not exist"),
        (status = 409, description = "The update conflicts with how other entities use this one, for example it removes a group that referrers still need"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
    path = "/api/entities/{id}",
    params(
        ("id" = Uuid, Path, description = "The entity id"),
    ),
    responses(
        (status = 204, description = "Entity deleted"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to delete this entity or the group does not allow editing entities. A run token is refused when the entity is a definition, and a scoped token must hold every one of the entity's groups."),
        (status = 404, description = "One or more of the given groups does not exist or the entity does not exist"),
        (status = 409, description = "The entity is still in use: a tool still has tool instances, a model endpoint or credential is still referenced, or an entity, pipeline, or tool spec still references it (the message names what the caller can see)"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
    path = "/api/entities/tags/{id}",
    params(
        ("id" = Uuid, Path, description = "The ID of the entity to add tags too"),
    ),
    request_body = TagRequest<Entity>,
    responses(
        (status = 204, description = "entity tags updated"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation; a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route. A run token is refused when the entity is a definition kind"),
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
    path = "/api/entities/tags/{id}",
    params(
        ("id" = Uuid, Path, description = "The id for the entity to delete tags from"),
    ),
    request_body = TagDeleteRequest<Entity>,
    responses(
        (status = 204, description = "entity tags deleted"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation; a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route. A run token is refused when the entity is a definition kind"),
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
    get,
    path = "/api/entities/{id}/image",
    params(
        ("id" = Uuid, Path, description = "The entity's ID")
    ),
    responses(
        (status = 200, description = "The image was successfully retrieved"),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The entity does not exist or the entity does not have an image"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        AssociationListParams,
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing the associations for an entity", body = ApiCursor<Association>),
        (status = 400, description = "The request is invalid: a path parameter has an invalid value; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The entity does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
#[utoipa::path(
    post,
    path = "/api/entities/grep/",
    params(
        GrepQueryParams,
    ),
    request_body = GrepRequest,
    responses(
        (status = 200, description = "A page of grep matches, with a cursor to continue searching entities that were not reached", body = ApiCursor<GrepMatches>),
        (status = 400, description = "The request is invalid: the pattern is not a valid regex; the JSON body is malformed; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The cursor or a group named in the request does not exist or is not visible to this user"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
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
#[utoipa::path(
    post,
    path = "/api/entities/grep/{id}",
    params(
        ("id" = Uuid, Path, description = "The id of the entity to grep"),
        GrepQueryParams,
    ),
    request_body = GrepRequest,
    responses(
        (status = 200, description = "The grep matches for this entity", body = GrepMatches),
        (status = 400, description = "The request is invalid: the pattern is not a valid regex; a path parameter has an invalid value; the JSON body is malformed; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The entity does not exist or is not visible to this user"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
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
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The entity does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The entity does not exist in the user's groups"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The tool does not exist in the user's groups"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
#[into_params(parameter_in = Query)]
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
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user can't edit the workflow in every one of its groups. A run token is refused."),
        (status = 404, description = "The workflow does not exist in the user's groups"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user can't edit the toolset in every one of its groups. A run token is refused."),
        (status = 404, description = "The toolset does not exist in the user's groups"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        (status = 401, description = "This user doesn't own or manage any of the endpoint's groups. A run token is refused"),
        (status = 404, description = "The model endpoint does not exist in the user's groups"),
        (status = 409, description = "Another request kept the endpoint locked for longer than the 20 seconds this request waits; try again"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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
        (status = 400, description = "The request is invalid: a path parameter has an invalid value"),
        (status = 401, description = "This user is not an admin. Only a full token may use this route: a scoped token, including a run token, is refused."),
        (status = 404, description = "The model credential does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
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

/// Create a notification for an entity in scylla
///
/// # Arguments
///
/// * `user` - The user sending the request
/// * `id` - The id of the entity the notification will be created for
/// * `state` - Shared Thorium objects
/// * `params` - The notification params sent with the request
/// * `req` - The notification request that was sent
#[utoipa::path(
    post,
    path = "/api/entities/notifications/{id}",
    params(
        ("id" = Uuid, Path, description = "The id of the entity the notification will be created for"),
        NotificationParams,
    ),
    request_body = NotificationRequest<Entity>,
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
#[instrument(name = "routes::entities::create_notification", skip_all, err(Debug))]
async fn create_notification(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    params: NotificationParams,
    Json(req): Json<NotificationRequest<Entity>>,
) -> Result<StatusCode, ApiError> {
    // only admins can create entity notifications
    is_admin!(&user);
    // check that the entity exists
    let entity = Entity::get(&user, id, &state.shared).await?;
    // generate the entity's key
    let key = entity.id.to_string();
    // create the notification
    shared::notifications::create_notification(entity, key, req, params, &state.shared).await
}

/// Get all of an entity's notifications
///
/// # Arguments
///
/// * `user` - The user that is requesting the notifications
/// * `id` - The id of the entity whose notifications are being requested
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/entities/notifications/{id}",
    params(
        ("id" = Uuid, Path, description = "The id of the entity whose notifications are being requested"),
    ),
    responses(
        (status = 200, description = "Notifications returned for the entity", body = Vec<EntityNotification>),
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
#[instrument(name = "routes::entities::get_notifications", skip_all, err(Debug))]
async fn get_notifications(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Notification<Entity>>>, ApiError> {
    // check that the entity exists and the user has access
    let entity = Entity::get(&user, id, &state.shared).await?;
    // generate the entity's key
    let key = entity.id.to_string();
    // get all of the entity's notifications
    shared::notifications::get_notifications(entity, key, &state.shared).await
}

/// Delete an entity notification
///
/// # Arguments
///
/// * `user` - The user that is deleting the notification
/// * `id` - The id of the entity whose notification is being deleted
/// * `notification_id` - The id of the notification to delete
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/entities/notifications/{id}/{notification_id}",
    params(
        ("id" = Uuid, Path, description = "The id of the entity whose notification is being deleted"),
        ("notification_id" = Uuid, Path, description = "The notification's unique ID"),
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
#[instrument(name = "routes::entities::delete_notification", skip_all, err(Debug))]
async fn delete_notification(
    user: AuthedUser,
    Path((id, notification_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // only admins can delete entity notifications
    is_admin!(&user);
    // check that the entity exists
    let entity = Entity::get(&user, id, &state.shared).await?;
    // generate the entity's key
    let key = entity.id.to_string();
    // delete the notification
    shared::notifications::delete_notification(entity, key, None, notification_id, &state.shared)
        .await
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(create, create_yara_hit, create_hunt, cancel_hunt, checkpoint_hunt, list_hunt_rules, hunt_system_status, set_quarantine_exemption, clear_quarantine_exemption, get, get_by_name, list, list_details, update, delete, tag, delete_tags, get_image, list_associations, associations, notifications, create_notification, get_notifications, delete_notification, tool_instances, workflow_toolset, request_instance, request_tool_set_instance, refresh_model_endpoint, model_credential_secret, grep, grep_entity),
    components(schemas(ApiCursor<Association>, ApiCursor<Entity>, ApiCursor<EntityListLine>, ApiCursor<GrepMatches>, ApiCursor<HuntRuleLine>, Association, Entity, EntityListLine, EntityResponse, GrepDocumentMatches, GrepMatch, GrepMatches, GrepQueryParams, GrepRequest, GrepSource, GrepStats, HuntCheckpoint, HuntCheckpointResponse, HuntCreateRequest, HuntRuleLine, HuntSystemStatus, InstancedAgentToolset, ModelCredentialEntity, Notification<Entity>, NotificationRequest<Entity>, TagDeleteRequest<Entity>, TagRequest<Entity>, ToolInstanceSummary, TruncationReason, YaraHitWriteRequest, YaraHitWriteResponse)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct EntityApiDocs;

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
        .route("/entities/yara/hit/", post(create_yara_hit))
        .route("/entities/hunt/", post(create_hunt))
        .route("/entities/hunt/{id}/cancel", post(cancel_hunt))
        .route("/entities/hunt/{id}/checkpoint", post(checkpoint_hunt))
        .route(
            "/entities/hunt/{id}/rules",
            axum::routing::get(list_hunt_rules),
        )
        .route(
            "/entities/hunt/status/",
            axum::routing::get(hunt_system_status),
        )
        .route(
            "/entities/rules/{id}/quarantine-exemption",
            post(set_quarantine_exemption).delete(clear_quarantine_exemption),
        )
        .route("/entities/", axum::routing::get(list))
        .route("/entities/details/", axum::routing::get(list_details))
        .route("/entities/by-name/", axum::routing::get(get_by_name))
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
        .route(
            "/entities/notifications/{id}",
            post(create_notification).get(get_notifications),
        )
        .route(
            "/entities/notifications/{id}/{notification_id}",
            axum::routing::delete(delete_notification),
        )
}
