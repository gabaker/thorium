//! Backend logic for model endpoints, model profiles, and model credentials
//!
//! These kinds keep one definition across their groups. Only owners and managers of every group
//! create, change, rotate, or delete endpoints and credentials, while profiles follow the usual
//! shared entity rules; bans are kept once per entity and owners or managers of any of its groups
//! may change them. An endpoint's credential and a profile's endpoint are mirrored by
//! `AuthenticatesWith` and `Serves` associations that exist only so a referenced entity can't be
//! deleted or lose a group its referrers need.

use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use tracing::instrument;
use uuid::Uuid;

use super::tools::{self, check_name_and_groups, visible_groups};
use crate::models::backends::{NotificationSupport, db};
use crate::models::entities::models::{build_catalog, parse_json_field};
use crate::models::{
    AssociationKind, AssociationListOpts, AssociationTarget, AssociationTargetColumn,
    CatalogEntrySource, CredentialKind, CredentialSecrets, Directionality, Entity, EntityForm,
    EntityKinds, EntityListParams, EntityMetadata, EntityMetadataUpdateForm, EntityResponse,
    EntityUpdateForm, Event, Group, GroupAllowAction, LimitsSource, MAX_LISTED_MODELS, ModelBan,
    ModelBanKind, ModelBanUpdate, ModelCapabilities, ModelCatalogEntry, ModelCatalogEntryRequest,
    ModelConfigRef, ModelCredentialEntity, ModelCredentialEntityRequest, ModelEndpointEntity,
    ModelEndpointEntityRequest, ModelLimits, ModelParameters, ModelProfileEntity,
    ModelProfileEntityRequest, ModelProvider, ModelRefreshReason, ModelRefreshReport,
    ModelRefreshState, ModelRefreshStatus, ModelRefreshTarget, ModelSecret, ModelsConf,
    ScopedModelBan, User,
};
use crate::utils::{ApiError, Shared};
use crate::{bad, conflict, internal_err, not_found, unauthorized};

/// How long a pending refresh counts as already queued, so asking again queues nothing
///
/// A pending refresh older than this is queued again, in case its event was lost.
const PENDING_REFRESH_WINDOW: chrono::Duration = chrono::Duration::minutes(10);

/// How long a refresh report or an access key's delete waits for the lock of a credential it
/// bans or clears
///
/// Each holds another lock while it waits (the report its endpoint's, the delete the access
/// key's), so the wait stays far below that lock's expiry; a credential held longer refuses the
/// request with a conflict, which the refresh or the deleting user retries.
const CREDENTIAL_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A reference association to move once the referrer's row is written
#[derive(Debug)]
pub(crate) struct ReferenceMove {
    /// The kind of reference association
    kind: AssociationKind,
    /// The id of the referrer
    source: Uuid,
    /// The name of the referrer
    source_name: String,
    /// The target the referrer no longer references, if it changed
    old_target: Option<Uuid>,
    /// The target the referrer now references, with its name, if it changed
    new_target: Option<(Uuid, String)>,
    /// The groups the new reference is written in
    groups: Vec<String>,
}

/// The bans an entity write added and removed, for the notifications to write afterwards
#[derive(Debug)]
pub(crate) struct ModelBanChanges {
    /// The id of the entity the bans are on
    id: Uuid,
    /// The entity's metadata after the write
    meta: EntityMetadata,
    /// The bans that were added, with the item each one is on
    added: Vec<ScopedModelBan>,
    /// The ids of the bans that were removed
    removed: Vec<Uuid>,
}

impl ModelBanChanges {
    /// Compare an entity's bans before and after a change
    ///
    /// # Arguments
    ///
    /// * `before` - The entity's bans before the change
    /// * `entity` - The entity after the change
    pub(crate) fn between(before: &HashMap<Uuid, ScopedModelBan>, entity: &Entity) -> Self {
        // find the bans that appeared and the ones that disappeared
        let after = ban_index(&entity.metadata);
        let added = after
            .iter()
            .filter(|(id, _)| !before.contains_key(id))
            .map(|(_, ban)| ban.clone())
            .collect();
        let removed = before
            .keys()
            .filter(|id| !after.contains_key(id))
            .copied()
            .collect();
        ModelBanChanges {
            id: entity.id,
            meta: entity.metadata.clone(),
            added,
            removed,
        }
    }

    /// Write a notification for each added ban and delete the one of each removed ban
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn notify(&self, shared: &Shared) -> Result<(), ApiError> {
        // nothing changed, so nothing to write
        if self.added.is_empty() && self.removed.is_empty() {
            return Ok(());
        }
        // notifications are kept once per entity under its id
        let key = self.id.to_string();
        match &self.meta {
            EntityMetadata::ModelEndpoint(meta) => {
                meta.update_ban_notifications(&key, &self.added, &self.removed, shared)
                    .await
            }
            EntityMetadata::ModelProfile(meta) => {
                meta.update_ban_notifications(&key, &self.added, &self.removed, shared)
                    .await
            }
            EntityMetadata::ModelCredential(meta) => {
                meta.update_ban_notifications(&key, &self.added, &self.removed, shared)
                    .await
            }
            _ => Ok(()),
        }
    }
}

/// What applying an update to a model entity needs done once the entity is written
#[derive(Debug)]
pub(crate) struct ModelUpdate {
    /// The reference association to move, if one moved
    pub(crate) moved: Option<ReferenceMove>,
    /// The bans the update added and removed
    pub(crate) bans: ModelBanChanges,
    /// The group credential a merged refresh report bans or unbans, and how
    pub(crate) credential_effect: Option<(Uuid, CredentialEffect)>,
    /// Why the updated endpoint needs a refresh, if it does
    pub(crate) refresh: Option<ModelRefreshReason>,
    /// Whether the updated credential's secret was rotated
    pub(crate) rotated: bool,
}

/// What applying an update to a model endpoint changed beyond its own row
#[derive(Debug, Default)]
struct EndpointChanges {
    /// The credential association to move, if the credential changed
    moved: Option<ReferenceMove>,
    /// The group credential a merged refresh report bans or unbans, and how
    credential_effect: Option<(Uuid, CredentialEffect)>,
    /// Whether the provider settings or the credential reference changed
    connection_changed: bool,
}

/// Get every ban of a model entity by id, with the item each one is on
///
/// # Arguments
///
/// * `meta` - The entity's metadata
pub(crate) fn ban_index(meta: &EntityMetadata) -> HashMap<Uuid, ScopedModelBan> {
    // collect the bans by id
    let mut index = HashMap::new();
    // each kind keeps its bans on different items
    match meta {
        EntityMetadata::ModelEndpoint(meta) => {
            // the endpoint's own bans
            for ban in meta.bans.values() {
                index.insert(ban.id, ScopedModelBan::top(ban.clone()));
            }
            // each catalog model's bans, naming the model
            for entry in &meta.catalog {
                let scope = format!("model `{}`", entry.model_id);
                for ban in entry.bans.values() {
                    index.insert(ban.id, ScopedModelBan::scoped(scope.clone(), ban.clone()));
                }
            }
        }
        EntityMetadata::ModelProfile(meta) => {
            // the profile's own bans
            for ban in meta.bans.values() {
                index.insert(ban.id, ScopedModelBan::top(ban.clone()));
            }
        }
        EntityMetadata::ModelCredential(meta) => {
            // the credential's own bans
            for ban in meta.bans.values() {
                index.insert(ban.id, ScopedModelBan::top(ban.clone()));
            }
        }
        _ => (),
    }
    index
}

/// Delete every notification of a model entity that is being deleted
///
/// # Arguments
///
/// * `entity` - The entity being deleted
/// * `shared` - Shared Thorium objects
pub(crate) async fn delete_notifications(entity: &Entity, shared: &Shared) -> Result<(), ApiError> {
    // notifications are kept under the entity's id
    let key = entity.id.to_string();
    match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => meta.delete_all_notifications(&key, shared).await,
        EntityMetadata::ModelProfile(meta) => meta.delete_all_notifications(&key, shared).await,
        EntityMetadata::ModelCredential(meta) => meta.delete_all_notifications(&key, shared).await,
        _ => Ok(()),
    }
}

/// Make sure an endpoint's provider is on the deployment's allowed endpoint list
///
/// # Arguments
///
/// * `name` - The name of the endpoint, for the refusal
/// * `provider` - The endpoint's provider
/// * `shared` - Shared Thorium objects
async fn check_whitelist(
    name: &str,
    provider: &ModelProvider,
    shared: &Shared,
) -> Result<(), ApiError> {
    // read the allowed endpoints and check the provider against them
    let settings = db::system::get_settings(shared).await?;
    bad_request(settings.models.check(name, provider))
}

/// Get the provider of a group endpoint the deployment still allows refreshing, with the
/// endpoint in all of its groups
///
/// Returns nothing for an endpoint that's gone, isn't an endpoint, is banned for leaving the
/// allowed endpoint list, or is no longer on it, so no refresh is queued for it.
///
/// # Arguments
///
/// * `id` - The endpoint's id
/// * `conf` - The allowed endpoint list
/// * `shared` - Shared Thorium objects
pub(crate) async fn refreshable_endpoint(
    id: Uuid,
    conf: &ModelsConf,
    shared: &Shared,
) -> Result<Option<Entity>, ApiError> {
    // read the endpoint in all of its groups
    let Some(entity) = read_reference(id, shared).await? else {
        return Ok(None);
    };
    // only an endpoint the deployment allows is refreshed
    let allowed = match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => refresh_allowed(conf, &meta.provider, &meta.bans),
        _ => false,
    };
    Ok(allowed.then_some(entity))
}

/// Whether the deployment still allows refreshing an endpoint
///
/// # Arguments
///
/// * `conf` - The allowed endpoint list
/// * `provider` - The endpoint's provider
/// * `bans` - The endpoint's bans
pub(crate) fn refresh_allowed(
    conf: &ModelsConf,
    provider: &ModelProvider,
    bans: &HashMap<Uuid, ModelBan>,
) -> bool {
    // an endpoint banned for leaving the list, or no longer on it, is never refreshed
    !bans
        .values()
        .any(|ban| ban.ban_kind == ModelBanKind::NotAllowedByDeployment)
        && conf.allows(provider)
}

/// Whether a kind is one of the model configuration kinds
///
/// # Arguments
///
/// * `kind` - The kind to check
pub(crate) fn is_model_kind(kind: EntityKinds) -> bool {
    // the three model configuration kinds
    matches!(
        kind,
        EntityKinds::ModelEndpoint | EntityKinds::ModelProfile | EntityKinds::ModelCredential
    )
}

/// Whether only owners and managers may write a kind
///
/// # Arguments
///
/// * `kind` - The kind to check
pub(crate) fn needs_manager(kind: EntityKinds) -> bool {
    // endpoints and credentials control where requests go and what pays for them
    matches!(
        kind,
        EntityKinds::ModelEndpoint | EntityKinds::ModelCredential
    )
}

/// Describe a kind the way users know it in errors
///
/// # Arguments
///
/// * `kind` - The kind to describe
fn kind_name(kind: EntityKinds) -> &'static str {
    // name each model kind in plain words
    match kind {
        EntityKinds::ModelEndpoint => "model endpoint",
        EntityKinds::ModelProfile => "model profile",
        EntityKinds::ModelCredential => "model credential",
        _ => "entity",
    }
}

/// Map a model validation message to a `400`
///
/// # Arguments
///
/// * `result` - The result of a model validation
fn bad_request<T>(result: Result<T, String>) -> Result<T, ApiError> {
    // turn any validation message into a bad request
    match result {
        Ok(value) => Ok(value),
        Err(msg) => bad!(msg),
    }
}

/// Check that a user may own and manage every group of a model endpoint or credential
///
/// # Arguments
///
/// * `user` - The user to check
/// * `groups` - Every group of the entity
/// * `action` - The action named in a refusal
/// * `name` - The name of the entity named in a refusal
/// * `shared` - Shared Thorium objects
async fn authorize_managers(
    user: &User,
    groups: &[String],
    action: &str,
    name: &str,
    shared: &Shared,
) -> Result<(), ApiError> {
    // owners, managers, and admins pass in every group that allows entities
    Group::authorize_graph(
        user,
        groups,
        Group::modifiable,
        Some(GroupAllowAction::Entities),
        action,
        name,
        shared,
    )
    .await?;
    Ok(())
}

/// Check that a user owns or manages at least one group of an entity
///
/// # Arguments
///
/// * `user` - The user to check
/// * `entity` - The entity, in all of its groups
/// * `action` - What the user is doing, named in a refusal
/// * `shared` - Shared Thorium objects
async fn authorize_any_manager(
    user: &User,
    entity: &Entity,
    action: &str,
    shared: &Shared,
) -> Result<(), ApiError> {
    // admins may change any ban
    if user.is_admin() {
        return Ok(());
    }
    // only the groups this user is in can make them a manager
    let mine = entity
        .groups
        .iter()
        .filter(|group| user.groups.contains(*group))
        .collect::<Vec<&String>>();
    let details = db::groups::list_details(mine.into_iter(), shared).await?;
    // an owner or manager of any of them may change bans
    if details.iter().any(|group| group.modifiable(user).is_ok()) {
        return Ok(());
    }
    unauthorized!(format!(
        "you don't have permission to {action} `{}`",
        entity.name
    ))
}

/// Find a referenced model entity, checking its kind and that it covers the referrer's groups
///
/// A missing or hidden target, or one of another kind, is a `400`; a target missing groups the
/// user can see is a `400` naming them, and one missing only hidden groups is a plain `401`.
///
/// # Arguments
///
/// * `user` - The user writing the reference
/// * `target` - The id of the referenced entity
/// * `kind` - The kind the referenced entity must be
/// * `field` - The name of the referencing field, for errors
/// * `referrer_groups` - Every group of the referrer
/// * `shared` - Shared Thorium objects
async fn resolve_reference(
    user: &User,
    target: Uuid,
    kind: EntityKinds,
    field: &str,
    referrer_groups: &[String],
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // the target must exist and be visible to this user
    let mut entity = match Entity::get(user, target, shared).await {
        Ok(entity) => entity,
        Err(err) if err.status() == axum::http::StatusCode::NOT_FOUND => {
            return bad!(format!(
                "{field} {target} doesn't name a {} you can see",
                kind_name(kind)
            ));
        }
        Err(err) => return Err(err),
    };
    // the target must be of the right kind
    if entity.kind != kind {
        return bad!(format!(
            "{field} must name a {}, but {target} is a {}",
            kind_name(kind),
            entity.kind
        ));
    }
    // the target must be in every group of the referrer
    entity.groups = db::entities::all_groups(target, shared).await?;
    let missing = crate::utils::helpers::missing_groups(referrer_groups, &entity.groups);
    if !missing.is_empty() {
        // name only the missing groups this user can see
        let visible = visible_groups(user, &missing)?;
        return bad!(format!(
            "{} `{}` isn't in groups {visible:?}; it must be in every group of the entity that references it",
            kind_name(kind),
            entity.name
        ));
    }
    Ok(entity)
}

/// Get the endpoint metadata of an entity
///
/// # Arguments
///
/// * `entity` - The entity to read
fn endpoint_meta(entity: &Entity) -> Result<&ModelEndpointEntity, ApiError> {
    // only endpoints have endpoint metadata
    match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => Ok(meta),
        _ => internal_err!(format!("{} is not a model endpoint", entity.id)),
    }
}

/// Get the credential metadata of an entity
///
/// # Arguments
///
/// * `entity` - The entity to read
fn credential_meta(entity: &Entity) -> Result<&ModelCredentialEntity, ApiError> {
    // only credentials have credential metadata
    match &entity.metadata {
        EntityMetadata::ModelCredential(meta) => Ok(meta),
        _ => internal_err!(format!("{} is not a model credential", entity.id)),
    }
}

/// Check that a model id list holds no empty ids
///
/// # Arguments
///
/// * `field` - The name of the field, for errors
/// * `ids` - The model ids to check
fn check_model_ids<'a, I: IntoIterator<Item = &'a String>>(
    field: &str,
    ids: I,
) -> Result<(), ApiError> {
    // a model id can't be empty
    if ids.into_iter().any(|id| id.trim().is_empty()) {
        return bad!(format!("{field} can't hold an empty model id"));
    }
    Ok(())
}

/// Build the secrets of a credential from its submitted values
///
/// Empty secrets count as missing, so a kind that needs a secret is never stored without one.
///
/// # Arguments
///
/// * `kind` - The kind of credential
/// * `secret` - The submitted primary secret
/// * `session_token` - The submitted session token
fn build_secrets(
    kind: &CredentialKind,
    secret: Option<ModelSecret>,
    session_token: Option<ModelSecret>,
) -> Result<CredentialSecrets, ApiError> {
    // empty secrets are treated as missing
    let secret = secret.filter(|secret| !secret.is_empty());
    let session_token = session_token.filter(|token| !token.is_empty());
    // check the primary secret against the kind
    match (kind.needs_secret(), &secret) {
        (true, None) => {
            return bad!(format!("a {} credential needs a secret", kind.cred_type()));
        }
        (false, Some(_)) => {
            return bad!(
                "an AwsAssumeRole credential holds no secret; its source credential does"
                    .to_owned()
            );
        }
        _ => (),
    }
    // only access keys have session tokens
    if session_token.is_some() && !matches!(kind, CredentialKind::AwsAccessKey { .. }) {
        return bad!("only an AwsAccessKey credential takes a session token".to_owned());
    }
    // fingerprint the primary secret
    let fingerprint = secret.as_ref().map(ModelSecret::fingerprint);
    Ok(CredentialSecrets {
        primary: secret,
        session_token,
        fingerprint,
    })
}

/// Apply a ban update to a ban list
///
/// Users other than admins may only add `Generic` bans; the other kinds are registered by
/// Thorium. Removing a ban that doesn't exist is a `404` and adding one whose id exists is a
/// `400`.
///
/// # Arguments
///
/// * `user` - The user changing the bans
/// * `bans` - The ban list to change
/// * `update` - The ban update to apply
fn apply_bans(
    user: &User,
    bans: &mut HashMap<Uuid, ModelBan>,
    update: ModelBanUpdate,
) -> Result<(), ApiError> {
    // check every ban before changing anything
    for ban in &update.bans_added {
        // only admins may add bans Thorium normally registers
        if !user.is_admin() && !matches!(ban.ban_kind, ModelBanKind::Generic(_)) {
            return bad!(format!(
                "only Generic bans can be added by hand, not {}",
                ban.ban_kind.name()
            ));
        }
        // bans can't be replaced, only added or removed
        if bans.contains_key(&ban.id) {
            return bad!(format!(
                "A ban with id '{}' already exists. Bans cannot be updated, only added or removed.",
                ban.id
            ));
        }
    }
    // every ban to remove must exist
    for id in &update.bans_removed {
        if !bans.contains_key(id) {
            return not_found!(format!("The ban with id '{id}' doesn't exist"));
        }
    }
    // add and remove the bans
    for ban in update.bans_added {
        bans.insert(ban.id, ban);
    }
    for id in &update.bans_removed {
        bans.remove(id);
    }
    Ok(())
}

/// Set or clear the `BrokenReference` ban for a reference
///
/// Every `BrokenReference` ban for another target is cleared, since the reference no longer
/// points there.
///
/// # Arguments
///
/// * `bans` - The referrer's bans
/// * `target` - The current target of the reference, if there is one
/// * `broken` - Whether the current target no longer resolves
pub(crate) fn sync_broken_reference(
    bans: &mut HashMap<Uuid, ModelBan>,
    target: Option<Uuid>,
    broken: bool,
) {
    // drop the bans for targets that aren't referenced or that resolve again
    bans.retain(|_, ban| match ban.ban_kind {
        ModelBanKind::BrokenReference(id) => broken && Some(id) == target,
        _ => true,
    });
    // ban a broken reference once
    if let (true, Some(target)) = (broken, target) {
        let banned = bans
            .values()
            .any(|ban| ban.ban_kind == ModelBanKind::BrokenReference(target));
        if !banned {
            let ban = ModelBan::new(ModelBanKind::BrokenReference(target));
            bans.insert(ban.id, ban);
        }
    }
}

/// The parts of an endpoint a refresh report merges into
pub(crate) struct RefreshedEndpoint<'a> {
    /// The endpoint's catalog
    pub(crate) catalog: &'a mut Vec<ModelCatalogEntry>,
    /// The endpoint's own bans
    pub(crate) bans: &'a mut HashMap<Uuid, ModelBan>,
    /// The state of the endpoint's most recent refresh
    pub(crate) refresh: &'a mut ModelRefreshStatus,
}

/// What a refresh report does to the credential the refresh used
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CredentialEffect {
    /// Ban the credential with this kind of ban, once per kind
    Ban(ModelBanKind),
    /// Clear the credential's rejected and expired bans
    Clear,
    /// Leave the credential alone
    Nothing,
}

/// Check that a refresh report holds only what a refresh can report
///
/// # Arguments
///
/// * `report` - The report to check
pub(crate) fn check_report(report: &ModelRefreshReport) -> Result<(), ApiError> {
    // a refresh only reports credential and endpoint failures
    if let Some(failure) = &report.failure
        && !matches!(
            failure,
            ModelBanKind::CredentialRejected { .. }
                | ModelBanKind::CredentialExpired
                | ModelBanKind::EndpointUnreachable(_)
        )
    {
        return bad!(format!(
            "a refresh report can't fail with {}; only CredentialRejected, CredentialExpired, and EndpointUnreachable are reported",
            failure.name()
        ));
    }
    // a report can't list more models than any provider serves
    if report.models.len() > MAX_LISTED_MODELS {
        return bad!(format!(
            "a refresh report can list at most {MAX_LISTED_MODELS} models, not {}",
            report.models.len()
        ));
    }
    // listed models need ids
    check_model_ids(
        "refresh_report.models",
        report.models.iter().map(|model| &model.model_id),
    )
}

/// Whether a ban list holds a ban of the same kind as another
///
/// # Arguments
///
/// * `bans` - The ban list
/// * `kind` - The kind of ban
fn holds_kind(bans: &HashMap<Uuid, ModelBan>, kind: &ModelBanKind) -> bool {
    // kinds are compared by name, so one ban per kind is kept whatever its detail
    bans.values().any(|ban| ban.ban_kind.name() == kind.name())
}

/// Merge a refresh report into an endpoint's catalog, bans, and refresh state
///
/// A listing is additive: new models are added as refreshed entries, refreshed limits follow
/// the listing while manual limits are kept, a refreshed model that's no longer listed gets one
/// `ModelRemoved` ban that clears when it's listed again, and the endpoint's
/// `EndpointUnreachable` bans are cleared. A failure leaves the catalog alone and bans the
/// endpoint once if it couldn't be reached. Disabled models are never touched.
///
/// # Arguments
///
/// * `endpoint` - The parts of the endpoint to merge into
/// * `report` - The report to merge
pub(crate) fn merge_refresh_report(
    endpoint: &mut RefreshedEndpoint<'_>,
    report: &ModelRefreshReport,
) {
    // a failure only bans and records the failure
    if let Some(failure) = &report.failure {
        if matches!(failure, ModelBanKind::EndpointUnreachable(_))
            && !holds_kind(endpoint.bans, failure)
        {
            let ban = ModelBan::new(failure.clone());
            endpoint.bans.insert(ban.id, ban);
        }
        *endpoint.refresh = ModelRefreshStatus {
            state: ModelRefreshState::Failed,
            requested: endpoint.refresh.requested,
            finished: Some(report.finished),
            error: Some(failure.to_string()),
        };
        return;
    }
    // add or update each listed model once, the first listing of an id winning
    let mut listed = HashSet::new();
    for model in &report.models {
        if !listed.insert(model.model_id.clone()) {
            continue;
        }
        let refreshed = LimitsSource::Refreshed {
            at: report.finished,
        };
        match endpoint
            .catalog
            .binary_search_by(|entry| entry.model_id.cmp(&model.model_id))
        {
            Ok(index) => {
                let entry = &mut endpoint.catalog[index];
                // refreshed limits follow the listing and manual ones are kept
                if matches!(entry.limits.source, LimitsSource::Refreshed { .. }) {
                    entry.limits = ModelLimits {
                        context_window: model.context_window,
                        max_output_tokens: model.max_output_tokens,
                        source: refreshed,
                    };
                }
                // a refreshed entry takes the capabilities the listing reports
                if let (CatalogEntrySource::Refreshed { .. }, Some(capabilities)) =
                    (&entry.source, &model.capabilities)
                {
                    entry.capabilities = capabilities.clone();
                }
                // a model listed again loses its removal ban
                entry
                    .bans
                    .retain(|_, ban| ban.ban_kind != ModelBanKind::ModelRemoved);
            }
            Err(index) => {
                // a new model is added as a refreshed entry
                endpoint.catalog.insert(
                    index,
                    ModelCatalogEntry {
                        model_id: model.model_id.clone(),
                        limits: ModelLimits {
                            context_window: model.context_window,
                            max_output_tokens: model.max_output_tokens,
                            source: refreshed,
                        },
                        capabilities: model.capabilities.clone().unwrap_or_default(),
                        bans: HashMap::new(),
                        source: CatalogEntrySource::Refreshed {
                            first_seen: report.finished,
                        },
                    },
                );
            }
        }
    }
    // a refreshed model that isn't listed is banned once and never deleted
    for entry in endpoint.catalog.iter_mut() {
        let unlisted = !listed.contains(&entry.model_id)
            && matches!(entry.source, CatalogEntrySource::Refreshed { .. });
        if unlisted && !holds_kind(&entry.bans, &ModelBanKind::ModelRemoved) {
            let ban = ModelBan::new(ModelBanKind::ModelRemoved);
            entry.bans.insert(ban.id, ban);
        }
    }
    // a successful listing clears the endpoint's unreachable bans
    endpoint
        .bans
        .retain(|_, ban| !matches!(ban.ban_kind, ModelBanKind::EndpointUnreachable(_)));
    *endpoint.refresh = ModelRefreshStatus {
        state: ModelRefreshState::Succeeded,
        requested: endpoint.refresh.requested,
        finished: Some(report.finished),
        error: None,
    };
}

/// Get what a refresh report does to the credential the refresh used
///
/// # Arguments
///
/// * `report` - The report
pub(crate) fn credential_effect(report: &ModelRefreshReport) -> CredentialEffect {
    // credential failures ban it, a success clears it, and an unreachable endpoint does nothing
    match &report.failure {
        Some(
            failure @ (ModelBanKind::CredentialRejected { .. } | ModelBanKind::CredentialExpired),
        ) => CredentialEffect::Ban(failure.clone()),
        Some(_) => CredentialEffect::Nothing,
        None => CredentialEffect::Clear,
    }
}

/// Apply what a refresh report does to a credential's bans
///
/// Returns whether the bans changed.
///
/// # Arguments
///
/// * `bans` - The credential's bans
/// * `effect` - What the report does to the credential
pub(crate) fn apply_credential_effect(
    bans: &mut HashMap<Uuid, ModelBan>,
    effect: &CredentialEffect,
) -> bool {
    // ban once per kind, or clear the refresh failure kinds
    match effect {
        CredentialEffect::Ban(kind) if !holds_kind(bans, kind) => {
            let ban = ModelBan::new(kind.clone());
            bans.insert(ban.id, ban);
            true
        }
        CredentialEffect::Clear => {
            let before = bans.len();
            bans.retain(|_, ban| {
                !matches!(
                    ban.ban_kind,
                    ModelBanKind::CredentialRejected { .. } | ModelBanKind::CredentialExpired
                )
            });
            bans.len() != before
        }
        _ => false,
    }
}

/// Read a referenced entity in every one of its groups, if it still exists
///
/// # Arguments
///
/// * `target` - The referenced entity
/// * `shared` - Shared Thorium objects
async fn read_reference(target: Uuid, shared: &Shared) -> Result<Option<Entity>, ApiError> {
    // a missing entity resolves to nothing
    let groups = match db::entities::all_groups(target, shared).await {
        Ok(groups) => groups,
        Err(err) if err.status() == axum::http::StatusCode::NOT_FOUND => return Ok(None),
        Err(err) => return Err(err),
    };
    // read it in all of its groups
    let mut entity = db::entities::get(&groups, target, shared).await?;
    entity.groups = groups;
    Ok(Some(entity))
}

/// Build a model endpoint from a create request
///
/// # Arguments
///
/// * `user` - The user creating the endpoint
/// * `name` - The name of the new endpoint
/// * `req` - The endpoint request
/// * `groups` - The groups of the new endpoint
/// * `shared` - Shared Thorium objects
async fn build_endpoint(
    user: &User,
    name: &str,
    req: ModelEndpointEntityRequest,
    groups: &[String],
    shared: &Shared,
) -> Result<(ModelEndpointEntity, Option<Entity>), ApiError> {
    // the provider's own settings must be valid
    bad_request(req.provider.validate())?;
    // the deployment must allow the provider and its url
    check_whitelist(name, &req.provider, shared).await?;
    // the catalog can't repeat a model id
    let catalog = bad_request(build_catalog(req.models))?;
    // disabled ids can't be empty
    check_model_ids("model_endpoint_disabled_models", &req.disabled_models)?;
    // the credential must be a credential this provider accepts that covers our groups
    let credential = match req.credential {
        Some(id) => {
            let entity = resolve_reference(
                user,
                id,
                EntityKinds::ModelCredential,
                "model_endpoint_credential",
                groups,
                shared,
            )
            .await?;
            bad_request(
                req.provider
                    .check_credential(&credential_meta(&entity)?.kind),
            )?;
            Some(entity)
        }
        None => None,
    };
    // build the endpoint
    let meta = ModelEndpointEntity {
        provider: req.provider,
        credential: req.credential,
        catalog,
        bans: HashMap::default(),
        refresh: crate::models::ModelRefreshStatus::default(),
        disabled_models: req.disabled_models,
        modified: Utc::now(),
    };
    Ok((meta, credential))
}

/// Get the limits and capabilities a profile's model has
///
/// # Arguments
///
/// * `endpoint` - The endpoint the profile calls
/// * `model_id` - The model the profile calls
/// * `limits` - The profile's own limits, if set
/// * `capabilities` - The profile's own capabilities, if set
fn effective_model(
    endpoint: &ModelEndpointEntity,
    model_id: &str,
    limits: Option<&ModelLimits>,
    capabilities: Option<&ModelCapabilities>,
) -> Result<(ModelLimits, ModelCapabilities), ApiError> {
    // the model must be in the endpoint's catalog
    let Some(entry) = endpoint.catalog_entry(model_id) else {
        return bad!(format!(
            "model_id `{model_id}` isn't in the endpoint's catalog; add it to the catalog first"
        ));
    };
    // the profile's own values replace the catalog entry's
    Ok((
        limits.cloned().unwrap_or_else(|| entry.limits.clone()),
        capabilities
            .cloned()
            .unwrap_or_else(|| entry.capabilities.clone()),
    ))
}

/// Check a profile against the endpoint it calls
///
/// # Arguments
///
/// * `profile` - The profile to check
/// * `endpoint` - The endpoint the profile calls
fn check_profile(
    profile: &ModelProfileEntity,
    endpoint: &ModelEndpointEntity,
) -> Result<(), ApiError> {
    // the protocol must be one the endpoint's provider serves
    bad_request(endpoint.provider.check_api(profile.api))?;
    // the model must be in the catalog, and the parameters must fit it
    let (limits, capabilities) = effective_model(
        endpoint,
        &profile.model_id,
        profile.limits.as_ref(),
        profile.capabilities.as_ref(),
    )?;
    bad_request(profile.parameters.check_fit(&limits, &capabilities))
}

/// Parse the limits a profile sets, always storing them as manual limits
///
/// # Arguments
///
/// * `field` - The name of the form field, for errors
/// * `raw` - The JSON encoded limits
fn parse_limits(field: &str, raw: &str) -> Result<ModelLimits, ApiError> {
    // parse the limits and ignore any source they name
    let mut limits: ModelLimits = parse_json_field(field, raw)?;
    limits.source = LimitsSource::Manual;
    Ok(limits)
}

/// Build a model profile from a create request
///
/// # Arguments
///
/// * `user` - The user creating the profile
/// * `req` - The profile request
/// * `groups` - The groups of the new profile
/// * `shared` - Shared Thorium objects
async fn build_profile(
    user: &User,
    req: ModelProfileEntityRequest,
    groups: &[String],
    shared: &Shared,
) -> Result<(ModelProfileEntity, Entity), ApiError> {
    // the endpoint must cover our groups
    let endpoint = resolve_reference(
        user,
        req.endpoint,
        EntityKinds::ModelEndpoint,
        "model_profile_endpoint",
        groups,
        shared,
    )
    .await?;
    // parse the JSON encoded settings
    let parameters: ModelParameters = match &req.parameters {
        Some(raw) => parse_json_field("model_profile_parameters", raw)?,
        None => ModelParameters::default(),
    };
    let limits = match &req.limits {
        Some(raw) => Some(parse_limits("model_profile_limits", raw)?),
        None => None,
    };
    // build the profile and check it against its endpoint
    let meta = ModelProfileEntity {
        endpoint: req.endpoint,
        model_id: req.model_id,
        api: req.api,
        parameters,
        limits,
        capabilities: req.capabilities,
        bans: HashMap::default(),
        modified: Utc::now(),
    };
    check_profile(&meta, endpoint_meta(&endpoint)?)?;
    Ok((meta, endpoint))
}

/// Build a model credential from a create request
///
/// # Arguments
///
/// * `user` - The user creating the credential
/// * `req` - The credential request
/// * `groups` - The groups of the new credential
/// * `shared` - Shared Thorium objects
async fn build_credential(
    user: &User,
    req: ModelCredentialEntityRequest,
    groups: &[String],
    shared: &Shared,
) -> Result<ModelCredentialEntity, ApiError> {
    // the kind's metadata must be valid
    bad_request(req.kind.validate())?;
    // an assumed role's source must be an access key that covers our groups
    if let Some(source) = req.kind.role_source() {
        let entity = resolve_reference(
            user,
            source,
            EntityKinds::ModelCredential,
            "AwsAssumeRole.source",
            groups,
            shared,
        )
        .await?;
        if !matches!(
            credential_meta(&entity)?.kind,
            CredentialKind::AwsAccessKey { .. }
        ) {
            return bad!(format!(
                "AwsAssumeRole.source must name an AwsAccessKey credential, but `{}` is a {} credential",
                entity.name,
                credential_meta(&entity)?.kind.cred_type()
            ));
        }
    }
    // the secrets must fit the kind
    let secrets = build_secrets(&req.kind, req.secret, req.session_token)?;
    // a credential created with a secret was last rotated now
    let now = Utc::now();
    let rotated = secrets.primary.as_ref().map(|_| now);
    Ok(ModelCredentialEntity {
        kind: req.kind,
        secrets,
        expires: req.expires,
        rotated,
        bans: HashMap::default(),
        modified: now,
    })
}

/// Write a reference association from a referrer to its target in the referrer's groups
///
/// # Arguments
///
/// * `user` - The user writing the reference
/// * `kind` - The kind of reference association
/// * `source` - The referrer's id and name
/// * `target` - The target's id and name
/// * `groups` - The referrer's groups
/// * `shared` - Shared Thorium objects
async fn write_reference(
    user: &User,
    kind: AssociationKind,
    source: (Uuid, String),
    target: (Uuid, String),
    groups: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // build both ends of the edge
    let source = AssociationTarget::Entity {
        id: source.0,
        name: source.1,
    };
    let target = AssociationTarget::Entity {
        id: target.0,
        name: target.1,
    };
    // write the edge in every group of the referrer
    db::associations::create(
        user,
        groups.len(),
        kind,
        source,
        &vec![(target, groups.to_vec())],
        None,
        Directionality::To,
        shared,
    )
    .await
}

/// Create a model endpoint, profile, or credential from a submitted form
///
/// The entity's row is written before its reference association, so a failed write only ever
/// leaves an unreferenced entity behind.
///
/// # Arguments
///
/// * `user` - The user creating the entity
/// * `id` - The id of the new entity
/// * `kind` - The kind of entity to create
/// * `form` - The submitted form
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::create", skip_all, fields(id = id.to_string()), err(Debug))]
pub(crate) async fn create(
    user: &User,
    id: Uuid,
    kind: EntityKinds,
    mut form: EntityForm,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // check the name and groups
    let name = check_name_and_groups(form.name.as_ref(), &form.groups)?;
    // only owners and managers of every group create endpoints and credentials
    if needs_manager(kind) {
        authorize_managers(user, &form.groups, "create", &name, shared).await?;
    }
    // build the kind's metadata and find what it references
    let (metadata, reference) = match kind {
        EntityKinds::ModelEndpoint => {
            let req = ModelEndpointEntityRequest::from_form(&mut form.metadata)?;
            let (meta, credential) = build_endpoint(user, &name, req, &form.groups, shared).await?;
            let reference = credential.map(|cred| (AssociationKind::AuthenticatesWith, cred));
            (EntityMetadata::ModelEndpoint(meta), reference)
        }
        EntityKinds::ModelProfile => {
            let req = ModelProfileEntityRequest::from_form(&mut form.metadata)?;
            let (meta, endpoint) = build_profile(user, req, &form.groups, shared).await?;
            (
                EntityMetadata::ModelProfile(meta),
                Some((AssociationKind::Serves, endpoint)),
            )
        }
        EntityKinds::ModelCredential => {
            let req = ModelCredentialEntityRequest::from_form(&mut form.metadata)?;
            let meta = Box::pin(build_credential(user, req, &form.groups, shared)).await?;
            (EntityMetadata::ModelCredential(meta), None)
        }
        _ => return internal_err!(format!("{kind} is not a model configuration kind")),
    };
    // build the entity
    let entity = Entity {
        id,
        name: name.clone(),
        kind,
        metadata,
        description: form.description,
        submitter: user.username.clone(),
        groups: form.groups,
        tags: HashMap::default(),
        image: form.image,
        created: Utc::now(),
    };
    // write the entity first, which makes it visible
    db::entities::insert(user, &entity, form.tags, shared).await?;
    // then write the association mirroring its reference
    if let Some((assoc_kind, target)) = reference {
        write_reference(
            user,
            assoc_kind,
            (entity.id, entity.name.clone()),
            (target.id, target.name),
            &entity.groups,
            shared,
        )
        .await?;
    }
    // a new endpoint is refreshed once it's fully written; a new credential has no users yet
    if kind == EntityKinds::ModelEndpoint {
        let target = ModelRefreshTarget::GroupEndpoint {
            groups: entity.groups.clone(),
            endpoint: id,
        };
        Event::queue_model_refresh(user, target, ModelRefreshReason::EndpointChanged, shared)
            .await?;
    }
    Ok(EntityResponse::new(id, name))
}

/// Whether an update form changes any of a model entity's bans
///
/// # Arguments
///
/// * `kind` - The kind of entity being updated
/// * `form` - The metadata update form
fn changes_bans(kind: EntityKinds, form: &EntityMetadataUpdateForm) -> bool {
    // each kind has its own ban fields
    match kind {
        EntityKinds::ModelEndpoint => {
            form.model_endpoint_bans.is_some() || form.model_endpoint_model_bans.is_some()
        }
        EntityKinds::ModelProfile => form.model_profile_bans.is_some(),
        EntityKinds::ModelCredential => form.model_credential_bans.is_some(),
        _ => false,
    }
}

/// Whether an update form changes a model entity in any way other than its bans
///
/// # Arguments
///
/// * `kind` - The kind of entity being updated
/// * `form` - The update form
/// * `image_uploaded` - Whether the update uploaded a new image
fn changes_more_than_bans(
    kind: EntityKinds,
    form: &EntityUpdateForm,
    image_uploaded: bool,
) -> bool {
    // changes to the shared entity fields
    let meta = &form.metadata;
    let base = form.name.is_some()
        || form.description.is_some()
        || form.clear_description == Some(true)
        || form.clear_image == Some(true)
        || image_uploaded
        || !form.add_groups.is_empty()
        || !form.remove_groups.is_empty();
    // changes to the kind's own fields
    let own = match kind {
        EntityKinds::ModelEndpoint => {
            meta.model_endpoint_provider.is_some()
                || meta.model_endpoint_credential.is_some()
                || meta.clear_model_endpoint_credential == Some(true)
                || meta.add_model_endpoint_models.is_some()
                || !meta.remove_model_endpoint_models.is_empty()
                || !meta.disable_model_endpoint_models.is_empty()
                || !meta.enable_model_endpoint_models.is_empty()
        }
        EntityKinds::ModelProfile => {
            meta.model_profile_endpoint.is_some()
                || meta.model_profile_model_id.is_some()
                || meta.model_profile_api.is_some()
                || meta.model_profile_parameters.is_some()
                || meta.model_profile_limits.is_some()
                || meta.clear_model_profile_limits == Some(true)
                || meta.model_profile_capabilities.is_some()
                || meta.clear_model_profile_capabilities == Some(true)
        }
        EntityKinds::ModelCredential => {
            meta.model_credential_secret.is_some()
                || meta.model_credential_session_token.is_some()
                || meta.clear_model_credential_session_token == Some(true)
                || meta.model_credential_expires.is_some()
                || meta.clear_model_credential_expires == Some(true)
        }
        _ => false,
    };
    base || own
}

/// Authorize a parsed update of a model entity before anything is written
///
/// Edits of endpoints and credentials need owner or manager in every group, and edits of
/// profiles need edit rights in every group. An update that only changes bans needs owner or
/// manager in any group, because bans are kept once per entity.
///
/// # Arguments
///
/// * `user` - The user updating the entity
/// * `entity` - The entity, in all of its groups
/// * `form` - The parsed update form
/// * `image_uploaded` - Whether the update uploaded a new image
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::authorize_update", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn authorize_update(
    user: &User,
    entity: &Entity,
    form: &EntityUpdateForm,
    image_uploaded: bool,
    shared: &Shared,
) -> Result<(), ApiError> {
    // only admins send refresh reports
    if form.metadata.model_endpoint_refresh_report.is_some() && !user.is_admin() {
        return unauthorized!(format!(
            "only admins may send a refresh report for `{}`",
            entity.name
        ));
    }
    // see what this update changes
    let bans = changes_bans(entity.kind, &form.metadata);
    let edits = changes_more_than_bans(entity.kind, form, image_uploaded);
    // edits, and updates that change nothing, need the kind's edit rights in every group
    if edits || !bans {
        authorize_edit(user, entity, shared).await?;
    }
    // ban changes need an owner or manager of any group
    if bans {
        authorize_any_manager(user, entity, "change the bans of", shared).await?;
    }
    Ok(())
}

/// Authorize an edit of a model entity
///
/// Edits of endpoints and credentials need owner or manager in every group, and edits of
/// profiles need edit rights in every group.
///
/// # Arguments
///
/// * `user` - The user editing the entity
/// * `entity` - The entity, in all of its groups
/// * `shared` - Shared Thorium objects
pub(crate) async fn authorize_edit(
    user: &User,
    entity: &Entity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // endpoints and credentials are edited only by owners and managers
    if needs_manager(entity.kind) {
        return authorize_managers(user, &entity.groups, "edit", &entity.name, shared).await;
    }
    // profiles are edited by anyone who can edit in every group
    Group::authorize_graph(
        user,
        &entity.groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "edit",
        &entity.name,
        shared,
    )
    .await?;
    Ok(())
}

/// Find the profiles that call an endpoint
///
/// # Arguments
///
/// * `user` - The user checking the endpoint
/// * `endpoint` - The endpoint, in all of its groups
/// * `shared` - Shared Thorium objects
async fn profiles_of(
    user: &User,
    endpoint: &Entity,
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // find the profiles whose `Serves` edges point at this endpoint
    let referrers =
        db::associations::referrers_in_groups(user, &[endpoint.id], &endpoint.groups, shared)
            .await?;
    let ids = referrers
        .into_values()
        .flatten()
        .filter(|referrer| referrer.kind == AssociationKind::Serves)
        .filter_map(|referrer| match referrer.source {
            AssociationTargetColumn::Entity(id) => Some(id),
            _ => None,
        })
        .collect::<BTreeSet<Uuid>>();
    // read each profile that still exists
    let mut profiles = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(profile) = read_reference(id, shared).await? {
            profiles.push(profile);
        }
    }
    Ok(profiles)
}

/// Apply an update to a model endpoint
///
/// # Arguments
///
/// * `user` - The user updating the endpoint
/// * `entity` - The endpoint, in all of its groups
/// * `form` - The metadata update form
/// * `shared` - Shared Thorium objects
#[allow(clippy::too_many_lines)]
async fn update_endpoint(
    user: &User,
    entity: &mut Entity,
    form: &mut EntityMetadataUpdateForm,
    shared: &Shared,
) -> Result<EndpointChanges, ApiError> {
    // change a copy so nothing is applied unless every check passes
    let mut meta = endpoint_meta(entity)?.clone();
    // a refresh report must hold only what a refresh reports
    let report = match form.model_endpoint_refresh_report.take() {
        Some(raw) => {
            let report: ModelRefreshReport =
                parse_json_field("model_endpoint_refresh_report", &raw)?;
            check_report(&report)?;
            Some(report)
        }
        None => None,
    };
    // a provider set by the update changes the endpoint's connection
    let provider_set = form.model_endpoint_provider.is_some();
    // a new provider must be valid on its own
    if let Some(raw) = form.model_endpoint_provider.take() {
        let provider: ModelProvider = parse_json_field("model_endpoint_provider", &raw)?;
        bad_request(provider.validate())?;
        // the deployment must allow the new provider and its url
        check_whitelist(&entity.name, &provider, shared).await?;
        meta.provider = provider;
    }
    // a credential is either set, cleared, or kept
    if form.clear_model_endpoint_credential == Some(true)
        && form.model_endpoint_credential.is_some()
    {
        return bad!("set or clear model_endpoint_credential, not both".to_owned());
    }
    let old_credential = meta.credential;
    let mut new_target = None;
    if form.clear_model_endpoint_credential == Some(true) {
        meta.credential = None;
    } else if let Some(id) = form.model_endpoint_credential.take() {
        // the new credential must fit the provider and cover our groups
        let credential = resolve_reference(
            user,
            id,
            EntityKinds::ModelCredential,
            "model_endpoint_credential",
            &entity.groups,
            shared,
        )
        .await?;
        bad_request(
            meta.provider
                .check_credential(&credential_meta(&credential)?.kind),
        )?;
        meta.credential = Some(id);
        new_target = Some((id, credential.name));
    }
    // a kept credential must still fit a changed provider, and may have stopped resolving
    let mut broken = false;
    if new_target.is_none()
        && let Some(id) = meta.credential
    {
        match read_reference(id, shared).await? {
            Some(credential) => {
                bad_request(
                    meta.provider
                        .check_credential(&credential_meta(&credential)?.kind),
                )?;
            }
            None => broken = true,
        }
    }
    sync_broken_reference(&mut meta.bans, meta.credential, broken);
    // added catalog entries are stored as manual entries, replacing any with the same id
    if let Some(raw) = form.add_model_endpoint_models.take() {
        let models: Vec<ModelCatalogEntryRequest> =
            parse_json_field("add_model_endpoint_models", &raw)?;
        for entry in bad_request(build_catalog(models))? {
            // a replaced entry keeps its bans
            let mut entry = entry;
            if let Some(existing) = meta.catalog_entry(&entry.model_id) {
                entry.bans.clone_from(&existing.bans);
            }
            meta.upsert_entry(entry);
        }
    }
    // removed catalog entries must exist and must not be named by a profile
    if !form.remove_model_endpoint_models.is_empty() {
        let remove = std::mem::take(&mut form.remove_model_endpoint_models)
            .into_iter()
            .collect::<BTreeSet<String>>();
        for model_id in &remove {
            if meta.catalog_entry(model_id).is_none() {
                return bad!(format!(
                    "model_id `{model_id}` isn't in the endpoint's catalog"
                ));
            }
        }
        // a model a profile calls can't be removed
        let named = profiles_of(user, entity, shared)
            .await?
            .into_iter()
            .filter(|profile| match &profile.metadata {
                EntityMetadata::ModelProfile(profile) => remove.contains(&profile.model_id),
                _ => false,
            })
            .map(|profile| format!("`{}` ({})", profile.name, profile.id))
            .collect::<Vec<String>>();
        if !named.is_empty() {
            return conflict!(format!(
                "the catalog models being removed are called by model profiles: {}",
                named.join(", ")
            ));
        }
        meta.catalog
            .retain(|entry| !remove.contains(&entry.model_id));
    }
    // take the ids to disable and enable
    let disable = std::mem::take(&mut form.disable_model_endpoint_models);
    let enable = std::mem::take(&mut form.enable_model_endpoint_models);
    // every id must be a valid model id
    check_model_ids("disable_model_endpoint_models", &disable)?;
    check_model_ids("enable_model_endpoint_models", &enable)?;
    // disabling and enabling one id at once is ambiguous
    if let Some(both) = disable.intersection(&enable).next() {
        return bad!(format!(
            "model id `{both}` can't be disabled and enabled at once"
        ));
    }
    // apply the disables and enables
    meta.disabled_models.extend(disable);
    meta.disabled_models.retain(|id| !enable.contains(id));
    // apply ban changes to the endpoint
    if let Some(raw) = form.model_endpoint_bans.take() {
        let update: ModelBanUpdate = parse_json_field("model_endpoint_bans", &raw)?;
        apply_bans(user, &mut meta.bans, update)?;
    }
    // apply ban changes to catalog entries
    if let Some(raw) = form.model_endpoint_model_bans.take() {
        let updates: BTreeMap<String, ModelBanUpdate> =
            parse_json_field("model_endpoint_model_bans", &raw)?;
        for (model_id, update) in updates {
            let Ok(index) = meta
                .catalog
                .binary_search_by(|entry| entry.model_id.cmp(&model_id))
            else {
                return not_found!(format!(
                    "model_id `{model_id}` isn't in the endpoint's catalog"
                ));
            };
            apply_bans(user, &mut meta.catalog[index].bans, update)?;
        }
    }
    // merge a refresh report, noting what it does to the group credential the refresh used;
    // only a report naming the endpoint's current credential changes it, so a report from a
    // refresh that used an earlier credential never bans the new one
    let credential_effect = match report {
        Some(report) => {
            merge_refresh_report(
                &mut RefreshedEndpoint {
                    catalog: &mut meta.catalog,
                    bans: &mut meta.bans,
                    refresh: &mut meta.refresh,
                },
                &report,
            );
            meta.credential
                .filter(|credential| report.credential == Some(ModelConfigRef::Group(*credential)))
                .map(|credential| (credential, credential_effect(&report)))
        }
        None => None,
    };
    // mark the endpoint modified and apply the changes
    meta.modified = Utc::now();
    let moved = meta.credential != old_credential;
    entity.metadata = EntityMetadata::ModelEndpoint(meta);
    // move the credential's association once the endpoint is written
    Ok(EndpointChanges {
        moved: moved.then(|| ReferenceMove {
            kind: AssociationKind::AuthenticatesWith,
            source: entity.id,
            source_name: entity.name.clone(),
            old_target: old_credential,
            new_target,
            groups: entity.groups.clone(),
        }),
        credential_effect,
        connection_changed: provider_set || moved,
    })
}

/// Apply an update to a model profile
///
/// # Arguments
///
/// * `user` - The user updating the profile
/// * `entity` - The profile, in all of its groups
/// * `form` - The metadata update form
/// * `shared` - Shared Thorium objects
async fn update_profile(
    user: &User,
    entity: &mut Entity,
    form: &mut EntityMetadataUpdateForm,
    shared: &Shared,
) -> Result<Option<ReferenceMove>, ApiError> {
    // change a copy so nothing is applied unless every check passes
    let mut meta = match &entity.metadata {
        EntityMetadata::ModelProfile(meta) => meta.clone(),
        _ => return internal_err!(format!("{} is not a model profile", entity.id)),
    };
    let old_endpoint = meta.endpoint;
    // a new endpoint must cover our groups, and a kept one may have stopped resolving
    let mut new_target = None;
    let endpoint = match form.model_profile_endpoint.take() {
        Some(id) => {
            let endpoint = resolve_reference(
                user,
                id,
                EntityKinds::ModelEndpoint,
                "model_profile_endpoint",
                &entity.groups,
                shared,
            )
            .await?;
            meta.endpoint = id;
            new_target = Some((id, endpoint.name.clone()));
            Some(endpoint)
        }
        None => read_reference(meta.endpoint, shared).await?,
    };
    sync_broken_reference(&mut meta.bans, Some(meta.endpoint), endpoint.is_none());
    // apply the model, protocol, and settings changes
    let mut changed = new_target.is_some();
    if let Some(model_id) = form.model_profile_model_id.take() {
        meta.model_id = model_id;
        changed = true;
    }
    if let Some(api) = form.model_profile_api.take() {
        meta.api = api;
        changed = true;
    }
    if let Some(raw) = form.model_profile_parameters.take() {
        meta.parameters = parse_json_field("model_profile_parameters", &raw)?;
        changed = true;
    }
    if form.clear_model_profile_limits == Some(true) {
        meta.limits = None;
        changed = true;
    } else if let Some(raw) = form.model_profile_limits.take() {
        meta.limits = Some(parse_limits("model_profile_limits", &raw)?);
        changed = true;
    }
    if form.clear_model_profile_capabilities == Some(true) {
        meta.capabilities = None;
        changed = true;
    } else if let Some(raw) = form.model_profile_capabilities.take() {
        meta.capabilities = Some(parse_json_field("model_profile_capabilities", &raw)?);
        changed = true;
    }
    // a changed profile must still fit its endpoint
    if changed {
        match &endpoint {
            Some(endpoint) => check_profile(&meta, endpoint_meta(endpoint)?)?,
            None => {
                return bad!(format!(
                    "endpoint {} no longer exists; set model_profile_endpoint to a valid endpoint",
                    meta.endpoint
                ));
            }
        }
    }
    // apply ban changes
    if let Some(raw) = form.model_profile_bans.take() {
        let update: ModelBanUpdate = parse_json_field("model_profile_bans", &raw)?;
        apply_bans(user, &mut meta.bans, update)?;
    }
    // mark the profile modified and apply the changes
    meta.modified = Utc::now();
    let moved = meta.endpoint != old_endpoint;
    entity.metadata = EntityMetadata::ModelProfile(meta);
    // move the endpoint's association once the profile is written
    Ok(moved.then(|| ReferenceMove {
        kind: AssociationKind::Serves,
        source: entity.id,
        source_name: entity.name.clone(),
        old_target: Some(old_endpoint),
        new_target,
        groups: entity.groups.clone(),
    }))
}

/// Apply an update to a model credential
///
/// Setting a secret rotates the credential: its fingerprint and rotation time are updated and
/// the bans a rejected or expired secret registered are cleared.
///
/// # Arguments
///
/// * `user` - The user updating the credential
/// * `entity` - The credential, in all of its groups
/// * `form` - The metadata update form
fn update_credential(
    user: &User,
    entity: &mut Entity,
    form: &mut EntityMetadataUpdateForm,
) -> Result<bool, ApiError> {
    // change a copy so nothing is applied unless every check passes
    let mut meta = credential_meta(entity)?.clone();
    let now = Utc::now();
    // a new secret rotates the credential
    let rotated = form.model_credential_secret.is_some();
    if let Some(secret) = form.model_credential_secret.take() {
        // empty secrets are never stored
        if secret.is_empty() {
            return bad!("model_credential_secret can't be empty".to_owned());
        }
        // an assumed role holds no secret
        if !meta.kind.needs_secret() {
            return bad!(
                "an AwsAssumeRole credential holds no secret; its source credential does"
                    .to_owned()
            );
        }
        meta.secrets.fingerprint = Some(secret.fingerprint());
        meta.secrets.primary = Some(secret);
        meta.rotated = Some(now);
        // a new secret clears the bans the old one earned
        meta.bans.retain(|_, ban| {
            !matches!(
                ban.ban_kind,
                ModelBanKind::CredentialRejected { .. } | ModelBanKind::CredentialExpired
            )
        });
    }
    // a session token is set or cleared on access keys only
    if form.clear_model_credential_session_token == Some(true) {
        meta.secrets.session_token = None;
    } else if let Some(token) = form.model_credential_session_token.take() {
        if token.is_empty() {
            return bad!("model_credential_session_token can't be empty".to_owned());
        }
        if !matches!(meta.kind, CredentialKind::AwsAccessKey { .. }) {
            return bad!("only an AwsAccessKey credential takes a session token".to_owned());
        }
        meta.secrets.session_token = Some(token);
    }
    // the expiration is set or cleared
    if form.clear_model_credential_expires == Some(true) {
        meta.expires = None;
    } else if let Some(expires) = form.model_credential_expires.take() {
        meta.expires = Some(expires);
    }
    // apply ban changes
    if let Some(raw) = form.model_credential_bans.take() {
        let update: ModelBanUpdate = parse_json_field("model_credential_bans", &raw)?;
        apply_bans(user, &mut meta.bans, update)?;
    }
    // mark the credential modified and apply the changes
    meta.modified = now;
    entity.metadata = EntityMetadata::ModelCredential(meta);
    Ok(rotated)
}

/// Apply the metadata of a parsed update to a model entity
///
/// Returns the reference association to move and the ban changes to notify about once the
/// entity is written.
///
/// # Arguments
///
/// * `user` - The user updating the entity
/// * `entity` - The entity, in all of its (possibly changed) groups
/// * `form` - The metadata update form
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::update_meta", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn update_meta(
    user: &User,
    entity: &mut Entity,
    mut form: EntityMetadataUpdateForm,
    shared: &Shared,
) -> Result<ModelUpdate, ApiError> {
    // note the bans before the update
    let before = ban_index(&entity.metadata);
    // apply the kind's own update
    let mut changes = EndpointChanges::default();
    let mut rotated = false;
    match entity.kind {
        EntityKinds::ModelEndpoint => {
            changes = Box::pin(update_endpoint(user, entity, &mut form, shared)).await?;
        }
        EntityKinds::ModelProfile => {
            changes.moved = Box::pin(update_profile(user, entity, &mut form, shared)).await?;
        }
        EntityKinds::ModelCredential => {
            rotated = update_credential(user, entity, &mut form)?;
        }
        kind => return internal_err!(format!("{kind} is not a model configuration kind")),
    }
    // find the bans the update added and removed
    let bans = ModelBanChanges::between(&before, entity);
    Ok(ModelUpdate {
        moved: changes.moved,
        bans,
        credential_effect: changes.credential_effect,
        refresh: changes
            .connection_changed
            .then_some(ModelRefreshReason::EndpointChanged),
        rotated,
    })
}

/// Apply what a refresh report does to a group credential, under the credential's lock
///
/// The endpoint's lock is held by the caller, so locks are always taken endpoint first, and the
/// credential's lock is waited on only briefly; a credential another write holds longer refuses
/// the report with a conflict, and the refresh sends it again.
///
/// # Arguments
///
/// * `user` - The user who sent the report
/// * `credential` - The credential the refresh used
/// * `effect` - What the report does to it
/// * `shared` - Shared Thorium objects
async fn apply_report_to_credential(
    user: &User,
    credential: Uuid,
    effect: &CredentialEffect,
    shared: &Shared,
) -> Result<(), ApiError> {
    // nothing to do for an effect that changes nothing
    if *effect == CredentialEffect::Nothing {
        return Ok(());
    }
    // re-read the credential under its lock, skipping one deleted since the refresh
    let token =
        db::entities::lock_instances(credential, "model credential", CREDENTIAL_LOCK_WAIT, shared)
            .await?;
    let result = async {
        let Some(mut entity) = read_reference(credential, shared).await? else {
            return Ok(());
        };
        let before = ban_index(&entity.metadata);
        let EntityMetadata::ModelCredential(meta) = &mut entity.metadata else {
            return Ok(());
        };
        // write and notify only when the bans changed
        if !apply_credential_effect(&mut meta.bans, effect) {
            return Ok(());
        }
        meta.modified = Utc::now();
        let changes = ModelBanChanges::between(&before, &entity);
        db::entities::update(user, entity, &[], &[], shared).await?;
        changes.notify(shared).await
    }
    .await;
    // release the lock whether or not the write worked
    tools::unlock(credential, &token, shared).await?;
    result
}

/// Queue a refresh of each endpoint that authenticates with a credential
///
/// # Arguments
///
/// * `user` - The user who changed the credential
/// * `credential` - The credential, in all of its groups
/// * `shared` - Shared Thorium objects
async fn refresh_users_of(
    user: &User,
    credential: &Entity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find the endpoints whose `AuthenticatesWith` edges point at the credential
    let referrers =
        db::associations::referrers_in_groups(user, &[credential.id], &credential.groups, shared)
            .await?;
    let endpoints = referrers
        .into_values()
        .flatten()
        .filter(|referrer| referrer.kind == AssociationKind::AuthenticatesWith)
        .filter_map(|referrer| match referrer.source {
            AssociationTargetColumn::Entity(id) => Some(id),
            _ => None,
        })
        .collect::<BTreeSet<Uuid>>();
    // queue one refresh per endpoint the deployment still allows, in all of its groups
    let reason = ModelRefreshReason::CredentialChanged {
        credential: ModelConfigRef::Group(credential.id),
    };
    let settings = db::system::get_settings(shared).await?;
    for endpoint in endpoints {
        let Some(entity) = refreshable_endpoint(endpoint, &settings.models, shared).await? else {
            continue;
        };
        let target = ModelRefreshTarget::GroupEndpoint {
            groups: entity.groups,
            endpoint,
        };
        Event::queue_model_refresh(user, target, reason.clone(), shared).await?;
    }
    Ok(())
}

/// Finish an update of a model entity once its row is written
///
/// This moves its reference association, writes and deletes the notifications of the bans it
/// changed, applies a merged refresh report to the group credential the refresh used, and
/// queues the refreshes the update calls for.
///
/// # Arguments
///
/// * `user` - The user who updated the entity
/// * `entity` - The entity as written, in all of its groups
/// * `update` - What applying the update needs done
/// * `shared` - Shared Thorium objects
pub(crate) async fn finish_update(
    user: &User,
    entity: &Entity,
    update: ModelUpdate,
    shared: &Shared,
) -> Result<(), ApiError> {
    // move a reference once the referrer is written in its final groups
    Box::pin(apply_move(user, update.moved, shared)).await?;
    // write and delete the notifications of the bans the update changed
    update.bans.notify(shared).await?;
    // a refresh report bans or unbans the group credential it used
    if let Some((credential, effect)) = &update.credential_effect {
        Box::pin(apply_report_to_credential(
            user,
            *credential,
            effect,
            shared,
        ))
        .await?;
    }
    // a changed connection refreshes the endpoint, if the deployment still allows it
    if let Some(reason) = update.refresh
        && let EntityMetadata::ModelEndpoint(meta) = &entity.metadata
        && refresh_allowed(
            &db::system::get_settings(shared).await?.models,
            &meta.provider,
            &meta.bans,
        )
    {
        let target = ModelRefreshTarget::GroupEndpoint {
            groups: entity.groups.clone(),
            endpoint: entity.id,
        };
        Event::queue_model_refresh(user, target, reason, shared).await?;
    }
    // a rotated secret refreshes every endpoint that uses it
    if update.rotated {
        Box::pin(refresh_users_of(user, entity, shared)).await?;
    }
    Ok(())
}

/// Ask for a refresh of a model endpoint, marking it pending
///
/// Owners and managers of any of the endpoint's groups, and admins, may ask; the endpoint must
/// still be on the deployment's allowed endpoint list. A refresh still pending from a request in
/// the last ten minutes is already queued, so asking again only answers; an older pending
/// refresh is queued again in case its event was lost.
///
/// # Arguments
///
/// * `user` - The user asking for the refresh
/// * `id` - The endpoint's id
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::request_refresh", skip(user, shared), err(Debug))]
pub(crate) async fn request_refresh(
    user: &User,
    id: Uuid,
    shared: &Shared,
) -> Result<(), ApiError> {
    // the endpoint must exist and be visible
    let mut entity = Entity::get(user, id, shared).await?;
    if entity.kind != EntityKinds::ModelEndpoint {
        return not_found!(format!("Model endpoint {id} not found"));
    }
    // an owner or manager of any of its groups may refresh it
    entity.groups = db::entities::all_groups(id, shared).await?;
    authorize_any_manager(user, &entity, "refresh", shared).await?;
    // the deployment must still allow it
    check_whitelist(&entity.name, &endpoint_meta(&entity)?.provider, shared).await?;
    // mark it pending under its lock, rereading it so no other write is lost
    let token = tools::lock(id, &entity.name, shared).await?;
    let result = async {
        let Some(mut fresh) = read_reference(id, shared).await? else {
            return not_found!(format!("Model endpoint {id} not found"));
        };
        let EntityMetadata::ModelEndpoint(meta) = &mut fresh.metadata else {
            return not_found!(format!("Model endpoint {id} not found"));
        };
        // a refresh asked for recently and still pending is already queued
        let now = Utc::now();
        if meta.refresh.state == ModelRefreshState::Pending
            && meta
                .refresh
                .requested
                .is_some_and(|requested| now - requested < PENDING_REFRESH_WINDOW)
        {
            return Ok(None);
        }
        meta.refresh.state = ModelRefreshState::Pending;
        meta.refresh.requested = Some(now);
        let groups = fresh.groups.clone();
        db::entities::update(user, fresh, &[], &[], shared).await?;
        Ok(Some(groups))
    }
    .await;
    // release the lock whether or not the write worked
    tools::unlock(id, &token, shared).await?;
    let Some(groups) = result? else {
        return Ok(());
    };
    // queue the refresh in every group of the endpoint
    let target = ModelRefreshTarget::GroupEndpoint {
        groups,
        endpoint: id,
    };
    Event::queue_model_refresh(user, target, ModelRefreshReason::Manual, shared).await
}

/// Read a group model credential with its secrets, which only admins may do
///
/// # Arguments
///
/// * `user` - The user reading the credential
/// * `id` - The credential's id
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::credential_secret", skip(user, shared), err(Debug))]
pub(crate) async fn credential_secret(
    user: &User,
    id: Uuid,
    shared: &Shared,
) -> Result<ModelCredentialEntity, ApiError> {
    // only admins read secrets
    if !user.is_admin() {
        return unauthorized!();
    }
    // the credential must exist
    match read_reference(id, shared).await? {
        Some(Entity {
            metadata: EntityMetadata::ModelCredential(meta),
            ..
        }) => Ok(meta),
        _ => not_found!(format!("Model credential {id} not found")),
    }
}

/// Move a reference association after its referrer's row is written
///
/// # Arguments
///
/// * `user` - The user who updated the referrer
/// * `moved` - The reference to move, if one moved
/// * `shared` - Shared Thorium objects
pub(crate) async fn apply_move(
    user: &User,
    moved: Option<ReferenceMove>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // nothing moves without a changed reference
    let Some(moved) = moved else {
        return Ok(());
    };
    // drop the edges to the old target in every group
    if let Some(old) = moved.old_target {
        let source = AssociationTargetColumn::Entity(moved.source);
        let old_column = serde_json::to_string(&AssociationTargetColumn::Entity(old))?;
        let opts = AssociationListOpts::default()
            .groups(moved.groups.clone())
            .limit(500);
        let mut cursor = db::associations::list(user, opts, &source, shared).await?;
        loop {
            // keep only this reference's edges to the old target
            let stale = cursor
                .data
                .drain(..)
                .filter(|assoc| assoc.kind == moved.kind && assoc.other == old_column)
                .collect::<Vec<_>>();
            db::associations::delete_many(&source, &stale, shared).await?;
            // stop once every page was read
            if cursor.exhausted() {
                break;
            }
            cursor.next(shared).await?;
        }
    }
    // write the edge to the new target
    if let Some(target) = moved.new_target {
        write_reference(
            user,
            moved.kind,
            (moved.source, moved.source_name),
            target,
            &moved.groups,
            shared,
        )
        .await?;
    }
    Ok(())
}

/// Describe an entity referrer for a refusal
///
/// # Arguments
///
/// * `id` - The referrer's id
/// * `shared` - Shared Thorium objects
async fn describe(id: Uuid, shared: &Shared) -> Result<String, ApiError> {
    // name the referrer by its kind and name when it still exists
    Ok(match read_reference(id, shared).await? {
        Some(entity) => format!("{} `{}` ({id})", entity.kind, entity.name),
        None => format!("entity {id}"),
    })
}

/// List the ids of every entity of one kind in some groups
///
/// # Arguments
///
/// * `user` - The user listing the entities
/// * `kind` - The kind of entity to list
/// * `groups` - The groups to list from
/// * `shared` - Shared Thorium objects
pub(crate) async fn list_ids(
    user: &User,
    kind: EntityKinds,
    groups: &[String],
    shared: &Shared,
) -> Result<HashSet<Uuid>, ApiError> {
    // list the kind in these groups
    let params = EntityListParams {
        groups: groups.to_vec(),
        kinds: vec![kind],
        limit: 10_000,
        ..EntityListParams::default()
    };
    let mut cursor = db::entities::list(user, params, true, shared).await?;
    let mut ids = HashSet::new();
    loop {
        // keep each entity once
        ids.extend(cursor.data.drain(..).map(|line| line.id));
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        cursor.next(shared).await?;
    }
    Ok(ids)
}

/// Find the assumed-role credentials in some groups whose source is an access key
///
/// # Arguments
///
/// * `user` - The user checking the access key
/// * `source` - The access key credential
/// * `groups` - The groups to look in
/// * `shared` - Shared Thorium objects
async fn roles_using(
    user: &User,
    source: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // list the model credentials in these groups
    let ids = list_ids(user, EntityKinds::ModelCredential, groups, shared).await?;
    // keep the assumed roles whose source is this access key
    let mut roles = Vec::new();
    for id in ids {
        if let Some(entity) = read_reference(id, shared).await?
            && let EntityMetadata::ModelCredential(meta) = &entity.metadata
            && meta.kind.role_source() == Some(source)
        {
            roles.push(entity);
        }
    }
    Ok(roles)
}

/// Make sure a model entity's group change keeps its references contained
///
/// Added groups of an endpoint or credential need owners or managers like a create, a referrer
/// gaining a group needs its target in that group, and a target can't lose a group while
/// something in it references it.
///
/// # Arguments
///
/// * `user` - The user changing the groups
/// * `entity` - The entity, in all of its groups before the change
/// * `add` - The groups being added
/// * `remove` - The groups being removed
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::check_group_change", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn check_group_change(
    user: &User,
    entity: &Entity,
    add: &[String],
    remove: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // only owners and managers may put endpoints and credentials in a group
    if needs_manager(entity.kind) && !add.is_empty() {
        let _ = Group::authorize_check_allow_all(
            user,
            add,
            Group::modifiable,
            "manage",
            Some(GroupAllowAction::Entities),
            shared,
        )
        .await?;
    }
    // a referrer's target must be in every added group
    if !add.is_empty() {
        let target = match &entity.metadata {
            EntityMetadata::ModelEndpoint(meta) => meta.credential,
            EntityMetadata::ModelProfile(meta) => Some(meta.endpoint),
            EntityMetadata::ModelCredential(meta) => meta.kind.role_source(),
            _ => None,
        };
        if let Some(target) = target
            && let Some(target) = read_reference(target, shared).await?
        {
            let missing = crate::utils::helpers::missing_groups(add, &target.groups);
            if !missing.is_empty() {
                let visible = visible_groups(user, &missing)?;
                return bad!(format!(
                    "{} `{}` isn't in groups {visible:?}; add those groups to it first",
                    kind_name(target.kind),
                    target.name
                ));
            }
        }
    }
    // a target can't lose a group while something in it references it
    if !remove.is_empty() && needs_manager(entity.kind) {
        let mut referrers = BTreeMap::<String, BTreeSet<Uuid>>::new();
        // the association referrers in the removed groups
        let found =
            db::associations::referrers_in_groups(user, &[entity.id], remove, shared).await?;
        for (group, refs) in found {
            for referrer in refs {
                if matches!(
                    referrer.kind,
                    AssociationKind::AuthenticatesWith | AssociationKind::Serves
                ) && let AssociationTargetColumn::Entity(id) = referrer.source
                {
                    referrers.entry(group.clone()).or_default().insert(id);
                }
            }
        }
        // the assumed roles in the removed groups whose source is this access key
        if let EntityMetadata::ModelCredential(meta) = &entity.metadata
            && matches!(meta.kind, CredentialKind::AwsAccessKey { .. })
        {
            for role in roles_using(user, entity.id, remove, shared).await? {
                for group in role.groups.iter().filter(|group| remove.contains(group)) {
                    referrers.entry(group.clone()).or_default().insert(role.id);
                }
            }
        }
        // refuse the removal, naming only the referrers in groups this user can see
        if !referrers.is_empty() {
            let mut listed = Vec::new();
            for (group, ids) in referrers {
                if user.is_admin() || user.groups.contains(&group) {
                    let mut names = Vec::with_capacity(ids.len());
                    for id in ids {
                        names.push(describe(id, shared).await?);
                    }
                    listed.push(format!("in `{group}`: {}", names.join(", ")));
                }
            }
            let msg = if listed.is_empty() {
                format!(
                    "{} `{}` is still referenced in the groups being removed",
                    kind_name(entity.kind),
                    entity.name
                )
            } else {
                format!(
                    "{} `{}` is still referenced in the groups being removed: {}",
                    kind_name(entity.kind),
                    entity.name,
                    listed.join("; ")
                )
            };
            return conflict!(msg);
        }
    }
    Ok(())
}

/// Make sure a referenced model entity isn't deleted
///
/// An endpoint a profile calls, or a credential an endpoint authenticates with, can't be
/// deleted; the refusal lists the referrers.
///
/// # Arguments
///
/// * `user` - The user deleting the entity
/// * `entity` - The entity, in all of its groups
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::check_delete", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn check_delete(
    user: &User,
    entity: &Entity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // only endpoints and credentials are referenced
    if !needs_manager(entity.kind) {
        return Ok(());
    }
    // find the referrers in every group
    let found =
        db::associations::referrers_in_groups(user, &[entity.id], &entity.groups, shared).await?;
    let ids = found
        .into_values()
        .flatten()
        .filter(|referrer| {
            matches!(
                referrer.kind,
                AssociationKind::AuthenticatesWith | AssociationKind::Serves
            )
        })
        .filter_map(|referrer| match referrer.source {
            AssociationTargetColumn::Entity(id) => Some(id),
            _ => None,
        })
        .collect::<BTreeSet<Uuid>>();
    // a referenced entity can't be deleted
    if !ids.is_empty() {
        let mut names = Vec::with_capacity(ids.len());
        for id in ids {
            names.push(describe(id, shared).await?);
        }
        return conflict!(format!(
            "{} `{}` is still referenced by {}; change or delete them first",
            kind_name(entity.kind),
            entity.name,
            names.join(", ")
        ));
    }
    Ok(())
}

/// Ban an assumed role for losing its source access key if that key is its source
///
/// Returns whether the key is the role's source, so the role must be written, even when the
/// role was already banned for it.
///
/// # Arguments
///
/// * `role` - The assumed role's metadata
/// * `source` - The access key being deleted
fn ban_for_lost_source(role: &mut ModelCredentialEntity, source: Uuid) -> bool {
    // a role that assumes from another key keeps its bans
    if role.kind.role_source() != Some(source) {
        return false;
    }
    // ban the role for its missing source
    sync_broken_reference(&mut role.bans, Some(source), true);
    role.modified = Utc::now();
    true
}

/// Ban the assumed-role credentials whose source access key is being deleted
///
/// A role another write holds past a short wait refuses the delete with a conflict.
///
/// # Arguments
///
/// * `user` - The user deleting the access key
/// * `entity` - The credential being deleted, in all of its groups
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::ban_dependents", skip_all, fields(id = entity.id.to_string()), err(Debug))]
pub(crate) async fn ban_dependents(
    user: &User,
    entity: &Entity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // only access keys are the source of other credentials
    let EntityMetadata::ModelCredential(meta) = &entity.metadata else {
        return Ok(());
    };
    if !matches!(meta.kind, CredentialKind::AwsAccessKey { .. }) {
        return Ok(());
    }
    // a role's groups are a subset of its source's, so look in the access key's groups
    for found in roles_using(user, entity.id, &entity.groups, shared).await? {
        // change each role under its lock, taken after the access key's that the delete holds,
        // waiting only briefly so a role another write holds refuses the delete with a conflict
        let token =
            db::entities::lock_instances(found.id, &found.name, CREDENTIAL_LOCK_WAIT, shared)
                .await?;
        let result: Result<Option<ModelBanChanges>, ApiError> = async {
            // reread the role under its lock, skipping roles deleted since the search
            let Some(mut role) = read_reference(found.id, shared).await? else {
                return Ok(None);
            };
            // ban the role only if its reread copy still names the access key as its source
            let before = ban_index(&role.metadata);
            let banned = match &mut role.metadata {
                EntityMetadata::ModelCredential(role_meta) => {
                    ban_for_lost_source(role_meta, entity.id)
                }
                _ => false,
            };
            if !banned {
                return Ok(None);
            }
            let changes = ModelBanChanges::between(&before, &role);
            // rewrite the role in every group
            db::entities::update(user, role, &[], &[], shared).await?;
            Ok(Some(changes))
        }
        .await;
        // release the lock whether or not the write worked
        tools::unlock(found.id, &token, shared).await?;
        // notify about the role's new ban once its lock is released
        if let Some(changes) = result? {
            changes.notify(shared).await?;
        }
    }
    Ok(())
}

/// Add or clear the `NotAllowedByDeployment` ban on a set of bans
///
/// Returns whether the bans changed.
///
/// # Arguments
///
/// * `bans` - The bans of an endpoint
/// * `allowed` - Whether the endpoint is on the allowed endpoint list
pub(crate) fn sync_whitelist_ban(bans: &mut HashMap<Uuid, ModelBan>, allowed: bool) -> bool {
    // find whether the endpoint is banned for being outside the list
    let banned = bans
        .values()
        .any(|ban| ban.ban_kind == ModelBanKind::NotAllowedByDeployment);
    match (allowed, banned) {
        // an allowed endpoint loses every whitelist ban
        (true, true) => {
            bans.retain(|_, ban| ban.ban_kind != ModelBanKind::NotAllowedByDeployment);
            true
        }
        // an endpoint outside the list is banned once
        (false, false) => {
            let ban = ModelBan::new(ModelBanKind::NotAllowedByDeployment);
            bans.insert(ban.id, ban);
            true
        }
        // the bans already match
        _ => false,
    }
}

/// Add or clear an endpoint's `NotAllowedByDeployment` ban, marking it modified if it changed
///
/// Returns whether the bans changed.
///
/// # Arguments
///
/// * `entity` - The endpoint
/// * `conf` - The allowed endpoint list
fn sync_endpoint_whitelist_ban(entity: &mut Entity, conf: &ModelsConf) -> bool {
    // only endpoints are on the allowed endpoint list
    let EntityMetadata::ModelEndpoint(meta) = &mut entity.metadata else {
        return false;
    };
    let synced = sync_whitelist_ban(&mut meta.bans, conf.allows(&meta.provider));
    if synced {
        meta.modified = Utc::now();
    }
    synced
}

/// Ban every group model endpoint outside the allowed endpoint list and unban those inside it
///
/// Each endpoint whose bans change is rewritten under its lock.
///
/// # Arguments
///
/// * `conf` - The allowed endpoint list
/// * `user` - The admin running the scan
/// * `shared` - Shared Thorium objects
#[instrument(name = "models::scan_whitelist", skip_all, err(Debug))]
pub(crate) async fn scan_whitelist(
    conf: &ModelsConf,
    user: &User,
    shared: &Shared,
) -> Result<(), ApiError> {
    // page through every group in Thorium
    let mut groups = BTreeSet::new();
    let mut cursor = 0;
    loop {
        let page = db::groups::list(cursor, 10_000, shared).await?;
        groups.extend(page.names);
        match page.cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }
    // list the endpoints in every group, a batch of groups at a time
    let groups = groups.into_iter().collect::<Vec<String>>();
    let mut ids = HashSet::new();
    for batch in groups.chunks(100) {
        ids.extend(list_ids(user, EntityKinds::ModelEndpoint, batch, shared).await?);
    }
    for id in ids {
        // read each endpoint that still exists in all of its groups
        let Some(mut entity) = read_reference(id, shared).await? else {
            continue;
        };
        // only endpoints whose bans would change are locked and written
        if !sync_endpoint_whitelist_ban(&mut entity, conf) {
            continue;
        }
        // change the endpoint under its lock, rereading it so no other write is lost
        let token = tools::lock(id, &entity.name, shared).await?;
        let result: Result<Option<ModelBanChanges>, ApiError> = async {
            let Some(mut entity) = read_reference(id, shared).await? else {
                return Ok(None);
            };
            let before = ban_index(&entity.metadata);
            if !sync_endpoint_whitelist_ban(&mut entity, conf) {
                return Ok(None);
            }
            let changes = ModelBanChanges::between(&before, &entity);
            db::entities::update(user, entity, &[], &[], shared).await?;
            Ok(Some(changes))
        }
        .await;
        // release the lock whether or not the write worked
        tools::unlock(id, &token, shared).await?;
        // notify about the endpoint's changed bans once its lock is released
        if let Some(changes) = result? {
            changes.notify(shared).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use uuid::Uuid;

    use super::{
        CredentialEffect, RefreshedEndpoint, apply_credential_effect, ban_for_lost_source,
        build_secrets, check_report, credential_effect, merge_refresh_report,
        sync_broken_reference,
    };
    use crate::models::{
        AwsRoleSource, CatalogEntrySource, CredentialKind, CredentialSecrets, LimitsSource,
        ListedModel, ModelBan, ModelBanKind, ModelCapabilities, ModelCatalogEntry,
        ModelCredentialEntity, ModelLimits, ModelRefreshReport, ModelRefreshState,
        ModelRefreshStatus, ModelSecret,
    };
    use chrono::{DateTime, Utc};
    use std::collections::BTreeSet;

    /// Build a catalog entry
    ///
    /// # Arguments
    ///
    /// * `model_id` - The entry's model id
    /// * `source` - How the entry got into the catalog
    /// * `limits` - The entry's limits
    fn entry(model_id: &str, source: CatalogEntrySource, limits: ModelLimits) -> ModelCatalogEntry {
        ModelCatalogEntry {
            model_id: model_id.to_owned(),
            limits,
            capabilities: ModelCapabilities::default(),
            bans: HashMap::new(),
            source,
        }
    }

    /// Build a listed model with optional limits
    ///
    /// # Arguments
    ///
    /// * `model_id` - The model's id
    /// * `context_window` - The reported max input tokens
    /// * `max_output_tokens` - The reported max output tokens
    fn listed(
        model_id: &str,
        context_window: Option<u64>,
        max_output_tokens: Option<u64>,
    ) -> ListedModel {
        ListedModel {
            model_id: model_id.to_owned(),
            context_window,
            max_output_tokens,
            capabilities: None,
        }
    }

    /// Build a successful report
    ///
    /// # Arguments
    ///
    /// * `finished` - When the listing finished
    /// * `models` - The listed models
    fn listing(finished: DateTime<Utc>, models: Vec<ListedModel>) -> ModelRefreshReport {
        ModelRefreshReport {
            finished,
            models,
            failure: None,
            credential: None,
        }
    }

    /// Build a failed report
    ///
    /// # Arguments
    ///
    /// * `failure` - Why the listing failed
    fn failed(failure: ModelBanKind) -> ModelRefreshReport {
        ModelRefreshReport {
            finished: Utc::now(),
            models: Vec::new(),
            failure: Some(failure),
            credential: None,
        }
    }

    /// Count the bans of one kind in a ban list
    ///
    /// # Arguments
    ///
    /// * `bans` - The ban list
    /// * `name` - The name of the ban kind
    fn count(bans: &HashMap<Uuid, ModelBan>, name: &str) -> usize {
        bans.values()
            .filter(|ban| ban.ban_kind.name() == name)
            .count()
    }

    /// An endpoint's mergeable state
    #[derive(Default)]
    struct Endpoint {
        /// The catalog
        catalog: Vec<ModelCatalogEntry>,
        /// The endpoint's own bans
        bans: HashMap<Uuid, ModelBan>,
        /// The refresh state
        refresh: ModelRefreshStatus,
        /// The disabled model ids, which a merge never sees
        disabled: BTreeSet<String>,
    }

    impl Endpoint {
        /// Merge a report into this endpoint
        ///
        /// # Arguments
        ///
        /// * `report` - The report to merge
        fn merge(&mut self, report: &ModelRefreshReport) {
            merge_refresh_report(
                &mut RefreshedEndpoint {
                    catalog: &mut self.catalog,
                    bans: &mut self.bans,
                    refresh: &mut self.refresh,
                },
                report,
            );
        }

        /// Get a catalog entry
        ///
        /// # Arguments
        ///
        /// * `model_id` - The entry's model id
        fn entry(&self, model_id: &str) -> &ModelCatalogEntry {
            self.catalog
                .iter()
                .find(|entry| entry.model_id == model_id)
                .expect("the catalog holds the model")
        }
    }

    /// A report listing more models than any provider serves is refused
    #[test]
    fn refresh_report_model_cap() {
        // ten thousand models are accepted and one more is refused
        let models = (0..10_000)
            .map(|index| listed(&format!("model-{index}"), None, None))
            .collect::<Vec<ListedModel>>();
        let mut report = listing(Utc::now(), models);
        assert!(check_report(&report).is_ok());
        report.models.push(listed("model-extra", None, None));
        assert!(check_report(&report).is_err());
    }

    /// A refresh adds new models as refreshed entries in order, updates only refreshed limits,
    /// bans a refreshed model that disappears once, and clears only that ban when it reappears
    #[test]
    #[allow(clippy::too_many_lines)]
    fn refresh_merge_catalog() {
        // a manual entry with manual limits and a refreshed entry with refreshed limits
        let seen = Utc::now() - chrono::Duration::days(1);
        let manual_limits = ModelLimits {
            context_window: Some(1_000),
            max_output_tokens: Some(100),
            source: LimitsSource::Manual,
        };
        let old_limits = ModelLimits {
            context_window: Some(50),
            max_output_tokens: None,
            source: LimitsSource::Refreshed { at: seen },
        };
        let mut endpoint = Endpoint {
            catalog: vec![
                entry(
                    "b-manual",
                    CatalogEntrySource::Manual,
                    manual_limits.clone(),
                ),
                entry(
                    "d-old",
                    CatalogEntrySource::Refreshed { first_seen: seen },
                    old_limits,
                ),
            ],
            disabled: BTreeSet::from(["a-new".to_owned()]),
            ..Endpoint::default()
        };
        // a listing names a new model twice, the old one with new limits, and the manual one
        let now = Utc::now();
        endpoint.merge(&listing(
            now,
            vec![
                listed("a-new", Some(200_000), Some(8_192)),
                listed("d-old", Some(64_000), Some(4_096)),
                listed("b-manual", Some(9), Some(9)),
                listed("a-new", Some(1), Some(1)),
            ],
        ));
        // the new model is added once, sorted first, as a refreshed entry with its limits
        let ids = endpoint
            .catalog
            .iter()
            .map(|entry| entry.model_id.as_str())
            .collect::<Vec<&str>>();
        assert_eq!(ids, vec!["a-new", "b-manual", "d-old"]);
        let added = endpoint.entry("a-new");
        assert_eq!(
            added.source,
            CatalogEntrySource::Refreshed { first_seen: now }
        );
        assert_eq!(added.limits.context_window, Some(200_000));
        assert_eq!(added.limits.max_output_tokens, Some(8_192));
        assert_eq!(added.limits.source, LimitsSource::Refreshed { at: now });
        // refreshed limits follow the listing and manual limits are kept
        assert_eq!(endpoint.entry("d-old").limits.context_window, Some(64_000));
        assert_eq!(endpoint.entry("b-manual").limits, manual_limits);
        // a refresh never changes the disabled ids
        assert!(endpoint.disabled.contains("a-new"));
        assert_eq!(endpoint.refresh.state, ModelRefreshState::Succeeded);
        assert_eq!(endpoint.refresh.finished, Some(now));
        assert_eq!(endpoint.refresh.error, None);
        // a refreshed entry takes the capabilities its listing reports, a manual entry keeps its
        // own, and a model listed without any keeps the defaults
        let caps = ModelCapabilities {
            tool_calling: true,
            vision: true,
            ..ModelCapabilities::default()
        };
        let with_caps = |model_id: &str| ListedModel {
            capabilities: Some(caps.clone()),
            ..listed(model_id, None, None)
        };
        endpoint.merge(&listing(
            Utc::now(),
            vec![
                listed("a-new", None, None),
                with_caps("b-manual"),
                with_caps("d-old"),
            ],
        ));
        assert_eq!(endpoint.entry("d-old").capabilities, caps);
        assert_eq!(
            endpoint.entry("b-manual").capabilities,
            ModelCapabilities::default()
        );
        assert_eq!(
            endpoint.entry("a-new").capabilities,
            ModelCapabilities::default()
        );
        // a hand ban on the old model stays through every refresh
        let generic = ModelBan::generic("by hand");
        endpoint
            .catalog
            .iter_mut()
            .find(|entry| entry.model_id == "d-old")
            .unwrap()
            .bans
            .insert(generic.id, generic.clone());
        // two listings without the refreshed and manual models ban only the refreshed one, once
        for _ in 0..2 {
            endpoint.merge(&listing(Utc::now(), vec![listed("a-new", None, None)]));
        }
        assert_eq!(endpoint.catalog.len(), 3);
        assert_eq!(count(&endpoint.entry("d-old").bans, "ModelRemoved"), 1);
        assert!(endpoint.entry("b-manual").bans.is_empty());
        // listing it again clears only the removal ban
        endpoint.merge(&listing(
            Utc::now(),
            vec![listed("a-new", None, None), listed("d-old", None, None)],
        ));
        let back = endpoint.entry("d-old");
        assert_eq!(count(&back.bans, "ModelRemoved"), 0);
        assert!(back.bans.contains_key(&generic.id));
    }

    /// A failed refresh bans the endpoint once per kind and leaves the catalog alone, and a
    /// successful one clears exactly the refresh failure kinds
    #[test]
    fn refresh_merge_failures() {
        // a refreshed model and bans a refresh never clears
        let seen = Utc::now();
        let mut endpoint = Endpoint {
            catalog: vec![entry(
                "model",
                CatalogEntrySource::Refreshed { first_seen: seen },
                ModelLimits::default(),
            )],
            ..Endpoint::default()
        };
        for kind in [
            ModelBanKind::Generic("by hand".to_owned()),
            ModelBanKind::BrokenReference(Uuid::new_v4()),
            ModelBanKind::NotAllowedByDeployment,
        ] {
            let ban = ModelBan::new(kind);
            endpoint.bans.insert(ban.id, ban);
        }
        let catalog = endpoint.catalog.clone();
        // two unreachable reports ban the endpoint once and keep the catalog
        for _ in 0..2 {
            endpoint.merge(&failed(ModelBanKind::EndpointUnreachable(
                "connection refused".to_owned(),
            )));
        }
        assert_eq!(count(&endpoint.bans, "EndpointUnreachable"), 1);
        assert_eq!(endpoint.catalog, catalog);
        assert_eq!(endpoint.refresh.state, ModelRefreshState::Failed);
        assert!(endpoint.refresh.finished.is_some());
        assert!(
            endpoint
                .refresh
                .error
                .as_deref()
                .is_some_and(|error| error.contains("EndpointUnreachable")),
        );
        // a credential failure leaves the endpoint's bans alone
        endpoint.merge(&failed(ModelBanKind::CredentialExpired));
        assert_eq!(endpoint.bans.len(), 4);
        // a successful report clears the unreachable ban and nothing else
        endpoint.merge(&listing(Utc::now(), vec![listed("model", None, None)]));
        assert_eq!(count(&endpoint.bans, "EndpointUnreachable"), 0);
        assert_eq!(endpoint.bans.len(), 3);
        assert_eq!(endpoint.refresh.state, ModelRefreshState::Succeeded);
        assert_eq!(endpoint.refresh.error, None);
    }

    /// A refresh's credential failures land on the credential once per kind and a successful
    /// refresh clears exactly those
    #[test]
    fn refresh_credential_effects() {
        // a credential with a hand ban
        let mut bans = HashMap::new();
        let generic = ModelBan::generic("by hand");
        bans.insert(generic.id, generic.clone());
        // a rejection is added once
        let rejected = failed(ModelBanKind::CredentialRejected {
            status: 401,
            message: "invalid x-api-key".to_owned(),
        });
        for _ in 0..2 {
            apply_credential_effect(&mut bans, &credential_effect(&rejected));
        }
        assert_eq!(count(&bans, "CredentialRejected"), 1);
        // an expiry is its own kind
        apply_credential_effect(
            &mut bans,
            &credential_effect(&failed(ModelBanKind::CredentialExpired)),
        );
        assert_eq!(count(&bans, "CredentialExpired"), 1);
        // an unreachable endpoint does nothing to the credential
        let unreachable = failed(ModelBanKind::EndpointUnreachable("timeout".to_owned()));
        assert!(matches!(
            credential_effect(&unreachable),
            CredentialEffect::Nothing
        ));
        // a successful listing clears both and keeps the hand ban
        let effect = credential_effect(&listing(Utc::now(), Vec::new()));
        assert!(matches!(effect, CredentialEffect::Clear));
        apply_credential_effect(&mut bans, &effect);
        assert_eq!(bans.len(), 1);
        assert!(bans.contains_key(&generic.id));
    }

    /// Only the three refresh failure kinds and non-empty model ids are accepted in a report
    #[test]
    fn refresh_report_checks() {
        // the refresh failure kinds are accepted
        for kind in [
            ModelBanKind::EndpointUnreachable("down".to_owned()),
            ModelBanKind::CredentialExpired,
            ModelBanKind::CredentialRejected {
                status: 403,
                message: "denied".to_owned(),
            },
        ] {
            assert!(check_report(&failed(kind)).is_ok());
        }
        // every other kind is refused
        for kind in [
            ModelBanKind::Generic("no".to_owned()),
            ModelBanKind::ModelRemoved,
            ModelBanKind::BrokenReference(Uuid::new_v4()),
            ModelBanKind::NotAllowedByDeployment,
        ] {
            assert!(check_report(&failed(kind)).is_err());
        }
        // an empty model id is refused
        assert!(check_report(&listing(Utc::now(), vec![listed(" ", None, None)])).is_err());
        assert!(check_report(&listing(Utc::now(), Vec::new())).is_ok());
    }

    /// A broken reference is banned once and the ban clears when it resolves or moves
    #[test]
    fn broken_reference_bans() {
        // a broken reference adds one ban
        let target = Uuid::new_v4();
        let mut bans = HashMap::new();
        let generic = ModelBan::generic("by hand");
        bans.insert(generic.id, generic.clone());
        sync_broken_reference(&mut bans, Some(target), true);
        sync_broken_reference(&mut bans, Some(target), true);
        assert_eq!(bans.len(), 2);
        assert!(
            bans.values()
                .any(|ban| ban.ban_kind == ModelBanKind::BrokenReference(target))
        );
        // pointing somewhere else clears it and keeps the generic ban
        sync_broken_reference(&mut bans, Some(Uuid::new_v4()), false);
        assert_eq!(bans.len(), 1);
        assert!(bans.contains_key(&generic.id));
        // a reference that resolves again clears its ban
        sync_broken_reference(&mut bans, Some(target), true);
        sync_broken_reference(&mut bans, Some(target), false);
        assert_eq!(bans.len(), 1);
    }

    /// Empty secrets count as missing and only access keys take session tokens
    #[test]
    fn secrets_for_kinds() {
        // an empty secret is refused for a kind that needs one
        let empty = build_secrets(&CredentialKind::ApiKey, Some(ModelSecret::from("")), None);
        assert!(empty.is_err());
        // a set secret is fingerprinted
        let secrets = build_secrets(
            &CredentialKind::ApiKey,
            Some(ModelSecret::from("sk-test")),
            None,
        )
        .unwrap();
        assert_eq!(
            secrets.fingerprint,
            Some(ModelSecret::from("sk-test").fingerprint())
        );
        // a session token on an API key is refused
        let token = build_secrets(
            &CredentialKind::ApiKey,
            Some(ModelSecret::from("sk-test")),
            Some(ModelSecret::from("tok")),
        );
        assert!(token.is_err());
    }

    /// Deleting an access key bans only the roles it's the source of, and leaves a role that
    /// assumes from another key alone
    #[test]
    fn lost_source_bans() {
        // build roles assuming from one access key or another
        let (source, other) = (Uuid::new_v4(), Uuid::new_v4());
        let role = |from: Uuid| ModelCredentialEntity {
            kind: CredentialKind::AwsAssumeRole {
                role_arn: "arn:aws:iam::123456789012:role/test".to_owned(),
                external_id: None,
                session_name: None,
                duration_seconds: None,
                source: AwsRoleSource::AccessKey(from),
            },
            secrets: CredentialSecrets::default(),
            expires: None,
            rotated: None,
            bans: HashMap::new(),
            modified: chrono::Utc::now(),
        };
        // deleting its source bans it once
        let mut using = role(source);
        assert!(ban_for_lost_source(&mut using, source));
        assert!(ban_for_lost_source(&mut using, source));
        assert_eq!(using.bans.len(), 1);
        assert!(
            using
                .bans
                .values()
                .any(|ban| ban.ban_kind == ModelBanKind::BrokenReference(source))
        );
        // deleting another key leaves a role and its bans alone
        let mut moved = role(other);
        let broken = ModelBan::new(ModelBanKind::BrokenReference(other));
        moved.bans.insert(broken.id, broken.clone());
        let modified = moved.modified;
        assert!(!ban_for_lost_source(&mut moved, source));
        assert_eq!(moved.bans.len(), 1);
        assert!(moved.bans.contains_key(&broken.id));
        assert_eq!(moved.modified, modified);
    }
}
