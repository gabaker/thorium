//! Integration tests for model endpoints, profiles, and credentials
//!
//! These cover who may write each kind, the provider and reference checks, secrets that are
//! stored but never returned, bans kept once per entity, and the reference associations that
//! keep a referenced entity from being deleted or losing a group its referrers need.

use chrono::Utc;
use std::collections::BTreeSet;
use thorium::models::{
    AiSettingsUpdate, AllowedEndpoint, AssociationKind, AssociationRequest, AssociationTarget,
    AwsRoleSource, AzureApiMode, AzureClientAuth, BedrockEndpointKind, CatalogEntrySource,
    CredentialKind, Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityUpdate, EventType, GroupAllowed, GroupRequest, GroupUpdate,
    GroupUsersRequest, GroupUsersUpdate, LimitsSource, ListedModel, ModelApi, ModelBan,
    ModelBanKind, ModelBanUpdate, ModelCapabilities, ModelCatalogEntryRequest, ModelConfigRef,
    ModelCredentialEntity, ModelCredentialEntityRequest, ModelEndpointEntity,
    ModelEndpointEntityRequest, ModelLimits, ModelParameters, ModelProfileEntity,
    ModelProfileEntityRequest, ModelProvider, ModelRefreshReason, ModelRefreshReport,
    ModelRefreshState, ModelRefreshTarget, ModelSecret, ModelsConfUpdate, NotificationType,
    TreeOpts, TreeQuery, UserCredentialRef, UserModelEndpoint, UserModelSettings,
    UserModelSettingsUpdate, UserRole, UserSettingsUpdate, UserUpdate, VertexLocation,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser};
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// Generate a unique secret value
fn gen_secret() -> String {
    format!("sk-{}", generators::gen_string(32).to_lowercase())
}

/// Get the endpoint metadata of an entity
///
/// # Arguments
///
/// * `entity` - The endpoint entity
fn endpoint_meta(entity: &Entity) -> &ModelEndpointEntity {
    match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => meta,
        _ => panic!("expected a model endpoint, got a {}", entity.kind),
    }
}

/// Get the profile metadata of an entity
///
/// # Arguments
///
/// * `entity` - The profile entity
fn profile_meta(entity: &Entity) -> &ModelProfileEntity {
    match &entity.metadata {
        EntityMetadata::ModelProfile(meta) => meta,
        _ => panic!("expected a model profile, got a {}", entity.kind),
    }
}

/// Get the credential metadata of an entity
///
/// # Arguments
///
/// * `entity` - The credential entity
fn credential_meta(entity: &Entity) -> &ModelCredentialEntity {
    match &entity.metadata {
        EntityMetadata::ModelCredential(meta) => meta,
        _ => panic!("expected a model credential, got a {}", entity.kind),
    }
}

/// Make sure a response body holds no secret, without ever printing the body
///
/// # Arguments
///
/// * `body` - The response body
/// * `secrets` - The secrets that must not appear
/// * `what` - What the body is, for the failure message
fn assert_no_secret(body: &str, secrets: &[&str], what: &str) {
    // check each secret without printing anything that might hold one
    for (index, secret) in secrets.iter().enumerate() {
        assert!(!body.contains(secret), "{what} returned secret #{index}");
    }
}

/// Get a raw API response body as a user
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `path` - The API path, starting with `/api/`
async fn raw_get(user: &TestUser, path: &str) -> Result<String, Error> {
    // send the request with the user's token
    let admin = test_utilities::admin_client().await?;
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", admin.host))
        .header("authorization", user.auth_header())
        .send()
        .await?;
    // only a successful response is inspected
    assert!(
        resp.status().is_success(),
        "GET {path} returned {}",
        resp.status()
    );
    Ok(resp.text().await?)
}

/// Create a user with a global role
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `role` - The global role to give the user
async fn user(admin: &Thorium, role: UserRole) -> Result<TestUser, Error> {
    generators::user_with_role(admin, role).await
}

/// Create a plain user
///
/// # Arguments
///
/// * `admin` - The admin client
async fn plain(admin: &Thorium) -> Result<TestUser, Error> {
    user(admin, UserRole::User).await
}

/// Create a group with chosen role holders that allows everything
///
/// # Arguments
///
/// * `roles` - The users holding each role
/// * `admin` - The admin client
async fn group(roles: GroupRoles, admin: &Thorium) -> Result<String, Error> {
    generators::group_with_roles(&roles, GroupAllowed::default(), admin).await
}

/// Build an Anthropic endpoint request with one catalog model
///
/// # Arguments
///
/// * `credential` - The credential the endpoint authenticates with, if any
/// * `model_id` - The model in the catalog
fn anthropic(credential: Option<Uuid>, model_id: &str) -> ModelEndpointEntityRequest {
    // the default Anthropic url with a manual catalog entry
    let mut entry = ModelCatalogEntryRequest::new(model_id);
    entry.context_window = Some(200_000);
    entry.max_output_tokens = Some(8_192);
    let mut req =
        ModelEndpointEntityRequest::new(generators::gen_anthropic_provider()).model(entry);
    req.credential = credential;
    req
}

/// Build an endpoint update
fn endpoint_update() -> EntityMetadataUpdate {
    EntityMetadataUpdate::model_endpoint()
}

/// Send a metadata update
///
/// # Arguments
///
/// * `client` - The client to send it with
/// * `id` - The entity to update
/// * `meta` - The metadata update
async fn update(client: &Thorium, id: Uuid, meta: EntityMetadataUpdate) -> Result<(), Error> {
    // send the metadata update
    client
        .entities
        .update(id, EntityUpdate::default().metadata(meta))
        .await?;
    Ok(())
}

/// Get the stored kind data of an entity's rows, sorted by group
///
/// # Arguments
///
/// * `id` - The entity to read
async fn rows(id: Uuid) -> Result<Vec<(String, String)>, Error> {
    // read and sort the rows
    let mut rows = test_utilities::entity_rows(id).await?;
    rows.sort();
    Ok(rows)
}

/// Get the edges of one kind an entity has, with the other end's id and the edge's groups
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The entity to list edges of
/// * `kind` - The kind of edge
async fn edges(
    admin: &Thorium,
    id: Uuid,
    kind: AssociationKind,
) -> Result<Vec<(Uuid, Vec<String>)>, Error> {
    // keep the entity edges of this kind
    Ok(admin
        .entities
        .associations(id)
        .await?
        .into_iter()
        .filter(|edge| edge.kind == kind)
        .filter_map(|edge| match edge.other {
            AssociationTarget::Entity { id, .. } => {
                let mut groups = edge.groups;
                groups.sort();
                Some((id, groups))
            }
            _ => None,
        })
        .collect())
}

/// Get a refusal's message
///
/// # Arguments
///
/// * `result` - The refused request
fn refusal<T>(result: Result<T, Error>) -> String {
    // the message of the error
    let msg = match result {
        Ok(_) => panic!("expected a refusal"),
        Err(err) => err.msg().unwrap_or_default(),
    };
    // unwrap the message from the API's error body when it's in one
    serde_json::from_str::<serde_json::Value>(&msg)
        .ok()
        .and_then(|body| body["error"].as_str().map(ToOwned::to_owned))
        .unwrap_or(msg)
}

/// Get the model refresh events queued for an endpoint, with who queued each one
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `endpoint` - The endpoint the refreshes target
async fn refreshes(
    admin: &Thorium,
    endpoint: Uuid,
) -> Result<Vec<(String, ModelRefreshTarget, ModelRefreshReason)>, Error> {
    // read the queued refreshes of this endpoint
    let queued = events::refresh_events(admin, endpoint).await?;
    Ok(queued
        .iter()
        .filter_map(|event| {
            events::refresh_event(event).map(|refresh| {
                (
                    event.user.clone(),
                    refresh.target.clone(),
                    refresh.reason.clone(),
                )
            })
        })
        .collect())
}

/// Get the model refresh events queued because a credential changed
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `credential` - The credential that changed
async fn credential_refreshes(
    admin: &Thorium,
    credential: ModelConfigRef,
) -> Result<Vec<ModelRefreshTarget>, Error> {
    // read every queued refresh and keep the ones this credential caused
    let queued = events::queued(admin, EventType::ModelRefresh).await?;
    Ok(queued
        .iter()
        .filter_map(events::refresh_event)
        .filter(|refresh| refresh.reason == ModelRefreshReason::CredentialChanged { credential })
        .map(|refresh| refresh.target.clone())
        .collect())
}

/// Build a successful refresh report
///
/// # Arguments
///
/// * `models` - The listed models and their reported context windows
fn report(models: &[(&str, Option<u64>)]) -> ModelRefreshReport {
    // every model is listed with only a context window
    ModelRefreshReport {
        finished: Utc::now(),
        models: models
            .iter()
            .map(|(id, context_window)| ListedModel {
                model_id: (*id).to_owned(),
                context_window: *context_window,
                max_output_tokens: None,
                capabilities: None,
            })
            .collect(),
        failure: None,
        credential: None,
    }
}

/// Build a failed refresh report
///
/// # Arguments
///
/// * `failure` - Why the listing failed
fn failed_report(failure: ModelBanKind) -> ModelRefreshReport {
    ModelRefreshReport {
        finished: Utc::now(),
        models: Vec::new(),
        failure: Some(failure),
        credential: None,
    }
}

/// Send a refresh report for an endpoint
///
/// # Arguments
///
/// * `client` - The client to send it with
/// * `endpoint` - The endpoint the report is for
/// * `report` - The report
async fn send_report(
    client: &Thorium,
    endpoint: Uuid,
    report: ModelRefreshReport,
) -> Result<(), Error> {
    // a report is an endpoint update
    update(client, endpoint, endpoint_update().refresh_report(report)).await
}

/// Send an update of a user's own model settings
///
/// # Arguments
///
/// * `client` - The client of the user to update
/// * `update` - The model settings update
async fn set_models(client: &Thorium, update: UserModelSettingsUpdate) -> Result<(), Error> {
    // wrap the model update in a user settings update
    let ai = AiSettingsUpdate::default().models(update);
    let settings = UserSettingsUpdate::default().ai(ai);
    client
        .users
        .update_self(UserUpdate::default().settings(settings))
        .await?;
    Ok(())
}

/// Get a user's own model settings
///
/// # Arguments
///
/// * `client` - The client of the user to read
async fn own_models(client: &Thorium) -> Result<UserModelSettings, Error> {
    // read the caller's settings
    let info = client.users.info().await?;
    Ok(info.settings.ai.map_or_default(|ai| ai.models))
}

/// Build an Anthropic provider with its own allowed url
///
/// # Arguments
///
/// * `name` - A name that makes the url unique
async fn allowed_anthropic(name: &str) -> Result<ModelProvider, Error> {
    // a unique url the deployment allows
    let provider = ModelProvider::Anthropic {
        base_url: Some(format!(
            "https://{name}-{}.anthropic.local",
            generators::gen_string(8).to_lowercase()
        )),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    };
    generators::allow_model_endpoint(&provider).await?;
    Ok(provider)
}

/// A report bans or clears only the credential it names, so a report from a refresh that used
/// an endpoint's earlier credential, or one naming no credential, never changes the current one
#[tokio::test]
async fn report_credential_must_match() -> Result<(), Error> {
    // europa owns minerva's endpoint, which moved from one credential to another
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    let first = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let second = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let mut req = ModelEndpointEntityRequest::new(allowed_anthropic("cred-match").await?);
    req.credential = Some(first);
    let endpoint = generators::model_endpoint(&groups, req, &europa.client).await?;
    update(
        &europa.client,
        endpoint.id,
        endpoint_update().endpoint_credential(second),
    )
    .await?;
    let rejected = || {
        failed_report(ModelBanKind::CredentialRejected {
            status: 401,
            message: "invalid x-api-key".to_owned(),
        })
    };
    let bans = |id| {
        let admin = admin.clone();
        async move { Ok::<usize, Error>(credential_meta(&admin.entities.get(id).await?).bans.len()) }
    };
    // a report naming the earlier credential or none bans neither
    for named in [Some(ModelConfigRef::Group(first)), None] {
        send_report(&admin, endpoint.id, rejected().with_credential(named)).await?;
    }
    is!(bans(first).await?, 0);
    is!(bans(second).await?, 0);
    // a report naming the current credential bans it
    send_report(
        &admin,
        endpoint.id,
        rejected().with_credential(Some(ModelConfigRef::Group(second))),
    )
    .await?;
    is!(bans(second).await?, 1);
    // a user's report bans only the credential of theirs it names
    let mine = UserCredentialRef::new(
        CredentialKind::ApiKey,
        Some(ModelSecret::from(gen_secret())),
    );
    let mine_id = mine.id;
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().credential(endpoint.id, mine),
    )
    .await?;
    for named in [Some(ModelConfigRef::User(Uuid::new_v4())), None] {
        set_models(
            &europa.client,
            UserModelSettingsUpdate::default()
                .refresh_report(endpoint.id, rejected().with_credential(named)),
        )
        .await?;
    }
    is!(
        own_models(&europa.client).await?.credentials[&endpoint.id]
            .bans
            .len(),
        0
    );
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().refresh_report(
            endpoint.id,
            rejected().with_credential(Some(ModelConfigRef::User(mine_id))),
        ),
    )
    .await?;
    is!(
        own_models(&europa.client).await?.credentials[&endpoint.id]
            .bans
            .len(),
        1
    );
    Ok(())
}

/// The allowed endpoint scan bans an endpoint only under the endpoint's lock, so it never drops
/// a change another write is making to it
#[tokio::test]
async fn whitelist_scan_waits_for_endpoint_lock() -> Result<(), Error> {
    // europa owns minerva's endpoint
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let provider = allowed_anthropic("scan-lock").await?;
    let endpoint = generators::model_endpoint(
        &[minerva.as_str()],
        ModelEndpointEntityRequest::new(provider.clone()),
        &europa.client,
    )
    .await?;
    // another write holds the endpoint's lock while the deployment stops allowing it
    test_utilities::hold_instance_lock(endpoint.id).await?;
    let entry = AllowedEndpoint::for_provider(&provider);
    let scan = tokio::spawn(generators::update_model_whitelist(
        ModelsConfUpdate::default().remove_endpoint(entry),
        true,
    ));
    // the scan waits for the lock instead of writing the endpoint
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let read = admin.entities.get(endpoint.id).await?;
    is!(endpoint_meta(&read).bans.is_empty(), true);
    // once the lock is released the scan bans it
    test_utilities::release_instance_lock(endpoint.id).await?;
    scan.await
        .map_err(|err| Error::new(format!("the scan panicked: {err}")))??;
    let read = admin.entities.get(endpoint.id).await?;
    let kinds = endpoint_meta(&read)
        .bans
        .values()
        .map(|ban| ban.ban_kind.clone())
        .collect::<Vec<_>>();
    is!(kinds, vec![ModelBanKind::NotAllowedByDeployment]);
    Ok(())
}

/// Deleting an access key bans its assumed roles only under each role's lock, waits only briefly
/// for a role another write holds, and then refuses with a conflict, leaving the role unbanned
/// and the key in place; the retried delete bans the role
#[tokio::test]
async fn lost_source_bans_wait_briefly_for_role_lock() -> Result<(), Error> {
    // an access key and a role that assumes from it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    let key = cred(
        &admin,
        &groups,
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIALOCK".to_owned(),
        },
    )
    .await?;
    let role = cred(
        &admin,
        &groups,
        CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::1:role/r".into(),
            external_id: None,
            session_name: None,
            duration_seconds: None,
            source: AwsRoleSource::AccessKey(key),
        },
    )
    .await?;
    // a delete while another write holds the role's lock is refused quickly
    test_utilities::hold_instance_lock(role).await?;
    let started = std::time::Instant::now();
    let refused = admin.entities.delete(key).await;
    let took = started.elapsed();
    test_utilities::release_instance_lock(role).await?;
    fail!(refused, 409);
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");
    // and leaves the role unbanned and the key in place
    is!(
        credential_meta(&admin.entities.get(role).await?)
            .bans
            .is_empty(),
        true
    );
    admin.entities.get(key).await?;
    // the retried delete bans the role and removes the key
    admin.entities.delete(key).await?;
    let kinds = credential_meta(&admin.entities.get(role).await?)
        .bans
        .values()
        .map(|ban| ban.ban_kind.clone())
        .collect::<Vec<_>>();
    is!(kinds, vec![ModelBanKind::BrokenReference(key)]);
    fail!(admin.entities.get(key).await, 404);
    Ok(())
}

/// A report's credential ban waits only briefly for a credential another write holds, and
/// refuses with a conflict the refresh retries instead of outwaiting the endpoint's lock
#[tokio::test]
async fn report_credential_lock_waits_briefly() -> Result<(), Error> {
    // europa owns minerva's endpoint and its credential
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    let credential = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let mut req = ModelEndpointEntityRequest::new(allowed_anthropic("cred-lock").await?);
    req.credential = Some(credential);
    let endpoint = generators::model_endpoint(&groups, req, &europa.client).await?;
    // a report rejecting the credential while another write holds it is refused quickly
    test_utilities::hold_instance_lock(credential).await?;
    let rejected = || {
        failed_report(ModelBanKind::CredentialRejected {
            status: 401,
            message: "invalid x-api-key".to_owned(),
        })
        .with_credential(Some(ModelConfigRef::Group(credential)))
    };
    let started = std::time::Instant::now();
    let refused = send_report(&admin, endpoint.id, rejected()).await;
    let took = started.elapsed();
    test_utilities::release_instance_lock(credential).await?;
    fail!(refused, 409);
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");
    is!(
        credential_meta(&admin.entities.get(credential).await?)
            .bans
            .is_empty(),
        true
    );
    // the retried report bans it
    send_report(&admin, endpoint.id, rejected()).await?;
    let kinds = credential_meta(&admin.entities.get(credential).await?)
        .bans
        .values()
        .map(|ban| ban.ban_kind.name())
        .collect::<Vec<_>>();
    is!(kinds, vec!["CredentialRejected"]);
    Ok(())
}

/// Asking for a refresh of an endpoint whose refresh is already pending answers like any
/// request but queues no second refresh
#[tokio::test]
async fn pending_refresh_queues_once() -> Result<(), Error> {
    // europa owns minerva's endpoint, whose creation queued one refresh
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let endpoint = generators::model_endpoint(
        &[minerva.as_str()],
        ModelEndpointEntityRequest::new(allowed_anthropic("pending").await?),
        &europa.client,
    )
    .await?;
    is!(refreshes(&admin, endpoint.id).await?.len(), 1);
    // the first request marks it pending and queues a refresh, the second only answers
    for _ in 0..2 {
        europa
            .client
            .entities
            .refresh_model_endpoint(endpoint.id)
            .await?;
    }
    is!(refreshes(&admin, endpoint.id).await?.len(), 2);
    let read = admin.entities.get(endpoint.id).await?;
    is!(
        endpoint_meta(&read).refresh.state,
        ModelRefreshState::Pending
    );
    // once a report finishes it, a request queues again
    send_report(&admin, endpoint.id, report(&[("claude-a", None)])).await?;
    europa
        .client
        .entities
        .refresh_model_endpoint(endpoint.id)
        .await?;
    is!(refreshes(&admin, endpoint.id).await?.len(), 3);
    Ok(())
}

/// Rotating a credential, moving an endpoint's credential, or setting a user credential queues
/// no refresh of an endpoint the deployment no longer allows
#[tokio::test]
async fn deallowed_endpoints_queue_no_refresh() -> Result<(), Error> {
    // europa owns minerva's endpoint and its credential
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    let credential = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let provider = allowed_anthropic("deallowed").await?;
    let mut req = ModelEndpointEntityRequest::new(provider.clone());
    req.credential = Some(credential);
    let endpoint = generators::model_endpoint(&groups, req, &europa.client).await?;
    is!(refreshes(&admin, endpoint.id).await?.len(), 1);
    // the deployment stops allowing the endpoint
    generators::update_model_whitelist(
        ModelsConfUpdate::default().remove_endpoint(AllowedEndpoint::for_provider(&provider)),
        true,
    )
    .await?;
    // rotating its credential queues nothing
    update(
        &europa.client,
        credential,
        EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
    )
    .await?;
    is!(
        credential_refreshes(&admin, ModelConfigRef::Group(credential))
            .await?
            .len(),
        0
    );
    // moving it to another credential queues nothing
    let other = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    update(
        &europa.client,
        endpoint.id,
        endpoint_update().endpoint_credential(other),
    )
    .await?;
    // a user credential for it queues nothing
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().credential(
            endpoint.id,
            UserCredentialRef::new(
                CredentialKind::ApiKey,
                Some(ModelSecret::from(gen_secret())),
            ),
        ),
    )
    .await?;
    is!(refreshes(&admin, endpoint.id).await?.len(), 1);
    Ok(())
}

/// The refresh route queues one refresh for an owner or manager of any group and refuses other
/// members, sets the endpoint's refresh state, and only admins send the reports that finish it
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_route() -> Result<(), Error> {
    // europa owns minerva and mars, callisto manages only minerva, and io, an analyst, and a
    // monitor are other members of minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let callisto = generators::moon_user(&admin, "callisto", &suffix).await?;
    let io = generators::moon_user(&admin, "io", &suffix).await?;
    let ganymede =
        generators::named_user_with_role(&admin, &format!("ganymede-{suffix}"), UserRole::Analyst)
            .await?;
    let amalthea = generators::moon_user(&admin, "amalthea", &suffix).await?;
    let outsider = generators::moon_user(&admin, "himalia", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .owner(&europa.username)
            .manager(&callisto.username)
            .user(&io.username)
            .user(&ganymede.username)
            .monitor(&amalthea.username),
    )
    .await?;
    let mars = generators::god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    generators::god_group(
        &admin,
        "vesta",
        &suffix,
        &GroupRoles::default().user(&outsider.username),
    )
    .await?;
    // an endpoint in both groups with a group credential
    let groups = [minerva.as_str(), mars.as_str()];
    let credential = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let mut req = ModelEndpointEntityRequest::new(allowed_anthropic("route").await?)
        .model(ModelCatalogEntryRequest::new("claude-manual"));
    req.credential = Some(credential);
    let endpoint = generators::model_endpoint(&groups, req, &europa.client).await?;
    // a new endpoint has never been refreshed, and creating it queued one refresh
    let meta = endpoint_meta(&endpoint);
    is!(meta.refresh.state, ModelRefreshState::Never);
    is!(meta.refresh.requested.is_none(), true);
    is!(meta.refresh.finished.is_none(), true);
    let mut sorted = vec![minerva.clone(), mars.clone()];
    sorted.sort();
    let target = ModelRefreshTarget::GroupEndpoint {
        groups: sorted,
        endpoint: endpoint.id,
    };
    is!(
        refreshes(&admin, endpoint.id).await?,
        vec![(
            europa.username.clone(),
            target.clone(),
            ModelRefreshReason::EndpointChanged
        )]
    );
    // members who don't own or manage a group can't refresh, and nothing is queued
    for member in [&io, &ganymede, &amalthea] {
        let resp = member
            .client
            .entities
            .refresh_model_endpoint(endpoint.id)
            .await;
        let error = resp.expect_err("a member's refresh is refused");
        is!(error.status().map(|status| status.as_u16()), Some(401));
        is!(
            error
                .msg()
                .unwrap_or_default()
                .contains("permission to refresh"),
            true
        );
    }
    // users outside its groups and unknown ids get a 404
    fail!(
        outsider
            .client
            .entities
            .refresh_model_endpoint(endpoint.id)
            .await,
        404
    );
    fail!(
        europa
            .client
            .entities
            .refresh_model_endpoint(Uuid::new_v4())
            .await,
        404
    );
    // a credential isn't an endpoint
    fail!(
        europa
            .client
            .entities
            .refresh_model_endpoint(credential)
            .await,
        404
    );
    // none of the refusals queued anything
    is!(refreshes(&admin, endpoint.id).await?.len(), 1);
    // a manager of only one group refreshes it, which marks it pending
    callisto
        .client
        .entities
        .refresh_model_endpoint(endpoint.id)
        .await?;
    let queued = refreshes(&admin, endpoint.id).await?;
    is!(queued.len(), 2);
    is!(
        &queued[1],
        &(
            callisto.username.clone(),
            target.clone(),
            ModelRefreshReason::Manual
        )
    );
    let meta = endpoint_meta(&admin.entities.get(endpoint.id).await?).clone();
    is!(meta.refresh.state, ModelRefreshState::Pending);
    is!(meta.refresh.requested.is_some(), true);
    // only admins send reports, even an owner of every group is refused
    fail!(
        send_report(&europa.client, endpoint.id, report(&[("claude-new", None)])).await,
        401
    );
    // a successful report finishes the refresh
    let listed = report(&[("claude-new", Some(100_000))]);
    let finished = listed.finished;
    send_report(&admin, endpoint.id, listed).await?;
    let meta = endpoint_meta(&admin.entities.get(endpoint.id).await?).clone();
    is!(meta.refresh.state, ModelRefreshState::Succeeded);
    is!(meta.refresh.finished, Some(finished));
    is!(meta.refresh.error.is_none(), true);
    is!(
        meta.catalog_entry("claude-new")
            .map(|entry| entry.limits.context_window),
        Some(Some(100_000))
    );
    // a failed report fails it with the reason
    send_report(
        &admin,
        endpoint.id,
        failed_report(ModelBanKind::EndpointUnreachable("refused".to_owned())),
    )
    .await?;
    let meta = endpoint_meta(&admin.entities.get(endpoint.id).await?).clone();
    is!(meta.refresh.state, ModelRefreshState::Failed);
    is!(
        meta.refresh
            .error
            .as_deref()
            .is_some_and(|error| error.contains("refused")),
        true
    );
    // a report naming a ban kind a refresh never registers is refused
    fail!(
        send_report(
            &admin,
            endpoint.id,
            failed_report(ModelBanKind::Generic("nope".to_owned()))
        )
        .await,
        400
    );
    // an endpoint outside the whitelist can't be refreshed, and nothing is queued
    let entry = AllowedEndpoint::for_provider(&meta.provider);
    generators::update_model_whitelist(
        ModelsConfUpdate::default().remove_endpoint(entry.clone()),
        false,
    )
    .await?;
    let refused = europa
        .client
        .entities
        .refresh_model_endpoint(endpoint.id)
        .await;
    generators::update_model_whitelist(ModelsConfUpdate::default().add_endpoint(entry), false)
        .await?;
    fail!(refused, 400);
    is!(refreshes(&admin, endpoint.id).await?.len(), 2);
    // a report for an endpoint deleted since its refresh started is a 404 and writes nothing
    admin.entities.delete(endpoint.id).await?;
    fail!(
        send_report(&admin, endpoint.id, report(&[("claude-new", None)])).await,
        404
    );
    is!(rows(endpoint.id).await?.len(), 0);
    Ok(())
}

/// Creating an endpoint, changing its provider or credential, rotating a credential endpoints
/// use, and a user setting their own credentials and endpoints each queue the refreshes they
/// need, and nothing else does
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_emission_sites() -> Result<(), Error> {
    // europa owns minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    // a credential no endpoint uses queues nothing, even when rotated
    let credential = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let group_ref = ModelConfigRef::Group(credential);
    update(
        &europa.client,
        credential,
        EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
    )
    .await?;
    is!(credential_refreshes(&admin, group_ref).await?.len(), 0);
    // two endpoints that use it each queue one refresh when created
    let mut first_req = ModelEndpointEntityRequest::new(allowed_anthropic("first").await?);
    first_req.credential = Some(credential);
    let first = generators::model_endpoint(&groups, first_req, &europa.client).await?;
    let mut second_req = ModelEndpointEntityRequest::new(allowed_anthropic("second").await?);
    second_req.credential = Some(credential);
    let second = generators::model_endpoint(&groups, second_req, &europa.client).await?;
    is!(refreshes(&admin, first.id).await?.len(), 1);
    is!(refreshes(&admin, second.id).await?.len(), 1);
    // rotating the credential queues one refresh per endpoint that uses it
    update(
        &europa.client,
        credential,
        EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
    )
    .await?;
    let mut targets = credential_refreshes(&admin, group_ref)
        .await?
        .into_iter()
        .filter_map(|target| match target {
            ModelRefreshTarget::GroupEndpoint { endpoint, groups } => Some((endpoint, groups)),
            _ => None,
        })
        .collect::<Vec<(Uuid, Vec<String>)>>();
    targets.sort();
    let mut expected = vec![
        (first.id, vec![minerva.clone()]),
        (second.id, vec![minerva.clone()]),
    ];
    expected.sort();
    is!(targets, expected);
    // changing only when it expires queues nothing
    update(
        &europa.client,
        credential,
        EntityMetadataUpdate::model_credential()
            .credential_expires(Utc::now() + chrono::Duration::days(30)),
    )
    .await?;
    is!(credential_refreshes(&admin, group_ref).await?.len(), 2);
    // a provider change and a credential change each queue one refresh of the endpoint
    update(
        &europa.client,
        first.id,
        endpoint_update().endpoint_provider(allowed_anthropic("moved").await?),
    )
    .await?;
    is!(refreshes(&admin, first.id).await?.len(), 3);
    update(
        &europa.client,
        first.id,
        endpoint_update().clear_endpoint_credential(),
    )
    .await?;
    let queued = refreshes(&admin, first.id).await?;
    is!(queued.len(), 4);
    is!(&queued[3].2, &ModelRefreshReason::EndpointChanged);
    // catalog, ban, and disabled model edits queue nothing
    update(
        &europa.client,
        first.id,
        endpoint_update()
            .add_catalog_model(ModelCatalogEntryRequest::new("claude-hand"))
            .disable_model("claude-hand")
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("paused"))),
    )
    .await?;
    is!(refreshes(&admin, first.id).await?.len(), 4);
    // europa setting her credential for the group endpoint queues one refresh for her
    let mine = UserCredentialRef::new(
        CredentialKind::ApiKey,
        Some(ModelSecret::from(gen_secret())),
    );
    let mine_id = mine.id;
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().credential(first.id, mine),
    )
    .await?;
    let user_target = ModelRefreshTarget::UserEndpoint {
        user: europa.username.clone(),
        endpoint: first.id,
    };
    let queued = refreshes(&admin, first.id).await?;
    is!(queued.len(), 5);
    is!(
        &queued[4],
        &(
            europa.username.clone(),
            user_target.clone(),
            ModelRefreshReason::CredentialChanged {
                credential: ModelConfigRef::User(mine_id)
            }
        )
    );
    // rotating it queues another
    let rotated = UserCredentialRef::new(
        CredentialKind::ApiKey,
        Some(ModelSecret::from(gen_secret())),
    );
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().credential(first.id, rotated),
    )
    .await?;
    is!(refreshes(&admin, first.id).await?.len(), 6);
    // adding and changing her own endpoint queue one each
    let own_id = Uuid::new_v4();
    let own = UserModelEndpoint::new("mine", allowed_anthropic("own").await?);
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().endpoint(own_id, own.clone()),
    )
    .await?;
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().endpoint(own_id, own.disable("claude-x")),
    )
    .await?;
    let queued = refreshes(&admin, own_id).await?;
    is!(queued.len(), 2);
    is!(
        &queued[0],
        &(
            europa.username.clone(),
            ModelRefreshTarget::UserEndpoint {
                user: europa.username.clone(),
                endpoint: own_id
            },
            ModelRefreshReason::EndpointChanged
        )
    );
    // removing her credential and her endpoint queue nothing
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().remove_credential(first.id),
    )
    .await?;
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().remove_endpoint(own_id),
    )
    .await?;
    is!(refreshes(&admin, first.id).await?.len(), 6);
    is!(refreshes(&admin, own_id).await?.len(), 2);
    Ok(())
}

/// Refresh reports ban and unban the credential the refresh used and the endpoint, each ban
/// writing and deleting its notification, and a user's own reports do the same in their
/// settings with notifications for that user
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_report_bans() -> Result<(), Error> {
    // europa owns minerva, which has an endpoint with a group credential
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    let credential = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let mut req = ModelEndpointEntityRequest::new(allowed_anthropic("bans").await?);
    req.credential = Some(credential);
    let endpoint = generators::model_endpoint(&groups, req, &europa.client)
        .await?
        .id;
    // a rejected key bans the credential once, with a notification
    let rejected = ModelBanKind::CredentialRejected {
        status: 401,
        message: "invalid x-api-key".to_owned(),
    };
    let named = Some(ModelConfigRef::Group(credential));
    for _ in 0..2 {
        send_report(
            &admin,
            endpoint,
            failed_report(rejected.clone()).with_credential(named),
        )
        .await?;
    }
    let cred_meta = credential_meta(&admin.entities.get(credential).await?).clone();
    is!(cred_meta.bans.len(), 1);
    is!(
        cred_meta
            .bans
            .values()
            .next()
            .map(|ban| ban.ban_kind.clone()),
        Some(rejected.clone())
    );
    is!(
        stored_notes(NotificationType::ModelCredentials, credential).await?,
        1
    );
    // an unreachable endpoint bans the endpoint, with a notification
    send_report(
        &admin,
        endpoint,
        failed_report(ModelBanKind::EndpointUnreachable("refused".to_owned())),
    )
    .await?;
    is!(
        endpoint_meta(&admin.entities.get(endpoint).await?)
            .bans
            .len(),
        1
    );
    is!(
        stored_notes(NotificationType::ModelEndpoints, endpoint).await?,
        1
    );
    // a successful listing clears both bans and their notifications
    send_report(
        &admin,
        endpoint,
        report(&[("claude-a", None)]).with_credential(named),
    )
    .await?;
    is!(
        credential_meta(&admin.entities.get(credential).await?)
            .bans
            .len(),
        0
    );
    is!(
        endpoint_meta(&admin.entities.get(endpoint).await?)
            .bans
            .len(),
        0
    );
    is!(
        stored_notes(NotificationType::ModelCredentials, credential).await?,
        0
    );
    is!(
        stored_notes(NotificationType::ModelEndpoints, endpoint).await?,
        0
    );
    // a model the next listing omits is banned on its endpoint, with a notification
    send_report(&admin, endpoint, report(&[])).await?;
    let meta = endpoint_meta(&admin.entities.get(endpoint).await?).clone();
    is!(
        meta.catalog_entry("claude-a").map(|entry| entry.bans.len()),
        Some(1)
    );
    is!(
        stored_notes(NotificationType::ModelEndpoints, endpoint).await?,
        1
    );
    // europa's own endpoint and credential take her reports through her own settings
    let own_id = Uuid::new_v4();
    let own = UserModelEndpoint::new("mine", allowed_anthropic("own").await?);
    let mine = UserCredentialRef::new(
        CredentialKind::ApiKey,
        Some(ModelSecret::from(gen_secret())),
    );
    let mine_named = Some(ModelConfigRef::User(mine.id));
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default()
            .endpoint(own_id, own)
            .credential(own_id, mine),
    )
    .await?;
    let notes_before = europa.client.users.notifications().await?.len();
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().refresh_report(
            own_id,
            failed_report(rejected.clone()).with_credential(mine_named),
        ),
    )
    .await?;
    let settings = own_models(&europa.client).await?;
    let banned = &settings.credentials[&own_id];
    is!(banned.bans.len(), 1);
    is!(
        settings.endpoints[&own_id].refresh.state,
        ModelRefreshState::Failed
    );
    is!(
        europa.client.users.notifications().await?.len(),
        notes_before + 1
    );
    // a successful report fills her catalog and clears the ban and its notification
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().refresh_report(
            own_id,
            report(&[("claude-own", Some(50_000))]).with_credential(mine_named),
        ),
    )
    .await?;
    let settings = own_models(&europa.client).await?;
    is!(settings.credentials[&own_id].bans.len(), 0);
    let own = &settings.endpoints[&own_id];
    is!(own.refresh.state, ModelRefreshState::Succeeded);
    is!(
        own.catalog
            .iter()
            .map(|entry| (entry.model_id.clone(), entry.source.clone()))
            .next()
            .map(|(id, source)| (id, matches!(source, CatalogEntrySource::Refreshed { .. }))),
        Some(("claude-own".to_owned(), true))
    );
    is!(
        europa.client.users.notifications().await?.len(),
        notes_before
    );
    // a report for an endpoint she has nothing for is a 404
    fail!(
        set_models(
            &europa.client,
            UserModelSettingsUpdate::default().refresh_report(Uuid::new_v4(), report(&[])),
        )
        .await,
        404
    );
    Ok(())
}

/// Creating, editing, banning, refreshing, and deleting model entities and moving their
/// reference edges queue no workflow graph event, and referenced entities can't be deleted
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn model_changes_queue_no_graph_events() -> Result<(), Error> {
    // europa owns minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    // two credentials, an endpoint using the first, and a profile on the endpoint
    let first = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let second = cred(&admin, &groups, CredentialKind::ApiKey).await?;
    let mut req = ModelEndpointEntityRequest::new(allowed_anthropic("quiet").await?)
        .model(ModelCatalogEntryRequest::new("claude-a"));
    req.credential = Some(first);
    let endpoint = generators::model_endpoint(&groups, req, &europa.client)
        .await?
        .id;
    let profile = generators::model_profile(
        &groups,
        ModelProfileEntityRequest::new(endpoint, "claude-a", ModelApi::AnthropicMessages),
        &europa.client,
    )
    .await?
    .id;
    // edit, rotate, ban, move the credential edge, refresh, and report
    update(
        &europa.client,
        endpoint,
        endpoint_update()
            .endpoint_credential(second)
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("paused"))),
    )
    .await?;
    update(
        &europa.client,
        second,
        EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
    )
    .await?;
    update(
        &europa.client,
        profile,
        EntityMetadataUpdate::model_profile()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("paused"))),
    )
    .await?;
    europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    send_report(&admin, endpoint, report(&[("claude-a", None)])).await?;
    // referenced entities can't be deleted
    fail!(admin.entities.delete(second).await, 409);
    fail!(admin.entities.delete(endpoint).await, 409);
    // deleting them in order works
    admin.entities.delete(profile).await?;
    admin.entities.delete(endpoint).await?;
    admin.entities.delete(second).await?;
    admin.entities.delete(first).await?;
    // none of it queued a workflow graph change
    for id in [first, second, endpoint, profile] {
        is!(events::graph_changes(&admin, id).await?.len(), 0);
    }
    Ok(())
}

/// The admin credential reads return secrets only to admins, and the refresh route's client
/// method reports unknown endpoints
#[tokio::test]
async fn credential_secret_reads() -> Result<(), Error> {
    // europa owns minerva, which has a credential, and an endpoint she has her own key for
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let groups = [minerva.as_str()];
    let secret = gen_secret();
    let credential =
        generators::model_credential(&groups, CredentialKind::ApiKey, Some(&secret), &admin)
            .await?
            .id;
    let endpoint = generators::model_endpoint(
        &groups,
        ModelEndpointEntityRequest::new(allowed_anthropic("secret").await?),
        &europa.client,
    )
    .await?
    .id;
    let own_secret = gen_secret();
    set_models(
        &europa.client,
        UserModelSettingsUpdate::default().credential(
            endpoint,
            UserCredentialRef::new(
                CredentialKind::ApiKey,
                Some(ModelSecret::from(own_secret.as_str())),
            ),
        ),
    )
    .await?;
    // an admin reads both secrets
    let read = admin.entities.model_credential_secret(credential).await?;
    is!(
        read.secrets
            .primary
            .as_ref()
            .map(|value| value.expose().to_owned()),
        Some(secret.clone())
    );
    let own = admin
        .users
        .user_model_credential(&europa.username, endpoint)
        .await?;
    is!(
        own.secrets
            .primary
            .as_ref()
            .map(|value| value.expose().to_owned()),
        Some(own_secret.clone())
    );
    // nobody else does, not even an owner reading their own
    fail!(
        europa
            .client
            .entities
            .model_credential_secret(credential)
            .await,
        401
    );
    fail!(
        europa
            .client
            .users
            .user_model_credential(&europa.username, endpoint)
            .await,
        401
    );
    // unknown ids, users, and endpoints without a credential are 404s
    fail!(
        admin.entities.model_credential_secret(Uuid::new_v4()).await,
        404
    );
    fail!(admin.entities.model_credential_secret(endpoint).await, 404);
    fail!(
        admin
            .users
            .user_model_credential(&europa.username, Uuid::new_v4())
            .await,
        404
    );
    fail!(
        admin
            .users
            .user_model_credential(&format!("nobody-{suffix}"), endpoint)
            .await,
        404
    );
    Ok(())
}

/// Only owners and managers write endpoints and credentials, any member who can edit writes
/// profiles, and every member reads them without secrets
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn manager_only_writes() -> Result<(), Error> {
    // a group with every role, including an analyst
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let manager = plain(&admin).await?;
    let member = plain(&admin).await?;
    let analyst = user(&admin, UserRole::Analyst).await?;
    let monitor = plain(&admin).await?;
    let g = group(
        GroupRoles::default()
            .owner(&owner.username)
            .manager(&manager.username)
            .user(&member.username)
            .user(&analyst.username)
            .monitor(&monitor.username),
        &admin,
    )
    .await?;
    let secret = gen_secret();
    // an owner creates a credential and a manager an endpoint that uses it
    let cred =
        generators::model_credential(&[&g], CredentialKind::ApiKey, Some(&secret), &owner.client)
            .await?;
    let endpoint =
        generators::model_endpoint(&[&g], anthropic(Some(cred.id), "claude-a"), &manager.client)
            .await?;
    // users, analysts, and monitors can't create, change, rotate, or delete them
    for denied in [&member, &analyst, &monitor] {
        let resp = generators::model_credential(
            &[&g],
            CredentialKind::ApiKey,
            Some(&secret),
            &denied.client,
        )
        .await;
        fail!(resp, 401);
        let resp =
            generators::model_endpoint(&[&g], anthropic(None, "claude-b"), &denied.client).await;
        fail!(resp, 401);
        let resp = denied
            .client
            .entities
            .update(endpoint.id, EntityUpdate::default().description("x"))
            .await;
        fail!(resp, 401);
        let resp = update(
            &denied.client,
            endpoint.id,
            endpoint_update().add_catalog_model(ModelCatalogEntryRequest::new("claude-c")),
        )
        .await;
        fail!(resp, 401);
        let resp = update(
            &denied.client,
            cred.id,
            EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
        )
        .await;
        fail!(resp, 401);
        let resp = denied.client.entities.delete(endpoint.id).await;
        fail!(resp, 401);
        let resp = denied.client.entities.delete(cred.id).await;
        fail!(resp, 401);
        // they still read both, with the credential's secret redacted
        let read = denied.client.entities.get(cred.id).await?;
        is!(
            credential_meta(&read).secrets.primary.clone(),
            None::<ModelSecret>
        );
        is!(
            credential_meta(&read).secrets.fingerprint.clone(),
            Some(ModelSecret::from(secret.as_str()).fingerprint())
        );
        assert_no_secret(
            &raw_get(denied, &format!("/api/entities/{}", cred.id)).await?,
            &[&secret],
            "a member's credential get",
        );
        denied.client.entities.get(endpoint.id).await?;
    }
    // any member who can edit creates a profile, but a monitor can't
    for creator in [&member, &analyst] {
        let req =
            ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages);
        generators::model_profile(&[&g], req, &creator.client).await?;
    }
    let req = ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages);
    let resp = generators::model_profile(&[&g], req, &monitor.client).await;
    fail!(resp, 401);
    // an owner and a manager each create, update, rotate, and delete an endpoint and a credential
    for writer in [&owner, &manager] {
        let spare_cred = generators::model_credential(
            &[&g],
            CredentialKind::ApiKey,
            Some(&gen_secret()),
            &writer.client,
        )
        .await?;
        let spare = generators::model_endpoint(
            &[&g],
            anthropic(Some(spare_cred.id), "claude-d"),
            &writer.client,
        )
        .await?;
        writer
            .client
            .entities
            .update(spare.id, EntityUpdate::default().description("changed"))
            .await?;
        is!(
            writer.client.entities.get(spare.id).await?.description,
            Some("changed".to_owned())
        );
        let rotated = gen_secret();
        update(
            &writer.client,
            spare_cred.id,
            EntityMetadataUpdate::model_credential().credential_secret(rotated.clone()),
        )
        .await?;
        let read = writer.client.entities.get(spare_cred.id).await?;
        is!(
            credential_meta(&read).secrets.fingerprint.clone(),
            Some(ModelSecret::from(rotated.as_str()).fingerprint())
        );
        writer.client.entities.delete(spare.id).await?;
        writer.client.entities.delete(spare_cred.id).await?;
        let resp = writer.client.entities.get(spare.id).await;
        fail!(resp, 404);
        let resp = writer.client.entities.get(spare_cred.id).await;
        fail!(resp, 404);
    }
    Ok(())
}

/// Create a credential as an admin and return its id
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `groups` - The credential's groups
/// * `kind` - The kind of credential
async fn cred(admin: &Thorium, groups: &[&str], kind: CredentialKind) -> Result<Uuid, Error> {
    // only kinds that hold a secret get one
    let secret = kind.needs_secret().then(gen_secret);
    Ok(
        generators::model_credential(groups, kind, secret.as_deref(), admin)
            .await?
            .id,
    )
}

/// Build a Bedrock runtime provider
fn bedrock() -> ModelProvider {
    ModelProvider::Bedrock {
        region: "us-east-1".into(),
        endpoint_kind: BedrockEndpointKind::Runtime,
        endpoint_url: None,
        workspace_id: None,
    }
}

/// Provider settings, protocols, credential kinds, and references are checked when entities are
/// created
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn provider_and_reference_checks() -> Result<(), Error> {
    // groups a and b
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    // bad urls and missing settings are refused, naming the setting
    let bad = [
        (
            ModelProvider::OpenAi {
                base_url: Some("ftp://x".into()),
                organization: None,
                project: None,
            },
            "OpenAi.base_url",
        ),
        (
            ModelProvider::OpenAi {
                base_url: Some("not a url".into()),
                organization: None,
                project: None,
            },
            "OpenAi.base_url",
        ),
        (
            ModelProvider::Bedrock {
                region: String::new(),
                endpoint_kind: BedrockEndpointKind::Runtime,
                endpoint_url: None,
                workspace_id: None,
            },
            "Bedrock.region",
        ),
        (
            ModelProvider::AzureOpenAi {
                endpoint: String::new(),
                api_mode: AzureApiMode::V1,
                entra_scope: None,
            },
            "AzureOpenAi.endpoint",
        ),
        (
            ModelProvider::LiteLlm {
                base_url: String::new(),
                key_header: None,
            },
            "LiteLlm.base_url",
        ),
    ];
    for (provider, field) in bad {
        let resp =
            generators::model_endpoint(&[&a], ModelEndpointEntityRequest::new(provider), &admin)
                .await;
        fail!(resp, 400);
        let msg = refusal(resp);
        assert!(msg.contains(field), "{msg}");
    }
    // a reference to the wrong kind of entity is refused
    let api_key = cred(&admin, &[&a, &b], CredentialKind::ApiKey).await?;
    let endpoint =
        generators::model_endpoint(&[&a], anthropic(Some(api_key), "claude-a"), &admin).await?;
    let profile = generators::model_profile(
        &[&a],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages),
        &admin,
    )
    .await?;
    let resp =
        generators::model_endpoint(&[&a], anthropic(Some(profile.id), "claude-a"), &admin).await;
    fail!(resp, 400);
    let resp = generators::model_profile(
        &[&a],
        ModelProfileEntityRequest::new(api_key, "claude-a", ModelApi::AnthropicMessages),
        &admin,
    )
    .await;
    fail!(resp, 400);
    // a protocol the provider doesn't serve is refused
    let resp = generators::model_profile(
        &[&a],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::BedrockConverse),
        &admin,
    )
    .await;
    fail!(resp, 400);
    // credential kinds a provider doesn't accept are refused
    let bearer = cred(&admin, &[&a], CredentialKind::AwsBearerToken).await?;
    let entra = cred(
        &admin,
        &[&a],
        CredentialKind::AzureEntraClient {
            tenant_id: "t".into(),
            client_id: "c".into(),
            auth: AzureClientAuth::ClientSecret,
        },
    )
    .await?;
    let openai = ModelProvider::OpenAi {
        base_url: None,
        organization: None,
        project: None,
    };
    for (provider, credential) in [
        (generators::gen_anthropic_provider(), bearer),
        (bedrock(), api_key),
        (openai.clone(), entra),
    ] {
        let req = ModelEndpointEntityRequest::new(provider).credential(credential);
        let resp = generators::model_endpoint(&[&a], req, &admin).await;
        fail!(resp, 400);
    }
    // the kinds a provider accepts are allowed
    let azure = ModelProvider::AzureOpenAi {
        endpoint: "https://res.openai.azure.com".into(),
        api_mode: AzureApiMode::V1,
        entra_scope: None,
    };
    generators::model_endpoint(
        &[&a],
        ModelEndpointEntityRequest::new(azure).credential(entra),
        &admin,
    )
    .await?;
    let gcp = cred(
        &admin,
        &[&a],
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
    )
    .await?;
    let vertex = ModelProvider::Vertex {
        project_id: Some("project".into()),
        location: VertexLocation::Global,
        endpoint_url: None,
    };
    generators::model_endpoint(
        &[&a],
        ModelEndpointEntityRequest::new(vertex).credential(gcp),
        &admin,
    )
    .await?;
    let access = cred(
        &admin,
        &[&a, &b],
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
    )
    .await?;
    let role = |source| CredentialKind::AwsAssumeRole {
        role_arn: "arn:aws:iam::1:role/r".into(),
        external_id: None,
        session_name: None,
        duration_seconds: None,
        source: AwsRoleSource::AccessKey(source),
    };
    let assumed = cred(&admin, &[&a], role(access)).await?;
    for credential in [access, assumed, bearer] {
        let req = ModelEndpointEntityRequest::new(bedrock()).credential(credential);
        generators::model_endpoint(&[&a], req, &admin).await?;
    }
    // an assumed role's source must be an access key that exists and covers its groups
    let narrow_access = cred(
        &admin,
        &[&a],
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIB".into(),
        },
    )
    .await?;
    for source in [api_key, Uuid::new_v4(), narrow_access] {
        let resp = generators::model_credential(&[&a, &b], role(source), None, &admin).await;
        fail!(resp, 400);
    }
    // gateways may skip a credential, and so may a provider whose runners bring their own
    let litellm = generators::model_endpoint(
        &[&a],
        ModelEndpointEntityRequest::new(generators::gen_litellm_provider()),
        &admin,
    )
    .await?;
    is!(endpoint_meta(&litellm).credential, None::<Uuid>);
    generators::model_endpoint(&[&a], ModelEndpointEntityRequest::new(openai), &admin).await?;
    Ok(())
}

/// Values written on lines of a harness's AWS config file can't hold control characters, whether
/// they come with a group credential, its rotation, or a user's own credential
#[tokio::test]
async fn aws_config_values_refuse_control_characters() -> Result<(), Error> {
    // a group and a value that would add a credential_process line to the config file
    let admin = test_utilities::admin_client().await?;
    let group = group(GroupRoles::default(), &admin).await?;
    let injected = "x\ncredential_process = /usr/bin/touch /tmp/injected";
    let access_key = |id: &str| CredentialKind::AwsAccessKey {
        access_key_id: id.into(),
    };
    // an access key whose id or secret holds a line break is refused
    let resp =
        generators::model_credential(&[&group], access_key(injected), Some("s"), &admin).await;
    fail!(resp, 400);
    let resp =
        generators::model_credential(&[&group], access_key("AKIA"), Some(injected), &admin).await;
    fail!(resp, 400);
    // an assumed role whose session name holds one is refused
    let access = cred(&admin, &[&group], access_key("AKIA")).await?;
    let role = CredentialKind::AwsAssumeRole {
        role_arn: "arn:aws:iam::1:role/r".into(),
        external_id: None,
        session_name: Some(injected.into()),
        duration_seconds: None,
        source: AwsRoleSource::AccessKey(access),
    };
    let resp = generators::model_credential(&[&group], role, None, &admin).await;
    fail!(resp, 400);
    // a rotation to a secret or session token holding one is refused
    for meta in [
        EntityMetadataUpdate::model_credential().credential_secret(injected),
        EntityMetadataUpdate::model_credential().credential_session_token(injected),
    ] {
        let resp = admin
            .entities
            .update(access, EntityUpdate::default().metadata(meta))
            .await;
        fail!(resp, 400);
    }
    // so is a user's own access key whose secret holds one
    let endpoint = generators::model_endpoint(
        &[&group],
        ModelEndpointEntityRequest::new(bedrock()).credential(access),
        &admin,
    )
    .await?;
    let mine = UserCredentialRef::new(access_key("AKIA"), Some(ModelSecret::from(injected)));
    let resp = set_models(
        &admin,
        UserModelSettingsUpdate::default().credential(endpoint.id, mine),
    )
    .await;
    fail!(resp, 400);
    Ok(())
}

/// Bans are changed by owners and managers of any group, while edits and rotations need every
/// group, and the refusal names only blocking groups the caller can see
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn shared_entity_rights() -> Result<(), Error> {
    // a manager of only a who is a user in b, an owner of only a who is a user in b, a manager of
    // only b, an owner of both, and a plain member of both
    let admin = test_utilities::admin_client().await?;
    let manager_a = plain(&admin).await?;
    let owner_a = plain(&admin).await?;
    let manager_b = plain(&admin).await?;
    let owner_both = plain(&admin).await?;
    let member = plain(&admin).await?;
    let only_a = plain(&admin).await?;
    let a = group(
        GroupRoles::default()
            .manager(&manager_a.username)
            .owner(&owner_a.username)
            .owner(&owner_both.username)
            .user(&member.username)
            .user(&only_a.username),
        &admin,
    )
    .await?;
    let b = group(
        GroupRoles::default()
            .user(&manager_a.username)
            .user(&owner_a.username)
            .manager(&manager_b.username)
            .owner(&owner_both.username)
            .user(&member.username),
        &admin,
    )
    .await?;
    let hidden = generators::groups(1, &admin).await?.remove(0).name;
    let cred_id = cred(&admin, &[&a, &b], CredentialKind::ApiKey).await?;
    let endpoint = generators::model_endpoint(
        &[&a, &b],
        anthropic(Some(cred_id), "claude-a"),
        &owner_both.client,
    )
    .await?;
    // a manager or an owner of only a can't edit, naming b
    let cred_name = admin.entities.get(cred_id).await?.name;
    for partial in [&manager_a, &owner_a] {
        let resp = partial
            .client
            .entities
            .update(endpoint.id, EntityUpdate::default().description("x"))
            .await;
        fail!(resp, 401);
        let msg = refusal(resp);
        is!(
            msg,
            format!(
                "you don't have permission to edit `{}` in `{b}`",
                endpoint.name
            )
        );
        // or rotate the credential
        let resp = update(
            &partial.client,
            cred_id,
            EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
        )
        .await;
        fail!(resp, 401);
        let msg = refusal(resp);
        is!(
            msg,
            format!("you don't have permission to edit `{cred_name}` in `{b}`")
        );
    }
    // a hidden blocking group is neither named nor counted
    let hidden_endpoint =
        generators::model_endpoint(&[&a, &hidden], anthropic(None, "claude-a"), &admin).await?;
    let resp = manager_a
        .client
        .entities
        .update(hidden_endpoint.id, EntityUpdate::default().description("x"))
        .await;
    fail!(resp, 401);
    let msg = refusal(resp);
    is!(
        msg,
        format!(
            "you don't have permission to edit `{}`",
            hidden_endpoint.name
        )
    );
    // a manager of only b adds a generic ban, and an owner of only a clears it
    let ban = ModelBan::generic("paused by b");
    update(
        &manager_b.client,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().add_ban(ban.clone())),
    )
    .await?;
    for reader in [&manager_a, &manager_b, &member, &only_a] {
        let read = reader.client.entities.get(endpoint.id).await?;
        is!(
            endpoint_meta(&read)
                .bans
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![ban.id]
        );
    }
    // the ban is stored in both groups' rows
    for (group, data) in rows(endpoint.id).await? {
        assert!(
            data.contains(&ban.id.to_string()),
            "no ban in the {group} row"
        );
    }
    update(
        &owner_a.client,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().remove_ban(ban.id)),
    )
    .await?;
    for reader in [&manager_b, &member, &only_a] {
        assert!(
            endpoint_meta(&reader.client.entities.get(endpoint.id).await?)
                .bans
                .is_empty()
        );
    }
    for (group, data) in rows(endpoint.id).await? {
        assert!(
            !data.contains(&ban.id.to_string()),
            "the ban stayed in the {group} row"
        );
    }
    // a ban on a catalog model works the same way
    let model_ban = ModelBan::generic("model paused");
    update(
        &manager_a.client,
        endpoint.id,
        endpoint_update().catalog_bans(
            "claude-a",
            ModelBanUpdate::default().add_ban(model_ban.clone()),
        ),
    )
    .await?;
    let read = admin.entities.get(endpoint.id).await?;
    assert!(
        endpoint_meta(&read).catalog[0]
            .bans
            .contains_key(&model_ban.id)
    );
    // a ban change that also edits the endpoint is refused as an edit, and nothing is applied
    let mixed = [
        endpoint_update()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x")))
            .disable_model("claude-a"),
        endpoint_update()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x")))
            .endpoint_provider(generators::gen_anthropic_provider()),
    ];
    for meta in mixed {
        let resp = update(&manager_a.client, endpoint.id, meta).await;
        fail!(resp, 401);
        let read = admin.entities.get(endpoint.id).await?;
        assert!(endpoint_meta(&read).bans.is_empty());
        assert!(endpoint_meta(&read).disabled_models.is_empty());
    }
    // a plain member can't add or clear bans
    let resp = update(
        &member.client,
        endpoint.id,
        endpoint_update().catalog_bans(
            "claude-a",
            ModelBanUpdate::default().remove_ban(model_ban.id),
        ),
    )
    .await;
    fail!(resp, 401);
    let resp = update(
        &member.client,
        endpoint.id,
        endpoint_update()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x"))),
    )
    .await;
    fail!(resp, 401);
    // only admins add the bans Thorium registers itself, and admins clear any ban
    let unreachable = ModelBan::new(ModelBanKind::EndpointUnreachable("down".into()));
    let resp = update(
        &owner_both.client,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().add_ban(unreachable.clone())),
    )
    .await;
    fail!(resp, 400);
    let msg = refusal(resp);
    assert!(msg.contains("EndpointUnreachable"), "{msg}");
    update(
        &admin,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().add_ban(unreachable.clone())),
    )
    .await?;
    update(
        &admin,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().remove_ban(unreachable.id)),
    )
    .await?;
    update(
        &admin,
        endpoint.id,
        endpoint_update().catalog_bans(
            "claude-a",
            ModelBanUpdate::default().remove_ban(model_ban.id),
        ),
    )
    .await?;
    // removing a ban that doesn't exist is a 404
    let resp = update(
        &owner_a.client,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().remove_ban(Uuid::new_v4())),
    )
    .await;
    fail!(resp, 404);
    // an owner of both groups edits the endpoint
    update(
        &owner_both.client,
        endpoint.id,
        endpoint_update().disable_model("claude-a"),
    )
    .await?;
    let read = admin.entities.get(endpoint.id).await?;
    is!(
        endpoint_meta(&read).disabled_models.clone(),
        BTreeSet::from(["claude-a".to_owned()])
    );
    // a profile follows the shared edit rule: editors of every group edit it, managers change
    // its bans, and plain members can't
    let profile = generators::model_profile(
        &[&a, &b],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages),
        &member.client,
    )
    .await?;
    update(
        &member.client,
        profile.id,
        EntityMetadataUpdate::model_profile().clear_profile_limits(),
    )
    .await?;
    let resp = update(
        &member.client,
        profile.id,
        EntityMetadataUpdate::model_profile()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x"))),
    )
    .await;
    fail!(resp, 401);
    update(
        &manager_b.client,
        profile.id,
        EntityMetadataUpdate::model_profile()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x"))),
    )
    .await?;
    let monitor = plain(&admin).await?;
    admin
        .groups
        .update(
            &b,
            &GroupUpdate::default()
                .monitors(GroupUsersUpdate::default().direct_add(&monitor.username)),
        )
        .await?;
    admin
        .groups
        .update(
            &a,
            &GroupUpdate::default()
                .users(GroupUsersUpdate::default().direct_add(&monitor.username)),
        )
        .await?;
    let resp = update(
        &monitor.client,
        profile.id,
        EntityMetadataUpdate::model_profile().clear_profile_limits(),
    )
    .await;
    fail!(resp, 401);
    Ok(())
}

/// A shared credential keeps one identical secret in every group's row, and rotations follow
/// the secret rules
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn shared_secrets() -> Result<(), Error> {
    // an owner of a and b, and a manager of only a
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let manager_a = plain(&admin).await?;
    let reader_a = plain(&admin).await?;
    let reader_b = plain(&admin).await?;
    let a = group(
        GroupRoles::default()
            .owner(&owner.username)
            .manager(&manager_a.username)
            .user(&reader_a.username),
        &admin,
    )
    .await?;
    let b = group(
        GroupRoles::default()
            .owner(&owner.username)
            .user(&reader_b.username),
        &admin,
    )
    .await?;
    let first = gen_secret();
    let access = CredentialKind::AwsAccessKey {
        access_key_id: "AKIA".into(),
    };
    let mut req = ModelCredentialEntityRequest::new(access, Some(first.as_str().into()));
    req.session_token = Some("session-token-for-test".into());
    req.expires = Some(Utc::now());
    let resp = owner
        .client
        .entities
        .create(generators::gen_model_entity(
            &[&a, &b],
            EntityMetadataRequest::ModelCredential(req),
        ))
        .await?;
    let id = resp.id;
    // both rows hold the same secret
    let before = rows(id).await?;
    is!(before.len(), 2);
    assert!(before[0].1 == before[1].1, "the group rows differ");
    assert!(
        before.iter().all(|(_, data)| data.contains(&first)),
        "a row lost the secret"
    );
    // a manager of only a can't rotate it, and both rows stay unchanged
    let resp = update(
        &manager_a.client,
        id,
        EntityMetadataUpdate::model_credential().credential_secret(gen_secret()),
    )
    .await;
    fail!(resp, 401);
    assert!(
        rows(id).await? == before,
        "a refused rotation changed a row"
    );
    // an owner of both rotates it, which rewrites both rows and clears rejection bans
    for kind in [
        ModelBanKind::CredentialRejected {
            status: 401,
            message: "invalid".into(),
        },
        ModelBanKind::CredentialExpired,
        ModelBanKind::Generic("kept".into()),
    ] {
        update(
            &admin,
            id,
            EntityMetadataUpdate::model_credential()
                .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::new(kind))),
        )
        .await?;
    }
    let rotated_before = credential_meta(&admin.entities.get(id).await?).rotated;
    assert!(rotated_before.is_some());
    let second = gen_secret();
    update(
        &owner.client,
        id,
        EntityMetadataUpdate::model_credential().credential_secret(second.as_str()),
    )
    .await?;
    let after = rows(id).await?;
    assert!(
        after[0].1 == after[1].1,
        "the group rows differ after rotation"
    );
    assert!(
        after
            .iter()
            .all(|(_, data)| data.contains(&second) && !data.contains(&first)),
        "a row holds the wrong secret"
    );
    // members of a and b read the same new fingerprint and rotation time
    let read_a = reader_a.client.entities.get(id).await?;
    let read_b = reader_b.client.entities.get(id).await?;
    let meta = credential_meta(&read_a);
    is!(
        meta.secrets.fingerprint.clone(),
        Some(ModelSecret::from(second.as_str()).fingerprint())
    );
    is!(
        meta.secrets.fingerprint.clone(),
        credential_meta(&read_b).secrets.fingerprint.clone()
    );
    is!(meta.rotated, credential_meta(&read_b).rotated);
    assert!(
        meta.rotated > rotated_before,
        "the rotation time didn't move"
    );
    let kinds = meta
        .bans
        .values()
        .map(|ban| ban.ban_kind.clone())
        .collect::<Vec<_>>();
    is!(kinds, vec![ModelBanKind::Generic("kept".into())]);
    // an update without a secret keeps the stored one
    update(
        &owner.client,
        id,
        EntityMetadataUpdate::model_credential().credential_expires(Utc::now()),
    )
    .await?;
    assert!(
        rows(id)
            .await?
            .iter()
            .all(|(_, data)| data.contains(&second)),
        "an update dropped the secret"
    );
    let read = admin.entities.get(id).await?;
    is!(
        credential_meta(&read).secrets.fingerprint.clone(),
        Some(ModelSecret::from(second.as_str()).fingerprint())
    );
    // clearing the session token removes only it, and clearing the expiration unsets it
    update(
        &owner.client,
        id,
        EntityMetadataUpdate::model_credential()
            .clear_credential_session_token()
            .clear_credential_expires(),
    )
    .await?;
    let stored = rows(id).await?;
    assert!(
        stored
            .iter()
            .all(|(_, data)| data.contains(&second) && !data.contains("session-token-for-test")),
        "the session token wasn't cleared alone"
    );
    is!(
        credential_meta(&admin.entities.get(id).await?).expires,
        None::<chrono::DateTime<Utc>>
    );
    // an empty rotation is refused
    let resp = update(
        &owner.client,
        id,
        EntityMetadataUpdate::model_credential().credential_secret(""),
    )
    .await;
    fail!(resp, 400);
    // every kind that needs a secret refuses a missing or empty one
    let kinds = [
        CredentialKind::ApiKey,
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        CredentialKind::AwsBearerToken,
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        CredentialKind::AzureEntraClient {
            tenant_id: "t".into(),
            client_id: "c".into(),
            auth: AzureClientAuth::Certificate,
        },
    ];
    for kind in kinds {
        let resp = generators::model_credential(&[&a], kind.clone(), None, &owner.client).await;
        fail!(resp, 400);
        let resp = generators::model_credential(&[&a], kind, Some(""), &owner.client).await;
        fail!(resp, 400);
    }
    // an assumed role needs no secret
    let role = CredentialKind::AwsAssumeRole {
        role_arn: "arn:aws:iam::1:role/r".into(),
        external_id: None,
        session_name: None,
        duration_seconds: None,
        source: AwsRoleSource::AccessKey(id),
    };
    let assumed = generators::model_credential(&[&a], role, None, &owner.client).await?;
    is!(
        credential_meta(&assumed).secrets.fingerprint.clone(),
        None::<String>
    );
    Ok(())
}

/// No entity response holds a secret, for members or admins, while the stored rows keep it
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn responses_never_hold_secrets() -> Result<(), Error> {
    // a credential with a secret and a session token in a group
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let member = plain(&admin).await?;
    let raw_admin = user(&admin, UserRole::Admin).await?;
    let g = group(
        GroupRoles::default()
            .owner(&owner.username)
            .user(&member.username),
        &admin,
    )
    .await?;
    let secret = gen_secret();
    let token = gen_secret();
    let mut req = ModelCredentialEntityRequest::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        Some(secret.as_str().into()),
    );
    req.session_token = Some(token.as_str().into());
    let id = owner
        .client
        .entities
        .create(generators::gen_model_entity(
            &[&g],
            EntityMetadataRequest::ModelCredential(req),
        ))
        .await?
        .id;
    let endpoint = generators::model_endpoint(
        &[&g],
        ModelEndpointEntityRequest::new(bedrock()).credential(id),
        &owner.client,
    )
    .await?;
    // the get, list, and details routes return only the fingerprint
    let fingerprint = ModelSecret::from(secret.as_str()).fingerprint();
    let paths = [
        format!("/api/entities/{id}"),
        format!("/api/entities/details/?groups={g}&kinds=ModelCredential"),
        format!("/api/entities/?groups={g}&kinds=ModelCredential"),
        format!("/api/entities/{}", endpoint.id),
    ];
    for reader in [&member, &raw_admin] {
        for path in &paths {
            let body = raw_get(reader, path).await?;
            assert_no_secret(&body, &[&secret, &token], path);
        }
        let body = raw_get(reader, &paths[0]).await?;
        assert!(body.contains(&fingerprint), "the get lost the fingerprint");
        // the list and details bodies name the credential, and details keep its fingerprint
        for path in &paths[1..3] {
            let body = raw_get(reader, path).await?;
            assert!(body.contains(&id.to_string()), "{path} lost the credential");
        }
        let body = raw_get(reader, &paths[1]).await?;
        assert!(
            body.contains(&fingerprint),
            "the details lost the fingerprint"
        );
    }
    // the client's view agrees
    let listed = member
        .client
        .entities
        .list_details(
            &EntityListOpts::default()
                .groups(vec![g.clone()])
                .kind(EntityKinds::ModelCredential),
        )
        .await?;
    is!(listed.data.len(), 1);
    is!(
        credential_meta(&listed.data[0]).secrets.primary.clone(),
        None::<ModelSecret>
    );
    is!(
        credential_meta(&listed.data[0])
            .secrets
            .session_token
            .clone(),
        None::<ModelSecret>
    );
    // trees grown from the endpoint or the credential hold no secret either
    for start in [endpoint.id, id] {
        let query = TreeQuery::default().entity(start);
        let tree = member
            .client
            .trees
            .start(&TreeOpts::default(), &query)
            .await?;
        let body = serde_json::to_string(&tree)?;
        assert_no_secret(&body, &[&secret, &token], "a tree");
        assert!(
            body.contains(&id.to_string()),
            "the tree didn't reach the credential"
        );
    }
    // an endpoint update stores no secret of its own and the credential's rows keep theirs
    owner
        .client
        .entities
        .update(endpoint.id, EntityUpdate::default().description("changed"))
        .await?;
    let stored = rows(id).await?;
    assert!(
        stored
            .iter()
            .all(|(_, data)| data.contains(&secret) && data.contains(&token)),
        "a row lost a secret"
    );
    Ok(())
}

/// Endpoints and profiles mirror their references with associations that block deletes, move
/// with the reference, need the target to cover the referrer, and can't be written by hand
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn integrity_associations() -> Result<(), Error> {
    // an owner of a and b
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let a = group(GroupRoles::default().owner(&owner.username), &admin).await?;
    let b = group(GroupRoles::default().owner(&owner.username), &admin).await?;
    let first = cred(&admin, &[&a, &b], CredentialKind::ApiKey).await?;
    let second = cred(&admin, &[&a, &b], CredentialKind::ApiKey).await?;
    let endpoint =
        generators::model_endpoint(&[&a, &b], anthropic(Some(first), "claude-a"), &owner.client)
            .await?;
    let profile = generators::model_profile(
        &[&a],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages),
        &owner.client,
    )
    .await?;
    // the endpoint authenticates with the credential and the profile serves the endpoint
    is!(
        edges(&admin, endpoint.id, AssociationKind::AuthenticatesWith).await?,
        vec![(first, {
            let mut groups = vec![a.clone(), b.clone()];
            groups.sort();
            groups
        })]
    );
    is!(
        edges(&admin, profile.id, AssociationKind::Serves).await?,
        vec![(endpoint.id, vec![a.clone()])]
    );
    // deleting a referenced credential or endpoint is refused, naming the referrer
    let resp = owner.client.entities.delete(first).await;
    fail!(resp, 409);
    assert!(refusal(resp).contains(&endpoint.name));
    let resp = owner.client.entities.delete(endpoint.id).await;
    fail!(resp, 409);
    assert!(refusal(resp).contains(&profile.name));
    // neither kind can be written through the generic association route, by anyone
    let source = AssociationTarget::Entity {
        id: endpoint.id,
        name: endpoint.name.clone(),
    };
    let target = AssociationTarget::Entity {
        id: second,
        name: "second".into(),
    };
    for (kind, field) in [
        (AssociationKind::AuthenticatesWith, "credential"),
        (AssociationKind::Serves, "endpoint"),
    ] {
        for client in [&admin, &owner.client] {
            let req = AssociationRequest::new(kind, source.clone()).target(target.clone());
            let resp = client.associations.create(&req).await;
            fail!(resp, 400);
            let msg = refusal(resp);
            assert!(msg.contains(kind.as_str()) && msg.contains(field), "{msg}");
        }
    }
    is!(
        edges(&admin, endpoint.id, AssociationKind::AuthenticatesWith)
            .await?
            .len(),
        1
    );
    // moving the endpoint's credential moves the edge
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().endpoint_credential(second),
    )
    .await?;
    owner.client.entities.delete(first).await?;
    let resp = owner.client.entities.delete(second).await;
    fail!(resp, 409);
    // clearing it removes the edge
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().clear_endpoint_credential(),
    )
    .await?;
    is!(
        edges(&admin, endpoint.id, AssociationKind::AuthenticatesWith).await?,
        Vec::<(Uuid, Vec<String>)>::new()
    );
    is!(
        endpoint_meta(&admin.entities.get(endpoint.id).await?).credential,
        None::<Uuid>
    );
    owner.client.entities.delete(second).await?;
    // moving the profile's endpoint moves its edge, and a model outside the catalog is refused
    let other =
        generators::model_endpoint(&[&a], anthropic(None, "claude-b"), &owner.client).await?;
    let resp = update(
        &owner.client,
        profile.id,
        EntityMetadataUpdate::model_profile().profile_endpoint(other.id),
    )
    .await;
    fail!(resp, 400);
    update(
        &owner.client,
        profile.id,
        EntityMetadataUpdate::model_profile()
            .profile_endpoint(other.id)
            .profile_model("claude-b"),
    )
    .await?;
    is!(
        edges(&admin, profile.id, AssociationKind::Serves).await?,
        vec![(other.id, vec![a.clone()])]
    );
    let resp = owner.client.entities.delete(other.id).await;
    fail!(resp, 409);
    let resp = update(
        &owner.client,
        profile.id,
        EntityMetadataUpdate::model_profile().profile_model("claude-missing"),
    )
    .await;
    fail!(resp, 400);
    // a reference whose target doesn't cover the referrer's groups is refused, naming them
    let narrow = cred(&admin, &[&a], CredentialKind::ApiKey).await?;
    let resp = generators::model_endpoint(
        &[&a, &b],
        anthropic(Some(narrow), "claude-a"),
        &owner.client,
    )
    .await;
    fail!(resp, 400);
    assert!(refusal(resp).contains(&b));
    let resp = generators::model_profile(
        &[&a, &b],
        ModelProfileEntityRequest::new(other.id, "claude-b", ModelApi::AnthropicMessages),
        &owner.client,
    )
    .await;
    fail!(resp, 400);
    assert!(refusal(resp).contains(&b));
    // once their referrers are gone both delete
    owner.client.entities.delete(endpoint.id).await?;
    owner.client.entities.delete(profile.id).await?;
    owner.client.entities.delete(other.id).await?;
    Ok(())
}

/// Group changes keep references contained: a target can't lose a group a referrer in it needs,
/// and a referrer can't gain a group its target isn't in
#[tokio::test]
async fn group_changes() -> Result<(), Error> {
    // endpoint e in a and b authenticates with credential c in a and b; profile p in b serves e
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let a = group(GroupRoles::default().owner(&owner.username), &admin).await?;
    let b = group(GroupRoles::default().owner(&owner.username), &admin).await?;
    let d = group(GroupRoles::default().owner(&owner.username), &admin).await?;
    let cred_id = cred(&admin, &[&a, &b], CredentialKind::ApiKey).await?;
    let endpoint = generators::model_endpoint(
        &[&a, &b],
        anthropic(Some(cred_id), "claude-a"),
        &owner.client,
    )
    .await?;
    let profile = generators::model_profile(
        &[&b],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages),
        &owner.client,
    )
    .await?;
    // removing b from c is refused naming e, and from e naming p
    let resp = owner
        .client
        .entities
        .update(cred_id, EntityUpdate::default().remove_group(&b))
        .await;
    fail!(resp, 409);
    assert!(refusal(resp).contains(&endpoint.name));
    let resp = owner
        .client
        .entities
        .update(endpoint.id, EntityUpdate::default().remove_group(&b))
        .await;
    fail!(resp, 409);
    assert!(refusal(resp).contains(&profile.name));
    // adding d to p while e isn't in d is refused naming d
    let resp = owner
        .client
        .entities
        .update(profile.id, EntityUpdate::default().group(&d))
        .await;
    fail!(resp, 400);
    assert!(refusal(resp).contains(&d));
    // adding d to c, then e, then p succeeds and the edges follow
    owner
        .client
        .entities
        .update(cred_id, EntityUpdate::default().group(&d))
        .await?;
    owner
        .client
        .entities
        .update(endpoint.id, EntityUpdate::default().group(&d))
        .await?;
    owner
        .client
        .entities
        .update(profile.id, EntityUpdate::default().group(&d))
        .await?;
    let serves = edges(&admin, profile.id, AssociationKind::Serves).await?;
    assert!(
        serves
            .iter()
            .any(|(id, groups)| *id == endpoint.id && groups.contains(&d))
    );
    // after p leaves b, removing b from e succeeds
    owner
        .client
        .entities
        .update(profile.id, EntityUpdate::default().remove_group(&b))
        .await?;
    owner
        .client
        .entities
        .update(endpoint.id, EntityUpdate::default().remove_group(&b))
        .await?;
    let mut groups = admin.entities.get(endpoint.id).await?.groups;
    groups.sort();
    let mut expected = vec![a.clone(), d.clone()];
    expected.sort();
    is!(groups, expected);
    // a deleted profile no longer blocks removing a group either
    let second_profile = generators::model_profile(
        &[&d],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages),
        &owner.client,
    )
    .await?;
    let resp = owner
        .client
        .entities
        .update(endpoint.id, EntityUpdate::default().remove_group(&d))
        .await;
    fail!(resp, 409);
    owner.client.entities.delete(second_profile.id).await?;
    owner.client.entities.delete(profile.id).await?;
    owner
        .client
        .entities
        .update(endpoint.id, EntityUpdate::default().remove_group(&d))
        .await?;
    Ok(())
}

/// Catalog entries are added as manual entries, removed with their bans unless a profile calls
/// them, and disabled ids stay listed and change only by owners and managers
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn catalog_edits() -> Result<(), Error> {
    // an owner and a plain member of a group
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let member = plain(&admin).await?;
    let g = group(
        GroupRoles::default()
            .owner(&owner.username)
            .user(&member.username),
        &admin,
    )
    .await?;
    // a create request repeating a model id is refused naming it
    let repeated = anthropic(None, "claude-a").model(ModelCatalogEntryRequest::new("claude-a"));
    let resp = generators::model_endpoint(&[&g], repeated, &owner.client).await;
    fail!(resp, 400);
    assert!(refusal(resp).contains("claude-a"));
    // a new id is inserted sorted as a manual entry
    let endpoint =
        generators::model_endpoint(&[&g], anthropic(None, "claude-m"), &owner.client).await?;
    let mut entry = ModelCatalogEntryRequest::new("claude-b");
    entry.context_window = Some(100_000);
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().add_catalog_model(entry),
    )
    .await?;
    let read = owner.client.entities.get(endpoint.id).await?;
    let meta = endpoint_meta(&read);
    let ids = meta
        .catalog
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect::<Vec<_>>();
    is!(ids, vec!["claude-b", "claude-m"]);
    is!(meta.catalog[0].source.clone(), CatalogEntrySource::Manual);
    is!(meta.catalog[0].limits.source.clone(), LimitsSource::Manual);
    is!(meta.catalog[0].limits.context_window, Some(100_000));
    // a model a profile calls can't be removed
    let profile = generators::model_profile(
        &[&g],
        ModelProfileEntityRequest::new(endpoint.id, "claude-m", ModelApi::AnthropicMessages),
        &owner.client,
    )
    .await?;
    let resp = update(
        &owner.client,
        endpoint.id,
        endpoint_update().remove_catalog_model("claude-m"),
    )
    .await;
    fail!(resp, 409);
    assert!(refusal(resp).contains(&profile.name));
    // removing an entry removes its bans
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().catalog_bans(
            "claude-b",
            ModelBanUpdate::default().add_ban(ModelBan::generic("x")),
        ),
    )
    .await?;
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().remove_catalog_model("claude-b"),
    )
    .await?;
    let read = owner.client.entities.get(endpoint.id).await?;
    is!(endpoint_meta(&read).catalog.len(), 1);
    let resp = update(
        &owner.client,
        endpoint.id,
        endpoint_update().catalog_bans(
            "claude-b",
            ModelBanUpdate::default().add_ban(ModelBan::generic("x")),
        ),
    )
    .await;
    fail!(resp, 404);
    // disabling and enabling ids adds and removes them, and the entry stays listed
    update(
        &owner.client,
        endpoint.id,
        endpoint_update()
            .disable_model("claude-m")
            .disable_model("claude-latest"),
    )
    .await?;
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().enable_model("claude-latest"),
    )
    .await?;
    let read = member.client.entities.get(endpoint.id).await?;
    is!(
        endpoint_meta(&read).disabled_models.clone(),
        BTreeSet::from(["claude-m".to_owned()])
    );
    assert!(endpoint_meta(&read).catalog_entry("claude-m").is_some());
    // a plain member can't change them
    let resp = update(
        &member.client,
        endpoint.id,
        endpoint_update().enable_model("claude-m"),
    )
    .await;
    fail!(resp, 401);
    // clearing bans leaves them unchanged
    let ban = ModelBan::generic("x");
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().add_ban(ban.clone())),
    )
    .await?;
    update(
        &owner.client,
        endpoint.id,
        endpoint_update().model_bans_update(ModelBanUpdate::default().remove_ban(ban.id)),
    )
    .await?;
    let read = owner.client.entities.get(endpoint.id).await?;
    is!(
        endpoint_meta(&read).disabled_models.clone(),
        BTreeSet::from(["claude-m".to_owned()])
    );
    // an id can't be disabled and enabled at once
    let resp = update(
        &owner.client,
        endpoint.id,
        endpoint_update().disable_model("x").enable_model("x"),
    )
    .await;
    fail!(resp, 400);
    Ok(())
}

/// Profile requests are checked against the endpoint's catalog and the model's limits and
/// capabilities, and their overrides can be cleared
#[tokio::test]
async fn profile_requests() -> Result<(), Error> {
    // an endpoint whose model can't think and produces at most 8192 tokens
    let admin = test_utilities::admin_client().await?;
    let g = generators::groups(1, &admin).await?.remove(0).name;
    let endpoint = generators::model_endpoint(&[&g], anthropic(None, "claude-a"), &admin).await?;
    let base =
        || ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages);
    // a model outside the catalog is refused
    let resp = generators::model_profile(
        &[&g],
        ModelProfileEntityRequest::new(endpoint.id, "claude-z", ModelApi::AnthropicMessages),
        &admin,
    )
    .await;
    fail!(resp, 400);
    // invalid JSON is refused
    let mut bad_params = base();
    bad_params.parameters = Some("{not json".into());
    let resp = generators::model_profile(&[&g], bad_params, &admin).await;
    fail!(resp, 400);
    let mut bad_limits = base();
    bad_limits.limits = Some(r#"{"context_window": "many"}"#.into());
    let resp = generators::model_profile(&[&g], bad_limits, &admin).await;
    fail!(resp, 400);
    // parameters that don't fit the model are refused
    let thinking = ModelParameters {
        thinking_budget_tokens: Some(1024),
        ..ModelParameters::default()
    };
    let resp = generators::model_profile(&[&g], base().parameters(&thinking)?, &admin).await;
    fail!(resp, 400);
    let capped = ModelParameters {
        max_output_tokens: Some(9000),
        ..ModelParameters::default()
    };
    let resp = generators::model_profile(&[&g], base().parameters(&capped)?, &admin).await;
    fail!(resp, 400);
    // limits are stored as manual whatever source they name, and float parameters round trip
    let mut req = base().parameters(&ModelParameters {
        temperature: Some(0.2),
        ..ModelParameters::default()
    })?;
    req.limits = Some(
        serde_json::json!({"context_window": 1000, "max_output_tokens": 500, "source": {"Refreshed": {"at": Utc::now()}}})
            .to_string(),
    );
    req.capabilities = Some(ModelCapabilities {
        thinking: true,
        ..ModelCapabilities::default()
    });
    let profile = generators::model_profile(&[&g], req, &admin).await?;
    let meta = profile_meta(&profile);
    is!(meta.parameters.temperature, Some(0.2));
    let limits = meta.limits.clone().expect("the profile lost its limits");
    is!(limits.source, LimitsSource::Manual);
    is!(limits.max_output_tokens, Some(500));
    // a profile that overrides capabilities may use them
    update(
        &admin,
        profile.id,
        EntityMetadataUpdate::model_profile().profile_parameters(thinking.clone()),
    )
    .await?;
    // clearing the overrides returns to the catalog entry's values, which can't think
    let resp = update(
        &admin,
        profile.id,
        EntityMetadataUpdate::model_profile().clear_profile_capabilities(),
    )
    .await;
    fail!(resp, 400);
    update(
        &admin,
        profile.id,
        EntityMetadataUpdate::model_profile()
            .profile_parameters(ModelParameters::default())
            .clear_profile_capabilities()
            .clear_profile_limits(),
    )
    .await?;
    let read = admin.entities.get(profile.id).await?;
    is!(profile_meta(&read).limits.clone(), None::<ModelLimits>);
    is!(
        profile_meta(&read).capabilities.clone(),
        None::<ModelCapabilities>
    );
    Ok(())
}

/// Profile parameters with a key the API doesn't know are accepted on create and update and the
/// key is dropped, so a newer client sending a new parameter works against an older API
#[tokio::test]
async fn profile_parameters_drop_unknown_keys() -> Result<(), Error> {
    // a group named for this test and an endpoint in it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let g = generators::readable_name("minerva", &suffix);
    admin
        .groups
        .create(&GroupRequest::new(&g).owners(GroupUsersRequest::default().direct("thorium")))
        .await?;
    let endpoint = generators::model_endpoint(&[&g], anthropic(None, "claude-a"), &admin).await?;
    let base =
        || ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages);
    // a create whose parameters hold an unknown key keeps the known keys and drops the rest
    let mut unknown = base();
    unknown.parameters = Some(r#"{"temperature": 0.5, "max_tokens": 100}"#.into());
    let created = generators::model_profile(&[&g], unknown, &admin).await?;
    let read = admin.entities.get(created.id).await?;
    let expected = ModelParameters {
        temperature: Some(0.5),
        ..ModelParameters::default()
    };
    is!(profile_meta(&read).parameters.clone(), expected.clone());
    // an update whose parameters hold an unknown key is applied without it
    let profile = generators::model_profile(&[&g], base(), &admin).await?;
    let sender = generators::named_user_with_role(
        &admin,
        &generators::readable_name("io", &suffix),
        UserRole::Admin,
    )
    .await?;
    let form = reqwest::multipart::Form::new().text(
        "metadata[model_profile_parameters]",
        r#"{"temperature": 0.5, "max_tokens": 100}"#,
    );
    let resp = reqwest::Client::new()
        .patch(format!("{}/api/entities/{}", admin.host, profile.id))
        .header("authorization", sender.auth_header())
        .multipart(form)
        .send()
        .await?;
    let status = resp.status().as_u16();
    is!(status, 204, resp.text().await?);
    let read = admin.entities.get(profile.id).await?;
    is!(profile_meta(&read).parameters.clone(), expected);
    Ok(())
}

/// Deleting an access key bans the assumed roles that use it, and a reference found missing on
/// an update bans its referrer until it's fixed
#[tokio::test]
async fn broken_references() -> Result<(), Error> {
    // an access key in a and b used by roles in a and in a and b, and another key's role
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let access = |id: &str| CredentialKind::AwsAccessKey {
        access_key_id: id.to_owned(),
    };
    let role = |source| CredentialKind::AwsAssumeRole {
        role_arn: "arn:aws:iam::1:role/r".into(),
        external_id: None,
        session_name: None,
        duration_seconds: None,
        source: AwsRoleSource::AccessKey(source),
    };
    let key = cred(&admin, &[&a, &b], access("AKIA")).await?;
    let other_key = cred(&admin, &[&a, &b], access("AKIB")).await?;
    let narrow_role = cred(&admin, &[&a], role(key)).await?;
    let wide_role = cred(&admin, &[&a, &b], role(key)).await?;
    let other_role = cred(&admin, &[&a], role(other_key)).await?;
    // deleting the key succeeds and bans both of its roles at once
    admin.entities.delete(key).await?;
    for id in [narrow_role, wide_role] {
        let read = admin.entities.get(id).await?;
        let kinds = credential_meta(&read)
            .bans
            .values()
            .map(|ban| ban.ban_kind.clone())
            .collect::<Vec<_>>();
        is!(kinds, vec![ModelBanKind::BrokenReference(key)]);
    }
    // a role with another source keeps no ban
    assert!(
        credential_meta(&admin.entities.get(other_role).await?)
            .bans
            .is_empty()
    );
    // deleting a credential of another kind bans nothing
    let api_key = cred(&admin, &[&a], CredentialKind::ApiKey).await?;
    admin.entities.delete(api_key).await?;
    assert!(
        credential_meta(&admin.entities.get(other_role).await?)
            .bans
            .is_empty()
    );
    // a banned role can still be deleted
    admin.entities.delete(narrow_role).await?;
    let resp = admin.entities.get(narrow_role).await;
    fail!(resp, 404);
    // a credential that vanished without its edge bans the endpoint on its next update
    let lost = cred(&admin, &[&a], CredentialKind::ApiKey).await?;
    let endpoint =
        generators::model_endpoint(&[&a], anthropic(Some(lost), "claude-a"), &admin).await?;
    test_utilities::delete_entity_rows(lost).await?;
    admin
        .entities
        .update(endpoint.id, EntityUpdate::default().description("touched"))
        .await?;
    let read = admin.entities.get(endpoint.id).await?;
    let kinds = endpoint_meta(&read)
        .bans
        .values()
        .map(|ban| ban.ban_kind.clone())
        .collect::<Vec<_>>();
    is!(kinds, vec![ModelBanKind::BrokenReference(lost)]);
    // setting a valid credential clears the ban
    let fixed = cred(&admin, &[&a], CredentialKind::ApiKey).await?;
    update(
        &admin,
        endpoint.id,
        endpoint_update().endpoint_credential(fixed),
    )
    .await?;
    assert!(
        endpoint_meta(&admin.entities.get(endpoint.id).await?)
            .bans
            .is_empty()
    );
    // an endpoint's submitter who is now a plain member can't delete it
    let manager = plain(&admin).await?;
    let g = group(GroupRoles::default().manager(&manager.username), &admin).await?;
    let mine =
        generators::model_endpoint(&[&g], anthropic(None, "claude-a"), &manager.client).await?;
    admin
        .groups
        .update(
            &g,
            &GroupUpdate::default()
                .managers(GroupUsersUpdate::default().direct_remove(&manager.username))
                .users(GroupUsersUpdate::default().direct_add(&manager.username)),
        )
        .await?;
    let resp = manager.client.entities.delete(mine.id).await;
    fail!(resp, 401);
    Ok(())
}

/// Count the stored notifications of one kind for one key
///
/// # Arguments
///
/// * `kind` - The notification type
/// * `id` - The entity the notifications are for
async fn stored_notes(kind: NotificationType, id: Uuid) -> Result<usize, Error> {
    test_utilities::notification_rows(kind, &id.to_string()).await
}

/// Bans on endpoints, catalog models, profiles, and credentials each write one notification under
/// their kind's type, members of the entity's groups read them, and removing a ban or its catalog
/// entry deletes it
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn entity_ban_notifications() -> Result<(), Error> {
    // an endpoint, profile, and credential in groups a and b with an owner, members, and a user
    // of another group
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let member_a = plain(&admin).await?;
    let member_b = plain(&admin).await?;
    let outsider = plain(&admin).await?;
    let a = group(
        GroupRoles::default()
            .owner(&owner.username)
            .user(&member_a.username),
        &admin,
    )
    .await?;
    let b = group(
        GroupRoles::default()
            .owner(&owner.username)
            .user(&member_b.username),
        &admin,
    )
    .await?;
    group(GroupRoles::default().user(&outsider.username), &admin).await?;
    let credential = cred(&admin, &[&a, &b], CredentialKind::ApiKey).await?;
    let mut req = anthropic(Some(credential), "claude-a");
    req = req.model(ModelCatalogEntryRequest::new("claude-b"));
    let endpoint = generators::model_endpoint(&[&a, &b], req, &owner.client)
        .await?
        .id;
    let profile = generators::model_profile(
        &[&a, &b],
        ModelProfileEntityRequest::new(endpoint, "claude-a", ModelApi::AnthropicMessages),
        &owner.client,
    )
    .await?
    .id;
    // an entity with no bans has no notifications
    is!(
        member_a
            .client
            .entities
            .notifications::<ModelProfileEntity>(profile)
            .await?
            .len(),
        0
    );
    // the owner bans the endpoint, a catalog model, the profile, and the credential
    let endpoint_ban = ModelBan::generic("endpoint paused");
    let model_ban = ModelBan::generic("model paused");
    let removed_model_ban = ModelBan::generic("other model paused");
    let profile_ban = ModelBan::generic("profile paused");
    let cred_ban = ModelBan::generic("credential paused");
    update(
        &owner.client,
        endpoint,
        endpoint_update()
            .model_bans_update(ModelBanUpdate::default().add_ban(endpoint_ban.clone()))
            .catalog_bans(
                "claude-a",
                ModelBanUpdate::default().add_ban(model_ban.clone()),
            )
            .catalog_bans(
                "claude-b",
                ModelBanUpdate::default().add_ban(removed_model_ban.clone()),
            ),
    )
    .await?;
    update(
        &owner.client,
        profile,
        EntityMetadataUpdate::model_profile()
            .model_bans_update(ModelBanUpdate::default().add_ban(profile_ban.clone())),
    )
    .await?;
    update(
        &admin,
        credential,
        EntityMetadataUpdate::model_credential()
            .model_bans_update(ModelBanUpdate::default().add_ban(cred_ban.clone())),
    )
    .await?;
    // members of either group read one notification per ban, keyed by the entity
    for member in [&member_a, &member_b] {
        let notes = member
            .client
            .entities
            .notifications::<ModelEndpointEntity>(endpoint)
            .await?;
        for ban in [&endpoint_ban, &model_ban, &removed_model_ban] {
            is!(
                notes
                    .iter()
                    .filter(|note| note.ban_id == Some(ban.id))
                    .count(),
                1
            );
        }
        assert!(notes.iter().all(|note| note.key == endpoint.to_string()));
        assert!(
            notes
                .iter()
                .any(|note| note.ban_id == Some(model_ban.id) && note.msg.contains("claude-a")),
            "the catalog ban's notification didn't name the model"
        );
        let notes = member
            .client
            .entities
            .notifications::<ModelProfileEntity>(profile)
            .await?;
        is!(notes.len(), 1);
        is!(notes[0].ban_id, Some(profile_ban.id));
        let notes = member
            .client
            .entities
            .notifications::<ModelCredentialEntity>(credential)
            .await?;
        is!(notes.len(), 1);
        is!(notes[0].ban_id, Some(cred_ban.id));
    }
    // each notification is stored once under its kind's type, not once per group
    is!(
        stored_notes(NotificationType::ModelEndpoints, endpoint).await?,
        3
    );
    is!(
        stored_notes(NotificationType::ModelProfiles, profile).await?,
        1
    );
    is!(
        stored_notes(NotificationType::ModelCredentials, credential).await?,
        1
    );
    // a user of another group gets a 404
    for id in [endpoint, profile, credential] {
        let resp = outsider
            .client
            .entities
            .notifications::<ModelEndpointEntity>(id)
            .await;
        fail!(resp, 404);
    }
    // removing the endpoint ban and the banned catalog entry deletes their notifications
    update(
        &owner.client,
        endpoint,
        endpoint_update()
            .model_bans_update(ModelBanUpdate::default().remove_ban(endpoint_ban.id))
            .remove_catalog_model("claude-b"),
    )
    .await?;
    let notes = member_a
        .client
        .entities
        .notifications::<ModelEndpointEntity>(endpoint)
        .await?;
    is!(notes.len(), 1);
    is!(notes[0].ban_id, Some(model_ban.id));
    // removing the profile's ban deletes its notification
    update(
        &owner.client,
        profile,
        EntityMetadataUpdate::model_profile()
            .model_bans_update(ModelBanUpdate::default().remove_ban(profile_ban.id)),
    )
    .await?;
    is!(
        stored_notes(NotificationType::ModelProfiles, profile).await?,
        0
    );
    // deleting a banned entity deletes its notifications
    owner.client.entities.delete(profile).await?;
    admin.entities.delete(endpoint).await?;
    is!(
        stored_notes(NotificationType::ModelEndpoints, endpoint).await?,
        0
    );
    Ok(())
}

/// Deleting an access key bans its assumed roles with a credential notification each, and
/// deleting a banned role removes its notification
#[tokio::test]
async fn broken_reference_notifications() -> Result<(), Error> {
    // an access key in a and b used by roles in a and in a and b
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let role = |source| CredentialKind::AwsAssumeRole {
        role_arn: "arn:aws:iam::1:role/r".into(),
        external_id: None,
        session_name: None,
        duration_seconds: None,
        source: AwsRoleSource::AccessKey(source),
    };
    let key = cred(
        &admin,
        &[&a, &b],
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
    )
    .await?;
    let narrow = cred(&admin, &[&a], role(key)).await?;
    let wide = cred(&admin, &[&a, &b], role(key)).await?;
    // deleting the key bans both roles, each with one notification naming the ban
    admin.entities.delete(key).await?;
    for id in [narrow, wide] {
        let read = admin.entities.get(id).await?;
        let ban = credential_meta(&read)
            .bans
            .values()
            .next()
            .cloned()
            .expect("the role wasn't banned");
        is!(ban.ban_kind, ModelBanKind::BrokenReference(key));
        let notes = admin
            .entities
            .notifications::<ModelCredentialEntity>(id)
            .await?;
        is!(notes.len(), 1);
        is!(notes[0].ban_id, Some(ban.id));
        is!(
            stored_notes(NotificationType::ModelCredentials, id).await?,
            1
        );
    }
    // deleting a banned role removes its notification
    admin.entities.delete(narrow).await?;
    is!(
        stored_notes(NotificationType::ModelCredentials, narrow).await?,
        0
    );
    is!(
        stored_notes(NotificationType::ModelCredentials, wide).await?,
        1
    );
    Ok(())
}

/// Send an entity update with a PNG image as a user, returning the status code
///
/// # Arguments
///
/// * `user` - The user sending the update
/// * `id` - The entity to update
/// * `name` - The image's file name
/// * `image` - The image's bytes
async fn send_image(user: &TestUser, id: Uuid, name: &str, image: &[u8]) -> Result<u16, Error> {
    // add the image to an otherwise empty update
    let admin = test_utilities::admin_client().await?;
    let form = EntityUpdate::default().to_form()?.part(
        "image",
        reqwest::multipart::Part::bytes(image.to_vec())
            .file_name(name.to_owned())
            .mime_str("image/png")?,
    );
    // send it with the user's token
    let resp = reqwest::Client::new()
        .patch(format!("{}/api/entities/{id}", admin.host))
        .header("authorization", user.auth_header())
        .multipart(form)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Download an entity's image as a user
///
/// # Arguments
///
/// * `user` - The user downloading the image
/// * `id` - The entity whose image to download
async fn download_image(user: &TestUser, id: Uuid) -> Result<(u16, Vec<u8>), Error> {
    // get the image with the user's token
    let admin = test_utilities::admin_client().await?;
    let resp = reqwest::Client::new()
        .get(format!("{}/api/entities/{id}/image", admin.host))
        .header("authorization", user.auth_header())
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().await?.to_vec()))
}

/// An image upload by a member who can't edit an endpoint is refused before anything is
/// written, so the endpoint's image is unchanged
#[tokio::test]
async fn refused_image_upload_keeps_image() -> Result<(), Error> {
    // an owner's endpoint with an image and a member of its group
    let admin = test_utilities::admin_client().await?;
    let owner = plain(&admin).await?;
    let member = plain(&admin).await?;
    let g = group(
        GroupRoles::default()
            .owner(&owner.username)
            .user(&member.username),
        &admin,
    )
    .await?;
    let endpoint =
        generators::model_endpoint(&[&g], anthropic(None, "claude-a"), &owner.client).await?;
    // the owner gives the endpoint an image
    is!(
        send_image(&owner, endpoint.id, "logo.png", b"original").await?,
        204
    );
    // the member's upload of an image with the same name is refused
    is!(
        send_image(&member, endpoint.id, "logo.png", b"replaced").await?,
        401
    );
    // the original image is still there, unchanged
    let (status, bytes) = download_image(&owner, endpoint.id).await?;
    is!(status, 200);
    is!(bytes, b"original".to_vec());
    // the member's upload of an image with a new name is refused too
    is!(
        send_image(&member, endpoint.id, "other.png", b"other").await?,
        401
    );
    // and the original image is still there and still set on the endpoint
    let (status, bytes) = download_image(&owner, endpoint.id).await?;
    is!(status, 200);
    is!(bytes, b"original".to_vec());
    is!(
        owner
            .client
            .entities
            .get(endpoint.id)
            .await?
            .image
            .is_some(),
        true
    );
    Ok(())
}

/// The tool capabilities are set on a profile create, a profile update, and a catalog edit, and
/// each is returned on get
#[tokio::test]
async fn tool_capabilities() -> Result<(), Error> {
    // an endpoint in minerva whose catalog entry marks strict tools
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let g = generators::named_group_with_roles(
        &generators::readable_name("minerva", &suffix),
        &GroupRoles::default(),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let mut entry = ModelCatalogEntryRequest::new("claude-t");
    entry.capabilities = ModelCapabilities {
        tool_calling: true,
        strict_tools: true,
        ..ModelCapabilities::default()
    };
    let req = ModelEndpointEntityRequest::new(generators::gen_anthropic_provider()).model(entry);
    let endpoint = generators::model_endpoint(&[&g], req, &admin).await?;
    let caps = |entity: &Entity, model_id: &str| {
        endpoint_meta(entity)
            .catalog
            .iter()
            .find(|entry| entry.model_id == model_id)
            .map(|entry| entry.capabilities.clone())
            .expect("the catalog entry is missing")
    };
    is!(
        caps(&admin.entities.get(endpoint.id).await?, "claude-t").strict_tools,
        true
    );
    // a catalog edit sets parallel tool calls on another entry
    let mut edited = ModelCatalogEntryRequest::new("claude-p");
    edited.capabilities = ModelCapabilities {
        parallel_tool_calls: true,
        ..ModelCapabilities::default()
    };
    update(
        &admin,
        endpoint.id,
        endpoint_update().add_catalog_model(edited),
    )
    .await?;
    let read = admin.entities.get(endpoint.id).await?;
    is!(caps(&read, "claude-p").parallel_tool_calls, true);
    is!(caps(&read, "claude-p").strict_tools, false);
    // a profile create sets forced tool choice
    let forced = ModelCapabilities {
        forced_tool_choice: true,
        ..ModelCapabilities::default()
    };
    let mut req =
        ModelProfileEntityRequest::new(endpoint.id, "claude-t", ModelApi::AnthropicMessages);
    req.capabilities = Some(forced.clone());
    let profile = generators::model_profile(&[&g], req, &admin).await?;
    let read = admin.entities.get(profile.id).await?;
    is!(
        profile_meta(&read).capabilities.clone(),
        Some(forced.clone())
    );
    // a profile update sets strict tools and parallel tool calls instead
    let both = ModelCapabilities {
        strict_tools: true,
        parallel_tool_calls: true,
        ..ModelCapabilities::default()
    };
    update(
        &admin,
        profile.id,
        EntityMetadataUpdate::model_profile().profile_capabilities(both.clone()),
    )
    .await?;
    let read = admin.entities.get(profile.id).await?;
    is!(profile_meta(&read).capabilities.clone(), Some(both.clone()));
    Ok(())
}

/// A ban added to a profile while an edit of its parameters is in flight survives the edit, and
/// the edit survives the ban, since writes to a profile wait for each other
#[tokio::test]
async fn profile_ban_survives_concurrent_edit() -> Result<(), Error> {
    // a profile in one group
    let admin = test_utilities::admin_client().await?;
    let g = generators::groups(1, &admin).await?.remove(0).name;
    let endpoint = generators::model_endpoint(&[&g], anthropic(None, "claude-a"), &admin).await?;
    let profile = generators::model_profile(
        &[&g],
        ModelProfileEntityRequest::new(endpoint.id, "claude-a", ModelApi::AnthropicMessages),
        &admin,
    )
    .await?;
    // a parameter edit holds right before it writes the profile it read
    test_utilities::faults::pause_entity_update(profile.id);
    let params = ModelParameters {
        temperature: Some(0.2),
        ..ModelParameters::default()
    };
    let editor = test_utilities::admin_client().await?;
    let edit_meta = EntityMetadataUpdate::model_profile().profile_parameters(params);
    let edit = tokio::spawn(async move { update(&editor, profile.id, edit_meta).await });
    let mut waited = 0;
    while !test_utilities::faults::entity_update_paused(profile.id) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        waited += 1;
        assert!(waited < 400, "the parameter edit never reached its write");
    }
    // a ban is added while the edit holds
    let ban = ModelBan::generic("profile paused mid edit");
    let banner = test_utilities::admin_client().await?;
    let ban_meta = EntityMetadataUpdate::model_profile()
        .model_bans_update(ModelBanUpdate::default().add_ban(ban.clone()));
    let banning = tokio::spawn(async move { update(&banner, profile.id, ban_meta).await });
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // the edit writes, then the ban
    test_utilities::faults::release_entity_update(profile.id);
    edit.await
        .map_err(|err| Error::new(format!("the edit panicked: {err}")))??;
    banning
        .await
        .map_err(|err| Error::new(format!("the ban panicked: {err}")))??;
    // the profile keeps both the new parameters and the ban
    let read = admin.entities.get(profile.id).await?;
    is!(profile_meta(&read).parameters.temperature, Some(0.2));
    is!(profile_meta(&read).bans.contains_key(&ban.id), true);
    Ok(())
}

/// A failed lock release after a committed refresh request answers as done and queues the
/// refresh once
#[tokio::test]
async fn failed_unlock_keeps_refresh_request() -> Result<(), Error> {
    // europa owns minerva's endpoint, whose creation queued one refresh, finished by a report
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let minerva = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().owner(&europa.username),
    )
    .await?;
    let endpoint = generators::model_endpoint(
        &[minerva.as_str()],
        ModelEndpointEntityRequest::new(allowed_anthropic("unlock").await?),
        &europa.client,
    )
    .await?;
    send_report(&admin, endpoint.id, report(&[("claude-a", None)])).await?;
    is!(refreshes(&admin, endpoint.id).await?.len(), 1);
    // the lock release after the pending mark fails once, and the request is still answered
    test_utilities::faults::fail_after_unlock(endpoint.id);
    europa
        .client
        .entities
        .refresh_model_endpoint(endpoint.id)
        .await?;
    // the endpoint is pending and its refresh was queued
    let read = admin.entities.get(endpoint.id).await?;
    is!(
        endpoint_meta(&read).refresh.state,
        ModelRefreshState::Pending
    );
    is!(refreshes(&admin, endpoint.id).await?.len(), 2);
    // asking again while it is pending only answers
    europa
        .client
        .entities
        .refresh_model_endpoint(endpoint.id)
        .await?;
    is!(refreshes(&admin, endpoint.id).await?.len(), 2);
    Ok(())
}
