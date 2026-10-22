//! Resolving the model each agent of a workflow run uses, and delivering a run's models
//!
//! Resolution runs once, when a workflow run is created: for each agent it walks the agent's
//! models in priority order and the auth levels in precedence order, and picks the first model
//! with a usable endpoint and credential. The resolver itself works on configuration already
//! read into memory, so every candidate is checked without another read. The admin run models
//! read turns a run's recorded bindings into the models and credentials its harness gets.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use tracing::instrument;
use uuid::Uuid;

use super::super::db;
use crate::models::backends::entities::models::list_ids;
use crate::models::{
    AgentModelMismatch, AuthedUser, CredentialKind, CredentialType, Entity, EntityKinds,
    EntityMetadata, HarnessCapabilities, HarnessCredential, HarnessEnv, HarnessModelEntry,
    HarnessModels, HarnessSpec, InstancedAgent, MismatchNames, ModelApi, ModelAuthLevel,
    ModelAuthSource, ModelBan, ModelBanKind, ModelCandidateRejection, ModelCapabilities,
    ModelCatalogEntry, ModelConfigRef, ModelCredentialEntity, ModelEndpointEntity, ModelLimits,
    ModelMatcher, ModelProfileEntity, ModelProvider, ModelRejectReason, ModelRequirements,
    ModelResolutionError, ModelsConf, Reaction, ReactionModelBinding, ReactionSource, RunModels,
    User, UserCredentialRef, UserModelSettings, WorkflowInstanceEntity,
};
use crate::utils::{ApiError, Shared};
use crate::{conflict, is_admin, not_found, unauthorized};

#[cfg(test)]
mod tests;

/// Every auth level in precedence order
const LEVELS: [ModelAuthLevel; 4] = [
    ModelAuthLevel::Harness,
    ModelAuthLevel::Workflow,
    ModelAuthLevel::Group,
    ModelAuthLevel::User,
];

/// A group model endpoint in the run group, as the resolver reads it
#[derive(Debug, Clone)]
pub struct EndpointRecord {
    /// The endpoint's id
    pub id: Uuid,
    /// The endpoint's name
    pub name: String,
    /// The endpoint
    pub meta: ModelEndpointEntity,
}

/// A group model profile in the run group, as the resolver reads it
#[derive(Debug, Clone)]
pub struct ProfileRecord {
    /// The profile's id
    pub id: Uuid,
    /// The profile's name
    pub name: String,
    /// The profile
    pub meta: ModelProfileEntity,
}

/// Everything resolving a run's models reads, already in memory
pub struct ResolveInputs<'a> {
    /// The agents of the workflow instance, in agent order
    pub agents: &'a [InstancedAgent],
    /// The workflow's default models, for agents whose own list is empty
    pub default_models: &'a [ModelMatcher],
    /// The live auth source of each agent, by agent id (a missing agent allows every level)
    pub agent_auth: &'a HashMap<Uuid, ModelAuthSource>,
    /// The live auth source of the workflow
    pub workflow_auth: &'a ModelAuthSource,
    /// The run group's model endpoints
    pub endpoints: &'a [EndpointRecord],
    /// The run group's model profiles
    pub profiles: &'a [ProfileRecord],
    /// The group credentials the endpoints authenticate with and the access keys assumed roles
    /// use, by id
    pub credentials: &'a HashMap<Uuid, ModelCredentialEntity>,
    /// The runner's own model settings
    pub user: &'a UserModelSettings,
    /// The deployment's allowed endpoint list
    pub whitelist: &'a ModelsConf,
    /// What the entry agent's harness can call
    pub harness: &'a HarnessCapabilities,
    /// The model ids the entry agent's harness never resolves to
    pub harness_disabled: &'a BTreeSet<String>,
}

/// An endpoint a candidate may be called on, at one level
#[derive(Clone, Copy)]
struct EndpointView<'a> {
    /// How the binding refers to this endpoint
    reference: ModelConfigRef,
    /// The endpoint's id
    id: Uuid,
    /// The endpoint's name
    name: &'a str,
    /// The endpoint's provider
    provider: &'a ModelProvider,
    /// The models the endpoint serves, sorted by model id
    catalog: &'a [ModelCatalogEntry],
    /// The endpoint's bans
    bans: &'a HashMap<Uuid, ModelBan>,
    /// The model ids the endpoint never resolves to
    disabled: &'a BTreeSet<String>,
    /// The group credential of a group endpoint
    group_credential: Option<Uuid>,
    /// Whether this is a group endpoint (otherwise one of the runner's own)
    group: bool,
}

impl<'a> EndpointView<'a> {
    /// View a group endpoint
    ///
    /// # Arguments
    ///
    /// * `record` - The group endpoint
    fn group(record: &'a EndpointRecord) -> Self {
        EndpointView {
            reference: ModelConfigRef::Group(record.id),
            id: record.id,
            name: &record.name,
            provider: &record.meta.provider,
            catalog: &record.meta.catalog,
            bans: &record.meta.bans,
            disabled: &record.meta.disabled_models,
            group_credential: record.meta.credential,
            group: true,
        }
    }

    /// View one of the runner's own endpoints
    ///
    /// # Arguments
    ///
    /// * `id` - The endpoint's id
    /// * `endpoint` - The runner's endpoint
    fn own(id: Uuid, endpoint: &'a crate::models::UserModelEndpoint) -> Self {
        EndpointView {
            reference: ModelConfigRef::User(id),
            id,
            name: &endpoint.name,
            provider: &endpoint.provider,
            catalog: &endpoint.catalog,
            bans: &endpoint.bans,
            disabled: &endpoint.disabled_models,
            group_credential: None,
            group: false,
        }
    }

    /// Get the catalog entry for a model id
    ///
    /// # Arguments
    ///
    /// * `model_id` - The exact model id
    fn entry(&self, model_id: &str) -> Option<&'a ModelCatalogEntry> {
        // catalogs are sorted by model id
        self.catalog
            .binary_search_by(|entry| entry.model_id.as_str().cmp(model_id))
            .ok()
            .map(|index| &self.catalog[index])
    }

    /// Get the API version this endpoint is configured with, for the providers that have one
    fn api_version(&self) -> Option<String> {
        // only some providers carry a version
        match self.provider {
            ModelProvider::Anthropic { version, .. } => Some(version.clone()),
            ModelProvider::AzureOpenAi {
                api_mode: crate::models::AzureApiMode::Legacy { api_version },
                ..
            } => Some(api_version.clone()),
            ModelProvider::Gemini { api_version, .. } => serde_json::to_value(api_version)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned)),
            _ => None,
        }
    }

    /// Whether a matcher's provider and API versions allow this endpoint
    ///
    /// # Arguments
    ///
    /// * `matcher` - The matcher being tried
    fn allowed_by(&self, matcher: &ModelMatcher) -> bool {
        // the provider must match when the matcher names one
        if matcher
            .provider
            .is_some_and(|provider| provider != self.provider.kind())
        {
            return false;
        }
        // the endpoint's version must be accepted when the matcher lists versions
        if !matcher.api_versions.is_empty() {
            return self
                .api_version()
                .is_some_and(|version| matcher.api_versions.contains(&version));
        }
        true
    }
}

/// One way to call a model on an endpoint: through a profile or straight from the catalog
#[derive(Clone)]
struct ModelOption<'a> {
    /// The model id
    model_id: &'a str,
    /// The profile used, if any
    profile: Option<&'a ProfileRecord>,
    /// The catalog entry of the model, if the catalog lists it
    entry: Option<&'a ModelCatalogEntry>,
    /// The protocol the model is called with
    api: ModelApi,
    /// The model's effective limits
    limits: ModelLimits,
    /// The model's effective capabilities
    capabilities: ModelCapabilities,
}

/// A credential a candidate authenticates with, as the resolver checks it
struct CredentialView<'a> {
    /// How the binding refers to this credential
    reference: ModelConfigRef,
    /// What kind of credential this is
    kind: &'a CredentialKind,
    /// The credential's bans, and those of the access key an assumed role uses
    bans: Vec<&'a ModelBan>,
    /// The missing access key an assumed role names, if it's missing
    broken: Option<Uuid>,
}

/// How a candidate authenticates: with a credential or keyless
enum Auth<'a> {
    /// With a credential
    Credential(CredentialView<'a>),
    /// Without any credential (a keyless gateway)
    Keyless,
}

/// The candidate being tried and where its rejections are recorded
struct Attempt<'r> {
    /// The matcher being tried, if the agent's effective list isn't empty
    matcher: Option<&'r ModelMatcher>,
    /// The position of the matcher in the agent's effective list
    priority: Option<u32>,
    /// The rejections recorded so far, in the order tried
    rejections: &'r mut Vec<ModelCandidateRejection>,
}

impl Attempt<'_> {
    /// Record why a candidate couldn't be used
    ///
    /// # Arguments
    ///
    /// * `level` - The level being tried
    /// * `endpoint` - The endpoint being tried
    /// * `reason` - Why the candidate couldn't be used
    fn reject(
        &mut self,
        level: Option<ModelAuthLevel>,
        endpoint: Option<ModelConfigRef>,
        reason: ModelRejectReason,
    ) {
        // keep the rejection in the order tried
        self.rejections.push(ModelCandidateRejection {
            matcher: self.matcher.cloned(),
            level,
            endpoint,
            reason,
        });
    }
}

/// Get the levels an agent may take auth from: those both its and the workflow's auth source
/// allow (an empty source allows every level)
///
/// # Arguments
///
/// * `agent` - The agent's auth source
/// * `workflow` - The workflow's auth source
fn allowed_levels(agent: &ModelAuthSource, workflow: &ModelAuthSource) -> BTreeSet<ModelAuthLevel> {
    // an empty source allows every level
    let allows = |source: &ModelAuthSource, level: &ModelAuthLevel| {
        source.levels.is_empty() || source.levels.contains(level)
    };
    LEVELS
        .iter()
        .filter(|level| allows(agent, level) && allows(workflow, level))
        .copied()
        .collect()
}

/// Get the ban a set of bans holds, the oldest first, if any
///
/// # Arguments
///
/// * `bans` - The bans to check
fn first_ban<'b>(bans: impl Iterator<Item = &'b ModelBan>) -> Option<&'b ModelBan> {
    // the oldest ban is reported so the reason is stable
    bans.min_by_key(|ban| (ban.time_banned, ban.id))
}

/// Check a model's effective limits and capabilities against an agent's requirements
///
/// Returns the first requirement the model misses.
///
/// # Arguments
///
/// * `requirements` - The agent's requirements
/// * `limits` - The model's effective limits
/// * `capabilities` - The model's effective capabilities
fn missed_requirement(
    requirements: &ModelRequirements,
    limits: &ModelLimits,
    capabilities: &ModelCapabilities,
) -> Option<String> {
    // a minimum context window needs a known, large enough context window
    if let Some(min) = requirements.min_context_window {
        match limits.context_window {
            None => {
                return Some(format!(
                    "min_context_window {min}: the model's context window isn't known; set context_window on its catalog entry or profile"
                ));
            }
            Some(window) if window < min => {
                return Some(format!(
                    "min_context_window {min}: the model's context window is {window}"
                ));
            }
            Some(_) => (),
        }
    }
    // a minimum output needs a known, large enough max output
    if let Some(min) = requirements.min_output_tokens {
        match limits.max_output_tokens {
            None => {
                return Some(format!(
                    "min_output_tokens {min}: the model's max_output_tokens isn't known; set max_output_tokens on its catalog entry or profile"
                ));
            }
            Some(most) if most < min => {
                return Some(format!(
                    "min_output_tokens {min}: the model's max_output_tokens is {most}"
                ));
            }
            Some(_) => (),
        }
    }
    // every required capability must be present
    let required = &requirements.capabilities;
    let checks = [
        (
            "tool_calling",
            required.tool_calling,
            capabilities.tool_calling,
        ),
        ("vision", required.vision, capabilities.vision),
        ("pdf_input", required.pdf_input, capabilities.pdf_input),
        (
            "structured_output",
            required.structured_output,
            capabilities.structured_output,
        ),
        ("thinking", required.thinking, capabilities.thinking),
        (
            "strict_tools",
            required.strict_tools,
            capabilities.strict_tools,
        ),
        (
            "parallel_tool_calls",
            required.parallel_tool_calls,
            capabilities.parallel_tool_calls,
        ),
        (
            "forced_tool_choice",
            required.forced_tool_choice,
            capabilities.forced_tool_choice,
        ),
    ];
    checks
        .iter()
        .find(|(_, needed, has)| *needed && !*has)
        .map(|(name, _, _)| format!("capabilities.{name}: the model lacks {name}"))
}

/// The resolver for one run's agents
struct Resolver<'a> {
    /// Everything resolution reads
    inputs: &'a ResolveInputs<'a>,
}

impl<'a> Resolver<'a> {
    /// Get the endpoints a level offers, in name order
    ///
    /// The group level offers the run group's endpoints, and the user level those endpoints
    /// plus the runner's own.
    ///
    /// # Arguments
    ///
    /// * `level` - The level
    fn endpoints(&self, level: ModelAuthLevel) -> Vec<EndpointView<'a>> {
        // the group endpoints are offered at both levels
        let mut views = self
            .inputs
            .endpoints
            .iter()
            .map(EndpointView::group)
            .collect::<Vec<EndpointView>>();
        // the user level also offers the runner's own endpoints
        if level == ModelAuthLevel::User {
            views.extend(
                self.inputs
                    .user
                    .endpoints
                    .iter()
                    .map(|(id, endpoint)| EndpointView::own(*id, endpoint)),
            );
        }
        // endpoints are tried by name
        views.sort_by(|a, b| a.name.cmp(b.name).then(a.id.cmp(&b.id)));
        views
    }

    /// Get the profiles on an endpoint for a model id, by name then id
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint
    /// * `model_id` - The model id
    fn profiles(&self, endpoint: &EndpointView, model_id: &str) -> Vec<&'a ProfileRecord> {
        // only group endpoints have profiles
        if !endpoint.group {
            return Vec::new();
        }
        let mut profiles = self
            .inputs
            .profiles
            .iter()
            .filter(|profile| {
                profile.meta.endpoint == endpoint.id && profile.meta.model_id == model_id
            })
            .collect::<Vec<&ProfileRecord>>();
        // the profile whose name sorts first wins, then the lower id
        profiles.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        profiles
    }

    /// Build the option of calling a model through a profile
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The profile's endpoint
    /// * `profile` - The profile
    fn profile_option(endpoint: &EndpointView<'a>, profile: &'a ProfileRecord) -> ModelOption<'a> {
        // the profile's limits and capabilities replace the catalog entry's
        let entry = endpoint.entry(&profile.meta.model_id);
        let limits = profile
            .meta
            .limits
            .clone()
            .or_else(|| entry.map(|entry| entry.limits.clone()))
            .unwrap_or_default();
        let capabilities = profile
            .meta
            .capabilities
            .clone()
            .or_else(|| entry.map(|entry| entry.capabilities.clone()))
            .unwrap_or_default();
        ModelOption {
            model_id: &profile.meta.model_id,
            profile: Some(profile),
            entry,
            api: profile.meta.api,
            limits,
            capabilities,
        }
    }

    /// Build the option of calling a catalog model with no profile
    ///
    /// # Arguments
    ///
    /// * `entry` - The catalog entry
    /// * `api` - The protocol to call it with
    fn catalog_option(entry: &'a ModelCatalogEntry, api: ModelApi) -> ModelOption<'a> {
        ModelOption {
            model_id: &entry.model_id,
            profile: None,
            entry: Some(entry),
            api,
            limits: entry.limits.clone(),
            capabilities: entry.capabilities.clone(),
        }
    }

    /// Get the ways an endpoint can serve a matcher: its profiles for the model, then its catalog
    /// entry
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint
    /// * `matcher` - The matcher being tried
    fn options(&self, endpoint: &EndpointView<'a>, matcher: &ModelMatcher) -> Vec<ModelOption<'a>> {
        // the provider and version must be ones the matcher accepts
        if !endpoint.allowed_by(matcher) {
            return Vec::new();
        }
        // each profile for the model whose protocol the matcher accepts and the provider serves,
        // since a provider change can leave a profile on a protocol the new provider doesn't serve
        let mut options = self
            .profiles(endpoint, &matcher.model_id)
            .into_iter()
            .map(|profile| Self::profile_option(endpoint, profile))
            .filter(|option| matcher.api.is_none_or(|api| api == option.api))
            .filter(|option| endpoint.provider.apis().contains(&option.api))
            .collect::<Vec<ModelOption>>();
        // then the catalog entry with the matcher's protocol or the provider's default
        if let Some(entry) = endpoint.entry(&matcher.model_id) {
            let api = matcher
                .api
                .unwrap_or_else(|| endpoint.provider.default_api());
            if endpoint.provider.apis().contains(&api) {
                options.push(Self::catalog_option(entry, api));
            }
        }
        options
    }

    /// Get the auth an endpoint offers at a level
    ///
    /// Returns `None` when the endpoint needs a credential and has no usable one at this level.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint
    /// * `level` - The level
    fn auth(&self, endpoint: &EndpointView<'a>, level: ModelAuthLevel) -> Option<Auth<'a>> {
        match level {
            // a group endpoint authenticates with its group credential, or keyless
            ModelAuthLevel::Group => match endpoint.group_credential {
                Some(id) => self.group_credential(id).map(Auth::Credential),
                None if endpoint.provider.allows_keyless() => Some(Auth::Keyless),
                None => None,
            },
            // the runner's own credential for the endpoint, or keyless for their own gateway
            ModelAuthLevel::User => match self.inputs.user.credentials.get(&endpoint.id) {
                Some(credential) => Some(Auth::Credential(self.user_credential(credential))),
                None if !endpoint.group && endpoint.provider.allows_keyless() => {
                    Some(Auth::Keyless)
                }
                None => None,
            },
            // the reserved levels offer nothing
            ModelAuthLevel::Harness | ModelAuthLevel::Workflow => None,
        }
    }

    /// View a group credential with the bans of the access key it assumes a role with
    ///
    /// # Arguments
    ///
    /// * `id` - The credential's id
    fn group_credential(&self, id: Uuid) -> Option<CredentialView<'a>> {
        // a missing credential isn't usable
        let credential = self.inputs.credentials.get(&id)?;
        let mut bans = credential.bans.values().collect::<Vec<&ModelBan>>();
        let mut broken = None;
        // an assumed role also needs its access key
        if let Some(source) = credential.kind.role_source() {
            match self.inputs.credentials.get(&source) {
                Some(key) => bans.extend(key.bans.values()),
                None => broken = Some(source),
            }
        }
        Some(CredentialView {
            reference: ModelConfigRef::Group(id),
            kind: &credential.kind,
            bans,
            broken,
        })
    }

    /// View one of the runner's credentials with the bans of the access key it assumes a role
    /// with
    ///
    /// # Arguments
    ///
    /// * `credential` - The runner's credential
    fn user_credential(&self, credential: &'a UserCredentialRef) -> CredentialView<'a> {
        let mut bans = credential.bans.values().collect::<Vec<&ModelBan>>();
        let mut broken = None;
        // an assumed role also needs the runner's access key it names
        if let Some(source) = credential.kind.role_source() {
            match self
                .inputs
                .user
                .credentials
                .values()
                .find(|other| other.id == source)
            {
                Some(key) => bans.extend(key.bans.values()),
                None => broken = Some(source),
            }
        }
        CredentialView {
            reference: ModelConfigRef::User(credential.id),
            kind: &credential.kind,
            bans,
            broken,
        }
    }

    /// Check one option of one endpoint at one level, picking it or recording why not
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent being resolved
    /// * `attempt` - The candidate being tried
    /// * `level` - The level being tried
    /// * `endpoint` - The endpoint being tried
    /// * `option` - The way the model is called
    #[allow(clippy::too_many_lines)]
    fn check(
        &self,
        agent: &InstancedAgent,
        attempt: &mut Attempt,
        level: ModelAuthLevel,
        endpoint: &EndpointView<'a>,
        option: &ModelOption<'a>,
    ) -> Option<ReactionModelBinding> {
        // the endpoint needs a usable credential at this level, or must be keyless
        let Some(auth) = self.auth(endpoint, level) else {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::NoCredential,
            );
            return None;
        };
        // nothing the candidate uses may be banned
        let credential = match &auth {
            Auth::Credential(credential) => Some(credential),
            Auth::Keyless => None,
        };
        if let Some(missing) = credential.and_then(|credential| credential.broken) {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::Banned(ModelBanKind::BrokenReference(missing)),
            );
            return None;
        }
        let bans = endpoint
            .bans
            .values()
            .chain(
                option
                    .entry
                    .into_iter()
                    .flat_map(|entry| entry.bans.values()),
            )
            .chain(
                option
                    .profile
                    .into_iter()
                    .flat_map(|profile| profile.meta.bans.values()),
            )
            .chain(
                credential
                    .into_iter()
                    .flat_map(|credential| credential.bans.iter().copied()),
            );
        if let Some(ban) = first_ban(bans) {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::Banned(ban.ban_kind.clone()),
            );
            return None;
        }
        // the endpoint must not disable the model
        if endpoint.disabled.contains(option.model_id) {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::DisabledOnEndpoint,
            );
            return None;
        }
        // the endpoint must still be on the deployment's allowed list
        if !self.inputs.whitelist.allows(endpoint.provider) {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::NotAllowedByDeployment,
            );
            return None;
        }
        // the model must meet the agent's requirements
        if let Some(missed) = missed_requirement(
            &agent.config.model_requirements,
            &option.limits,
            &option.capabilities,
        ) {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::RequirementNotMet(missed),
            );
            return None;
        }
        // the harness must speak the protocol
        if !self.inputs.harness.model_apis.contains(&option.api) {
            attempt.reject(
                Some(level),
                Some(endpoint.reference),
                ModelRejectReason::ApiNotSupported(option.api),
            );
            return None;
        }
        // the harness must be able to use the credential's kind
        if let Some(credential) = credential {
            let kind: CredentialType = credential.kind.cred_type();
            if !self.inputs.harness.credential_kinds.contains(&kind) {
                attempt.reject(
                    Some(level),
                    Some(endpoint.reference),
                    ModelRejectReason::CredentialNotSupported(kind),
                );
                return None;
            }
        }
        // every check passed, so this candidate is chosen
        Some(ReactionModelBinding {
            agent: agent.agent,
            priority: attempt.priority,
            level,
            endpoint: endpoint.reference,
            model_id: option.model_id.to_owned(),
            api: option.api,
            profile: option.profile.map(|profile| profile.id),
            credential: credential.map(|credential| credential.reference),
        })
    }

    /// Try one matcher at every level in precedence order
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent being resolved
    /// * `allowed` - The levels the agent may take auth from
    /// * `attempt` - The candidate being tried and where its rejections are recorded
    /// * `matcher` - The matcher being tried
    fn try_matcher(
        &self,
        agent: &InstancedAgent,
        allowed: &BTreeSet<ModelAuthLevel>,
        attempt: &mut Attempt,
        matcher: &ModelMatcher,
    ) -> Option<ReactionModelBinding> {
        // a model the harness disables is skipped at every level
        if self.inputs.harness_disabled.contains(&matcher.model_id) {
            attempt.reject(None, None, ModelRejectReason::DisabledByHarness);
            return None;
        }
        for level in LEVELS {
            // the level must be allowed by the agent's and workflow's auth sources
            if !allowed.contains(&level) {
                attempt.reject(Some(level), None, ModelRejectReason::LevelNotAllowed);
                continue;
            }
            // the reserved harness and workflow levels have no auth source
            if matches!(level, ModelAuthLevel::Harness | ModelAuthLevel::Workflow) {
                attempt.reject(Some(level), None, ModelRejectReason::NoEndpoint);
                continue;
            }
            // try each endpoint that serves the model at this level, in name order
            let mut served = false;
            for endpoint in self.endpoints(level) {
                for option in self.options(&endpoint, matcher) {
                    served = true;
                    if let Some(binding) = self.check(agent, attempt, level, &endpoint, &option) {
                        return Some(binding);
                    }
                }
            }
            // no endpoint at this level serves the model
            if !served {
                attempt.reject(Some(level), None, ModelRejectReason::NoEndpoint);
            }
        }
        None
    }

    /// Try the runner's default profiles in order, for an agent whose effective list is empty
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent being resolved
    /// * `allowed` - The levels the agent may take auth from
    /// * `attempt` - Where rejections are recorded
    fn try_default_profiles(
        &self,
        agent: &InstancedAgent,
        allowed: &BTreeSet<ModelAuthLevel>,
        attempt: &mut Attempt,
    ) -> Option<ReactionModelBinding> {
        for profile_id in &self.inputs.user.default_profiles {
            // a deleted profile, or one outside the run group, serves nothing
            let Some(profile) = self
                .inputs
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
            else {
                attempt.reject(
                    Some(ModelAuthLevel::Group),
                    None,
                    ModelRejectReason::NoEndpoint,
                );
                continue;
            };
            // its endpoint must be in the run group
            let Some(record) = self
                .inputs
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == profile.meta.endpoint)
            else {
                attempt.reject(
                    Some(ModelAuthLevel::Group),
                    None,
                    ModelRejectReason::NoEndpoint,
                );
                continue;
            };
            // a model the harness disables is skipped
            if self
                .inputs
                .harness_disabled
                .contains(&profile.meta.model_id)
            {
                attempt.reject(None, None, ModelRejectReason::DisabledByHarness);
                continue;
            }
            // a profile on a protocol its endpoint's provider doesn't serve is served by nothing
            let endpoint = EndpointView::group(record);
            if !endpoint.provider.apis().contains(&profile.meta.api) {
                attempt.reject(
                    Some(ModelAuthLevel::Group),
                    Some(endpoint.reference),
                    ModelRejectReason::NoEndpoint,
                );
                continue;
            }
            // try the profile at each level that can authenticate a group endpoint
            let option = Self::profile_option(&endpoint, profile);
            for level in [ModelAuthLevel::Group, ModelAuthLevel::User] {
                if !allowed.contains(&level) {
                    attempt.reject(Some(level), None, ModelRejectReason::LevelNotAllowed);
                    continue;
                }
                if let Some(binding) = self.check(agent, attempt, level, &endpoint, &option) {
                    return Some(binding);
                }
            }
        }
        None
    }

    /// Try every catalog model the allowed levels reach, by level, then endpoint name, then model
    /// id, for an agent whose effective list is empty
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent being resolved
    /// * `allowed` - The levels the agent may take auth from
    /// * `attempt` - Where rejections are recorded
    fn try_catalogs(
        &self,
        agent: &InstancedAgent,
        allowed: &BTreeSet<ModelAuthLevel>,
        attempt: &mut Attempt,
    ) -> Option<ReactionModelBinding> {
        for level in [ModelAuthLevel::Group, ModelAuthLevel::User] {
            // the level must be allowed
            if !allowed.contains(&level) {
                attempt.reject(Some(level), None, ModelRejectReason::LevelNotAllowed);
                continue;
            }
            // try every catalog model of every endpoint at this level
            let endpoints = self.endpoints(level);
            let mut served = false;
            for endpoint in &endpoints {
                for entry in endpoint.catalog {
                    served = true;
                    // a model the harness disables is skipped
                    if self.inputs.harness_disabled.contains(&entry.model_id) {
                        attempt.reject(None, None, ModelRejectReason::DisabledByHarness);
                        continue;
                    }
                    let option = Self::catalog_option(entry, endpoint.provider.default_api());
                    if let Some(binding) = self.check(agent, attempt, level, endpoint, &option) {
                        return Some(binding);
                    }
                }
            }
            // no endpoint at this level serves any model
            if !served {
                attempt.reject(Some(level), None, ModelRejectReason::NoEndpoint);
            }
        }
        None
    }

    /// Resolve one agent, or return every reason it couldn't be
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent to resolve
    fn agent(&self, agent: &InstancedAgent) -> Result<ReactionModelBinding, AgentModelMismatch> {
        // the levels this agent may take auth from
        let default_auth = ModelAuthSource::default();
        let agent_auth = self
            .inputs
            .agent_auth
            .get(&agent.agent)
            .unwrap_or(&default_auth);
        let allowed = allowed_levels(agent_auth, self.inputs.workflow_auth);
        // the agent's own list wins, then the workflow's defaults
        let list = if agent.config.models.is_empty() {
            self.inputs.default_models
        } else {
            &agent.config.models
        };
        let mut rejections = Vec::new();
        if list.is_empty() {
            // an empty list tries the reserved levels, the default profiles, then every catalog
            let mut attempt = Attempt {
                matcher: None,
                priority: None,
                rejections: &mut rejections,
            };
            for level in [ModelAuthLevel::Harness, ModelAuthLevel::Workflow] {
                let reason = if allowed.contains(&level) {
                    ModelRejectReason::NoEndpoint
                } else {
                    ModelRejectReason::LevelNotAllowed
                };
                attempt.reject(Some(level), None, reason);
            }
            if let Some(binding) = self.try_default_profiles(agent, &allowed, &mut attempt) {
                return Ok(binding);
            }
            if let Some(binding) = self.try_catalogs(agent, &allowed, &mut attempt) {
                return Ok(binding);
            }
        } else {
            // each matcher is tried in priority order, at every level
            for (index, matcher) in list.iter().enumerate() {
                let mut attempt = Attempt {
                    matcher: Some(matcher),
                    priority: u32::try_from(index).ok(),
                    rejections: &mut rejections,
                };
                if let Some(binding) = self.try_matcher(agent, &allowed, &mut attempt, matcher) {
                    return Ok(binding);
                }
            }
        }
        Err(AgentModelMismatch {
            agent: agent.agent,
            name: agent.name.clone(),
            candidates: rejections,
        })
    }
}

/// Resolve the model of every agent of a workflow run
///
/// Returns one binding per agent, sorted by agent id, or every reason resolution failed. When the
/// harness can't give agents different models, every agent must resolve to the same endpoint,
/// model, protocol, and credential.
///
/// # Arguments
///
/// * `inputs` - Everything resolution reads
pub fn resolve(
    inputs: &ResolveInputs<'_>,
) -> Result<Vec<ReactionModelBinding>, ModelResolutionError> {
    // resolve each agent in agent order, keeping every failure
    let resolver = Resolver { inputs };
    let mut bindings = Vec::with_capacity(inputs.agents.len());
    let mut mismatches = Vec::new();
    for agent in inputs.agents {
        match resolver.agent(agent) {
            Ok(binding) => bindings.push(binding),
            Err(mismatch) => mismatches.push(mismatch),
        }
    }
    // any unresolved agent fails the run
    if !mismatches.is_empty() {
        return Err(ModelResolutionError { agents: mismatches });
    }
    // a harness without per-agent models needs every agent on the same model
    if !inputs.harness.per_agent_models {
        let same = bindings.windows(2).all(|pair| {
            pair[0].endpoint == pair[1].endpoint
                && pair[0].model_id == pair[1].model_id
                && pair[0].api == pair[1].api
                && pair[0].credential == pair[1].credential
        });
        if !same {
            // name each agent and the model it resolved to
            let agents = inputs
                .agents
                .iter()
                .zip(&bindings)
                .map(|(agent, binding)| AgentModelMismatch {
                    agent: agent.agent,
                    name: agent.name.clone(),
                    candidates: vec![ModelCandidateRejection {
                        matcher: Some(ModelMatcher {
                            model_id: binding.model_id.clone(),
                            api: Some(binding.api),
                            ..ModelMatcher::default()
                        }),
                        level: Some(binding.level),
                        endpoint: Some(binding.endpoint),
                        reason: ModelRejectReason::MixedModelsUnsupported,
                    }],
                })
                .collect();
            return Err(ModelResolutionError { agents });
        }
    }
    // bindings are recorded sorted by agent id
    bindings.sort_by_key(|binding| binding.agent);
    Ok(bindings)
}

/// Read the entities of one kind in a group that the resolver needs, without their tags
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `kind` - The kind of entity to read
/// * `group` - The run group
/// * `shared` - Shared Thorium objects
async fn group_entities(
    user: &User,
    kind: EntityKinds,
    group: &str,
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // list the kind in the run group
    let groups = [group.to_owned()];
    let ids = list_ids(user, kind, &groups, shared)
        .await?
        .into_iter()
        .collect::<Vec<Uuid>>();
    // nothing to read when the group has none
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    db::entities::get_many_untagged(&groups, &ids, shared).await
}

/// Read the group credentials some endpoints authenticate with, and the access keys their
/// assumed roles use, in the run group
///
/// # Arguments
///
/// * `endpoints` - The endpoints
/// * `group` - The run group
/// * `shared` - Shared Thorium objects
async fn group_credentials(
    endpoints: &[EndpointRecord],
    group: &str,
    shared: &Shared,
) -> Result<HashMap<Uuid, ModelCredentialEntity>, ApiError> {
    // read each credential the endpoints name once
    let groups = [group.to_owned()];
    let ids = endpoints
        .iter()
        .filter_map(|endpoint| endpoint.meta.credential)
        .collect::<BTreeSet<Uuid>>()
        .into_iter()
        .collect::<Vec<Uuid>>();
    let mut credentials = read_credentials(&groups, &ids, shared).await?;
    // then the access keys their assumed roles use
    let sources = credentials
        .values()
        .filter_map(|credential| credential.kind.role_source())
        .filter(|source| !credentials.contains_key(source))
        .collect::<BTreeSet<Uuid>>()
        .into_iter()
        .collect::<Vec<Uuid>>();
    credentials.extend(read_credentials(&groups, &sources, shared).await?);
    Ok(credentials)
}

/// Read some model credential entities in a set of groups, without their tags
///
/// # Arguments
///
/// * `groups` - The groups to read them in
/// * `ids` - The credentials' ids
/// * `shared` - Shared Thorium objects
async fn read_credentials(
    groups: &[String],
    ids: &[Uuid],
    shared: &Shared,
) -> Result<HashMap<Uuid, ModelCredentialEntity>, ApiError> {
    // nothing to read for no ids
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    // keep only model credentials
    let entities = db::entities::get_many_untagged(groups, ids, shared).await?;
    Ok(entities
        .into_iter()
        .filter_map(|entity| match entity.metadata {
            EntityMetadata::ModelCredential(credential) => Some((entity.id, credential)),
            _ => None,
        })
        .collect())
}

/// Get the model settings of a user, or empty settings when they have none
///
/// # Arguments
///
/// * `user` - The user
fn model_settings(user: &User) -> UserModelSettings {
    // users without AI settings have no model settings
    user.settings
        .ai
        .as_ref()
        .map_or_default(|ai| ai.models.clone())
}

/// The workflow run whose agents' models are being resolved
pub struct RunModelSources<'a> {
    /// The workflow instance entity
    pub entity: &'a Entity,
    /// The workflow instance
    pub instance: &'a WorkflowInstanceEntity,
    /// The entry agent's harness tool instance entity
    pub harness_entity: &'a Entity,
    /// The entry agent's harness
    pub harness: &'a HarnessSpec,
}

/// Read the live auth source of each agent of an instance and of its workflow
///
/// The agents and the workflow are read in the groups they are in now, not the groups the instance
/// saw them in, so one moved to other groups keeps its restriction. A deleted agent or workflow
/// allows every level.
///
/// # Arguments
///
/// * `instance` - The workflow instance
/// * `shared` - Shared Thorium objects
async fn auth_sources(
    instance: &WorkflowInstanceEntity,
    shared: &Shared,
) -> Result<(HashMap<Uuid, ModelAuthSource>, ModelAuthSource), ApiError> {
    // read every agent and the workflow wherever they are now
    let mut ids = instance
        .agents
        .iter()
        .map(|agent| agent.agent)
        .collect::<Vec<Uuid>>();
    ids.push(instance.workflow);
    // keep each agent's auth source and the workflow's
    let mut agents = HashMap::with_capacity(ids.len());
    let mut workflow = ModelAuthSource::default();
    for entity in read_everywhere(&ids, shared).await? {
        match entity.metadata {
            EntityMetadata::Agent(agent) => {
                agents.insert(entity.id, agent.model_auth);
            }
            EntityMetadata::WorkflowCatalog(catalog) if entity.id == instance.workflow => {
                workflow = catalog.model_auth;
            }
            _ => (),
        }
    }
    Ok((agents, workflow))
}

/// Read the model ids a harness tool instance's tool never resolves to
///
/// # Arguments
///
/// * `harness_entity` - The harness tool instance entity
/// * `shared` - Shared Thorium objects
async fn harness_root(
    harness_entity: &Entity,
    shared: &Shared,
) -> Result<Option<Entity>, ApiError> {
    // the instance names its tool
    let EntityMetadata::ToolInstance(instance) = &harness_entity.metadata else {
        return Ok(None);
    };
    // read the tool in the instance's groups
    let found =
        db::entities::get_many_untagged(&harness_entity.groups, &[instance.tool], shared).await?;
    Ok(found.into_iter().next())
}

/// Resolve the model of every agent of a workflow run in its run group
///
/// A run whose agents can't all be resolved is refused with `400`, the mismatch text as its
/// message, and the `ModelResolutionError` fields in its body.
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `sources` - The workflow run's instance and harness
/// * `group` - The run group
/// * `shared` - Shared Thorium objects
#[instrument(name = "reactions::models::resolve_run", skip_all, fields(group = group), err(Debug))]
pub async fn resolve_run(
    user: &User,
    sources: &RunModelSources<'_>,
    group: &str,
    shared: &Shared,
) -> Result<Vec<ReactionModelBinding>, ApiError> {
    // read the run group's endpoints and profiles
    let endpoints = group_entities(user, EntityKinds::ModelEndpoint, group, shared)
        .await?
        .into_iter()
        .filter_map(|entity| match entity.metadata {
            EntityMetadata::ModelEndpoint(meta) => Some(EndpointRecord {
                id: entity.id,
                name: entity.name,
                meta,
            }),
            _ => None,
        })
        .collect::<Vec<EndpointRecord>>();
    let profiles = group_entities(user, EntityKinds::ModelProfile, group, shared)
        .await?
        .into_iter()
        .filter_map(|entity| match entity.metadata {
            EntityMetadata::ModelProfile(meta) => Some(ProfileRecord {
                id: entity.id,
                name: entity.name,
                meta,
            }),
            _ => None,
        })
        .collect::<Vec<ProfileRecord>>();
    // read the endpoints' credentials once each
    let credentials = group_credentials(&endpoints, group, shared).await?;
    // read the deployment's allowed endpoints
    let whitelist = db::system::get_settings(shared).await?.models;
    // read the harness's disabled models and the live auth sources
    let disabled = match harness_root(sources.harness_entity, shared).await? {
        Some(Entity {
            metadata: EntityMetadata::Tool(tool),
            ..
        }) => tool.disabled_models,
        _ => BTreeSet::new(),
    };
    let (agent_auth, workflow_auth) = auth_sources(sources.instance, shared).await?;
    let user_settings = model_settings(user);
    // resolve every agent in memory
    let inputs = ResolveInputs {
        agents: &sources.instance.agents,
        default_models: &sources.instance.default_models,
        agent_auth: &agent_auth,
        workflow_auth: &workflow_auth,
        endpoints: &endpoints,
        profiles: &profiles,
        credentials: &credentials,
        user: &user_settings,
        whitelist: &whitelist,
        harness: &sources.harness.capabilities,
        harness_disabled: &disabled,
    };
    match resolve(&inputs) {
        Ok(bindings) => Ok(bindings),
        Err(error) => {
            // name the endpoints, run group, instance, and harness in the text
            let mut names = MismatchNames {
                instance: sources.entity.name.clone(),
                group: group.to_owned(),
                harness: format!("harness \"{}\"", sources.harness_entity.name),
                endpoints: BTreeMap::new(),
            };
            names.endpoints.extend(
                endpoints
                    .iter()
                    .map(|endpoint| (endpoint.id, endpoint.name.clone())),
            );
            names.endpoints.extend(
                user_settings
                    .endpoints
                    .iter()
                    .map(|(id, endpoint)| (*id, endpoint.name.clone())),
            );
            Err(mismatch_error(&error, &names))
        }
    }
}

/// Build the `400` a run refused for its models returns
///
/// # Arguments
///
/// * `error` - Why resolution failed
/// * `names` - The names to render the text with
fn mismatch_error(error: &ModelResolutionError, names: &MismatchNames) -> ApiError {
    // the body carries the error's fields beside the text
    let details = match serde_json::to_value(error) {
        Ok(serde_json::Value::Object(details)) => details,
        _ => serde_json::Map::new(),
    };
    ApiError::Detailed {
        code: axum::http::StatusCode::BAD_REQUEST,
        msg: error.render(names),
        details,
    }
}

/// The configuration a run's bindings name, read for the run models read
pub struct DeliverySources<'a> {
    /// The group endpoints the bindings name, by id
    pub endpoints: &'a HashMap<Uuid, ModelEndpointEntity>,
    /// The group profiles the bindings name, by id
    pub profiles: &'a HashMap<Uuid, ModelProfileEntity>,
    /// The group credentials the bindings name and the access keys assumed roles use, by id
    pub credentials: &'a HashMap<Uuid, ModelCredentialEntity>,
    /// The runner's own model settings
    pub user: &'a UserModelSettings,
    /// The deployment's current allowed endpoint list
    pub whitelist: &'a ModelsConf,
    /// The values the harness exports and their names
    pub harness_env: Option<&'a HarnessEnv>,
    /// The path of the harness's models file
    pub models_file: &'a str,
}

/// Build the credential a binding names as its harness gets it, with its secrets
///
/// Returns why it can't be delivered when it's deleted or banned, or when an assumed role's
/// access key is. A ban is named only by its kind, since the reason reaches the run's stage logs.
///
/// # Arguments
///
/// * `reference` - The credential the binding names
/// * `sources` - The configuration the bindings name
fn deliver_credential(
    reference: ModelConfigRef,
    sources: &DeliverySources,
) -> Result<HarnessCredential, String> {
    // find the credential and the access key an assumed role uses
    let (kind, secrets, bans, source) = match reference {
        ModelConfigRef::Group(id) => {
            let Some(credential) = sources.credentials.get(&id) else {
                return Err(format!("model credential {id} no longer exists"));
            };
            let source = match credential.kind.role_source() {
                Some(source) => Some(deliver_credential(ModelConfigRef::Group(source), sources)?),
                None => None,
            };
            (
                &credential.kind,
                &credential.secrets,
                &credential.bans,
                source,
            )
        }
        ModelConfigRef::User(id) => {
            let Some(credential) = sources.user.credentials.values().find(|cred| cred.id == id)
            else {
                return Err(format!(
                    "the runner's model credential {id} no longer exists"
                ));
            };
            let source = match credential.kind.role_source() {
                Some(source) => Some(deliver_credential(ModelConfigRef::User(source), sources)?),
                None => None,
            };
            (
                &credential.kind,
                &credential.secrets,
                &credential.bans,
                source,
            )
        }
    };
    // a banned credential can't be delivered; the refusal reaches the run's stage logs, so it
    // names only the ban's kind and never its detail (a provider's message can echo the key)
    if let Some(ban) = first_ban(bans.values()) {
        return Err(format!(
            "model credential {} is banned ({})",
            match reference {
                ModelConfigRef::Group(id) | ModelConfigRef::User(id) => id,
            },
            ban.ban_kind.name()
        ));
    }
    Ok(HarnessCredential::new(
        reference,
        kind.clone(),
        secrets.clone(),
        source,
    ))
}

/// Check that the deployment still allows an endpoint a binding names
///
/// Returns why it can't be delivered when a scan banned it `NotAllowedByDeployment` or it's no
/// longer on the allowed endpoint list. Other endpoint bans don't stop delivery.
///
/// # Arguments
///
/// * `reference` - The endpoint the binding names
/// * `provider` - The endpoint's provider and connection settings
/// * `bans` - The endpoint's bans
/// * `whitelist` - The deployment's current allowed endpoint list
fn deployment_allows(
    reference: ModelConfigRef,
    provider: &ModelProvider,
    bans: &HashMap<Uuid, ModelBan>,
    whitelist: &ModelsConf,
) -> Result<(), String> {
    // name a group endpoint or the runner's own the way the other delivery refusals do
    let name = match reference {
        ModelConfigRef::Group(id) => format!("model endpoint {id}"),
        ModelConfigRef::User(id) => format!("the runner's model endpoint {id}"),
    };
    // a scan's ban says the deployment stopped allowing it; like a credential's, the refusal
    // names only the ban's kind
    let scan_bans = bans
        .values()
        .filter(|ban| ban.ban_kind == ModelBanKind::NotAllowedByDeployment);
    if let Some(ban) = first_ban(scan_bans) {
        return Err(format!("{name} is banned ({})", ban.ban_kind.name()));
    }
    // the list itself is checked too, so a removal applies before any scan
    if !whitelist.allows(provider) {
        return Err(format!(
            "{name} is no longer on the deployment's allowed endpoint list"
        ));
    }
    Ok(())
}

/// Build the model one binding names as its harness gets it
///
/// Returns why it can't be delivered when its endpoint is deleted or no longer allowed by the
/// deployment, or when its credential can't be delivered.
///
/// # Arguments
///
/// * `binding` - The binding
/// * `sources` - The configuration the bindings name
fn deliver_binding(
    binding: &ReactionModelBinding,
    sources: &DeliverySources,
) -> Result<HarnessModelEntry, String> {
    // find the endpoint's provider, catalog, and bans
    let (provider, catalog, bans) = match binding.endpoint {
        ModelConfigRef::Group(id) => match sources.endpoints.get(&id) {
            Some(endpoint) => (
                &endpoint.provider,
                endpoint.catalog.as_slice(),
                &endpoint.bans,
            ),
            None => return Err(format!("model endpoint {id} no longer exists")),
        },
        ModelConfigRef::User(id) => match sources.user.endpoints.get(&id) {
            Some(endpoint) => (
                &endpoint.provider,
                endpoint.catalog.as_slice(),
                &endpoint.bans,
            ),
            None => return Err(format!("the runner's model endpoint {id} no longer exists")),
        },
    };
    // an endpoint the deployment no longer allows isn't delivered, even to a run queued before
    deployment_allows(binding.endpoint, provider, bans, sources.whitelist)?;
    // the profile's parameters, limits, and capabilities apply over the catalog entry's
    let entry = catalog
        .iter()
        .find(|entry| entry.model_id == binding.model_id);
    let profile = binding.profile.and_then(|id| sources.profiles.get(&id));
    let limits = profile
        .and_then(|profile| profile.limits.clone())
        .or_else(|| entry.map(|entry| entry.limits.clone()))
        .unwrap_or_default();
    let capabilities = profile
        .and_then(|profile| profile.capabilities.clone())
        .or_else(|| entry.map(|entry| entry.capabilities.clone()))
        .unwrap_or_default();
    let parameters = profile.map_or_default(|profile| profile.parameters.clone());
    // the credential is read at delivery time, so a rotation is picked up
    let credential = match binding.credential {
        Some(reference) => Some(deliver_credential(reference, sources)?),
        None => None,
    };
    Ok(HarnessModelEntry {
        provider: provider.clone(),
        model_id: binding.model_id.clone(),
        api: binding.api,
        parameters,
        limits,
        capabilities,
        credential,
    })
}

/// Build the models a run's harness gets from its recorded bindings
///
/// Returns why they can't be delivered when an endpoint they name is deleted or no longer allowed
/// by the deployment, or a credential they name is deleted or banned.
///
/// # Arguments
///
/// * `bindings` - The run's bindings
/// * `entry_agent` - The run's entry agent, whose values go in the env
/// * `sources` - The configuration the bindings name
pub fn build_run_models(
    bindings: &[ReactionModelBinding],
    entry_agent: Uuid,
    sources: &DeliverySources,
) -> Result<RunModels, String> {
    // build each agent's model
    let mut models = HarnessModels::default();
    for binding in bindings {
        models
            .agents
            .insert(binding.agent, deliver_binding(binding, sources)?);
    }
    // the entry agent's values go under the harness's env names
    let env = match (sources.harness_env, models.agents.get(&entry_agent)) {
        (Some(env), Some(entry)) => RunModels::harness_env(env, entry, sources.models_file),
        _ => BTreeMap::new(),
    };
    Ok(RunModels { models, env })
}

/// Read some entities in all of their groups, without their tags
///
/// # Arguments
///
/// * `ids` - The entities' ids
/// * `shared` - Shared Thorium objects
async fn read_everywhere(ids: &[Uuid], shared: &Shared) -> Result<Vec<Entity>, ApiError> {
    // nothing to read for no ids
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // find every group the entities are in
    let groups = db::entities::groups_and_kinds(ids, shared)
        .await?
        .into_iter()
        .map(|row| row.group)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect::<Vec<String>>();
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    db::entities::get_many_untagged(&groups, ids, shared).await
}

/// Get the models of a run with their credentials' secrets, for the admin that spawns its harness
///
/// Only admins authenticated with their full token may read them. A reaction with no live run
/// token, or that isn't a workflow run, has none (`404`). A run can't get them (`409`) when a bound
/// endpoint or credential is deleted, a bound credential is banned, or a bound endpoint is banned
/// `NotAllowedByDeployment` or no longer on the deployment's allowed endpoint list.
///
/// # Arguments
///
/// * `user` - The user reading the run's models
/// * `group` - The group the run's reaction is in
/// * `id` - The id of the run's reaction
/// * `shared` - Shared Thorium objects
#[instrument(name = "reactions::models::run_models", skip_all, fields(reaction = id.to_string()), err(Debug))]
pub async fn run_models(
    user: &AuthedUser,
    group: &str,
    id: &Uuid,
    shared: &Shared,
) -> Result<RunModels, ApiError> {
    // only admins authenticated with their full token may read run models
    if user.is_scoped() {
        return unauthorized!();
    }
    is_admin!(user);
    // get the run's reaction
    let reaction: Reaction = db::reactions::get(group, id, shared).await?;
    // the run must still have its live token
    let owner = User::force_get(&reaction.creator, shared).await?;
    super::live_run_token(&reaction, &owner, shared).await?;
    // only workflow runs have model bindings
    let ReactionSource::WorkflowRun(run) = &reaction.source else {
        return not_found!(format!("reaction {id} has no model bindings"));
    };
    // read the harness instance and its tool for its models file and env
    let harness = read_everywhere(&[run.harness], shared).await?;
    let (models_file, tool) = match harness.first().map(|entity| &entity.metadata) {
        Some(EntityMetadata::ToolInstance(instance)) => {
            let models_file = match &instance.spec {
                crate::models::ToolSpec::Harness(spec) => spec.interface.inputs.models.clone(),
                _ => crate::models::HarnessInputs::default().models,
            };
            (models_file, Some(instance.tool))
        }
        _ => (crate::models::HarnessInputs::default().models, None),
    };
    let harness_env = match tool {
        Some(tool) => match read_everywhere(&[tool], shared).await?.into_iter().next() {
            Some(Entity {
                metadata: EntityMetadata::Tool(tool),
                ..
            }) => tool.harness_env,
            _ => None,
        },
        None => None,
    };
    // read each group endpoint, profile, and credential the bindings name once
    let mut group_ids = BTreeSet::new();
    for binding in &run.models {
        if let ModelConfigRef::Group(endpoint) = binding.endpoint {
            group_ids.insert(endpoint);
        }
        if let Some(profile) = binding.profile {
            group_ids.insert(profile);
        }
        if let Some(ModelConfigRef::Group(credential)) = binding.credential {
            group_ids.insert(credential);
        }
    }
    let mut endpoints = HashMap::new();
    let mut profiles = HashMap::new();
    let mut credentials = HashMap::new();
    let ids = group_ids.into_iter().collect::<Vec<Uuid>>();
    for entity in read_everywhere(&ids, shared).await? {
        match entity.metadata {
            EntityMetadata::ModelEndpoint(meta) => {
                endpoints.insert(entity.id, meta);
            }
            EntityMetadata::ModelProfile(meta) => {
                profiles.insert(entity.id, meta);
            }
            EntityMetadata::ModelCredential(meta) => {
                credentials.insert(entity.id, meta);
            }
            _ => (),
        }
    }
    // then the access keys any assumed role uses
    let sources = credentials
        .values()
        .filter_map(|credential: &ModelCredentialEntity| credential.kind.role_source())
        .filter(|source| !credentials.contains_key(source))
        .collect::<Vec<Uuid>>();
    for entity in read_everywhere(&sources, shared).await? {
        if let EntityMetadata::ModelCredential(meta) = entity.metadata {
            credentials.insert(entity.id, meta);
        }
    }
    // read the deployment's current allowed endpoint list, so a removal reaches queued runs
    let whitelist = db::system::get_settings(shared).await?.models;
    // build the run's models from what was read
    let user_settings = model_settings(&owner);
    let delivery = DeliverySources {
        endpoints: &endpoints,
        profiles: &profiles,
        credentials: &credentials,
        user: &user_settings,
        whitelist: &whitelist,
        harness_env: harness_env.as_ref(),
        models_file: &models_file,
    };
    match build_run_models(&run.models, run.agent, &delivery) {
        Ok(models) => Ok(models),
        Err(reason) => conflict!(format!(
            "the models of reaction {id} can't be delivered: {reason}"
        )),
    }
}
