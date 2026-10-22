//! Tests for resolving a run's models and building the models its harness gets

use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

use super::{
    DeliverySources, EndpointRecord, ProfileRecord, ResolveInputs, build_run_models, resolve,
};
use crate::models::{
    AgentConfig, AllowedEndpoint, AwsRoleSource, AzureClientAuth, BedrockEndpointKind,
    CatalogEntrySource, CredentialKind, CredentialSecrets, CredentialType, HarnessCapabilities,
    HarnessEnv, HarnessEnvVar, HarnessModelEntry, InstancedAgent, LimitsSource, MismatchNames,
    ModelApi, ModelAuthLevel, ModelAuthSource, ModelBan, ModelBanKind, ModelCapabilities,
    ModelCatalogEntry, ModelConfigRef, ModelCredentialEntity, ModelEndpointEntity, ModelLimits,
    ModelMatcher, ModelParameters, ModelProfileEntity, ModelProvider, ModelProviderKind,
    ModelRefreshStatus, ModelRejectReason, ModelRequirements, ModelResolutionError, ModelSecret,
    ModelsConf, ReactionModelBinding, RunModels, UserCredentialRef, UserModelEndpoint,
    UserModelSettings,
};

/// The secret every test credential holds
const SECRET: &str = "sk-resolver-test-secret";

/// Build an `OpenAI` provider at its default url
fn openai() -> ModelProvider {
    ModelProvider::OpenAi {
        base_url: None,
        organization: None,
        project: None,
    }
}

/// Build an Anthropic provider with an API version
///
/// # Arguments
///
/// * `version` - The `anthropic-version` the endpoint sends
fn anthropic(version: &str) -> ModelProvider {
    ModelProvider::Anthropic {
        base_url: None,
        version: version.to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    }
}

/// Build a Bedrock provider
///
/// # Arguments
///
/// * `kind` - Which Bedrock endpoint family it calls
fn bedrock(kind: BedrockEndpointKind) -> ModelProvider {
    ModelProvider::Bedrock {
        region: "us-east-1".to_owned(),
        endpoint_kind: kind,
        endpoint_url: None,
        workspace_id: None,
    }
}

/// Build a keyless-capable `OpenAI` compatible provider
///
/// # Arguments
///
/// * `url` - The server's url
fn compatible(url: &str) -> ModelProvider {
    ModelProvider::OpenAiCompatible {
        base_url: url.to_owned(),
        extra_headers: BTreeMap::new(),
        key_header: None,
    }
}

/// Build a catalog entry with known limits and tool calling
///
/// # Arguments
///
/// * `model_id` - The model id
/// * `context` - The context window
fn entry(model_id: &str, context: Option<u64>) -> ModelCatalogEntry {
    ModelCatalogEntry {
        model_id: model_id.to_owned(),
        limits: ModelLimits {
            context_window: context,
            max_output_tokens: Some(8_192),
            source: LimitsSource::Manual,
        },
        capabilities: ModelCapabilities {
            tool_calling: true,
            ..ModelCapabilities::default()
        },
        bans: HashMap::new(),
        source: CatalogEntrySource::Manual,
    }
}

/// Build a group endpoint serving some models
///
/// # Arguments
///
/// * `name` - The endpoint's name
/// * `provider` - The endpoint's provider
/// * `credential` - The group credential it authenticates with
/// * `models` - The model ids its catalog lists
fn endpoint(
    name: &str,
    provider: ModelProvider,
    credential: Option<Uuid>,
    models: &[&str],
) -> EndpointRecord {
    // keep the catalog sorted by model id
    let mut catalog = models
        .iter()
        .map(|model| entry(model, Some(200_000)))
        .collect::<Vec<ModelCatalogEntry>>();
    catalog.sort_by(|a, b| a.model_id.cmp(&b.model_id));
    EndpointRecord {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        meta: ModelEndpointEntity {
            provider,
            credential,
            catalog,
            bans: HashMap::new(),
            refresh: ModelRefreshStatus::default(),
            disabled_models: BTreeSet::new(),
            modified: Utc::now(),
        },
    }
}

/// Build a group profile of a model on an endpoint
///
/// # Arguments
///
/// * `name` - The profile's name
/// * `endpoint` - The endpoint it calls
/// * `model_id` - The model it calls
/// * `api` - The protocol it calls the model with
fn profile(name: &str, endpoint: Uuid, model_id: &str, api: ModelApi) -> ProfileRecord {
    ProfileRecord {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        meta: ModelProfileEntity {
            endpoint,
            model_id: model_id.to_owned(),
            api,
            parameters: ModelParameters::default(),
            limits: None,
            capabilities: None,
            bans: HashMap::new(),
            modified: Utc::now(),
        },
    }
}

/// Build a group credential of a kind
///
/// # Arguments
///
/// * `kind` - The kind of credential
fn credential(kind: CredentialKind) -> ModelCredentialEntity {
    ModelCredentialEntity {
        kind,
        secrets: CredentialSecrets {
            primary: Some(ModelSecret::from(SECRET)),
            session_token: None,
            fingerprint: None,
        },
        expires: None,
        rotated: None,
        bans: HashMap::new(),
        modified: Utc::now(),
    }
}

/// Build a matcher for an exact model id
///
/// # Arguments
///
/// * `model_id` - The model id
fn matcher(model_id: &str) -> ModelMatcher {
    ModelMatcher {
        model_id: model_id.to_owned(),
        ..ModelMatcher::default()
    }
}

/// Build an agent of a workflow instance that accepts some models
///
/// # Arguments
///
/// * `name` - The agent's name
/// * `models` - The models it accepts in priority order
fn agent(name: &str, models: Vec<ModelMatcher>) -> InstancedAgent {
    InstancedAgent {
        agent: Uuid::new_v4(),
        name: name.to_owned(),
        groups: vec!["corn".to_owned()],
        config: AgentConfig {
            models,
            ..AgentConfig::default()
        },
        config_hash: String::new(),
        harness: None,
        tools: Vec::new(),
        tool_sets: Vec::new(),
        bindings: Vec::new(),
        sub_agents: Vec::new(),
        toolset_digest: String::new(),
    }
}

/// Build a ban of a kind
///
/// # Arguments
///
/// * `kind` - The kind of ban
fn ban(kind: ModelBanKind) -> (Uuid, ModelBan) {
    let ban = ModelBan::new(kind);
    (ban.id, ban)
}

/// Build an allowed endpoint list holding exactly some endpoints' providers
///
/// # Arguments
///
/// * `providers` - The providers to allow
fn allowing<'p>(providers: impl IntoIterator<Item = &'p ModelProvider>) -> ModelsConf {
    // one entry per provider and url
    let mut conf = ModelsConf::default();
    for provider in providers {
        conf.allowed_endpoints
            .insert(AllowedEndpoint::for_provider(provider));
    }
    conf
}

/// Everything a resolution reads, owned by a test
struct World {
    /// The agents to resolve, in agent order
    agents: Vec<InstancedAgent>,
    /// The workflow's default models
    default_models: Vec<ModelMatcher>,
    /// Each agent's auth source
    agent_auth: HashMap<Uuid, ModelAuthSource>,
    /// The workflow's auth source
    workflow_auth: ModelAuthSource,
    /// The run group's endpoints
    endpoints: Vec<EndpointRecord>,
    /// The run group's profiles
    profiles: Vec<ProfileRecord>,
    /// The group credentials by id
    credentials: HashMap<Uuid, ModelCredentialEntity>,
    /// The runner's own model settings
    user: UserModelSettings,
    /// The allowed endpoint list, unless every endpoint is allowed
    whitelist: Option<ModelsConf>,
    /// What the harness can call
    harness: HarnessCapabilities,
    /// The models the harness disables
    disabled: BTreeSet<String>,
}

impl World {
    /// Build a world for some agents with a harness that can call every protocol and credential
    ///
    /// # Arguments
    ///
    /// * `agents` - The agents to resolve
    fn new(agents: Vec<InstancedAgent>) -> Self {
        // a harness that speaks everything and gives agents their own models
        let harness = HarnessCapabilities {
            model_apis: [
                ModelApi::OpenAiChat,
                ModelApi::OpenAiResponses,
                ModelApi::AnthropicMessages,
                ModelApi::BedrockConverse,
                ModelApi::BedrockInvoke,
            ]
            .into_iter()
            .collect(),
            credential_kinds: [
                CredentialType::ApiKey,
                CredentialType::AwsAccessKey,
                CredentialType::AwsAssumeRole,
                CredentialType::AwsBearerToken,
            ]
            .into_iter()
            .collect(),
            per_agent_models: true,
            ..HarnessCapabilities::default()
        };
        World {
            agents,
            default_models: Vec::new(),
            agent_auth: HashMap::new(),
            workflow_auth: ModelAuthSource::default(),
            endpoints: Vec::new(),
            profiles: Vec::new(),
            credentials: HashMap::new(),
            user: UserModelSettings::default(),
            whitelist: None,
            harness,
            disabled: BTreeSet::new(),
        }
    }

    /// Add a group credential, returning its id
    ///
    /// # Arguments
    ///
    /// * `cred` - The credential
    fn add_credential(&mut self, cred: ModelCredentialEntity) -> Uuid {
        let id = Uuid::new_v4();
        self.credentials.insert(id, cred);
        id
    }

    /// Add a group endpoint with a new API key credential, returning its index
    ///
    /// # Arguments
    ///
    /// * `name` - The endpoint's name
    /// * `provider` - The endpoint's provider
    /// * `models` - The models its catalog lists
    fn keyed_endpoint(&mut self, name: &str, provider: ModelProvider, models: &[&str]) -> usize {
        let cred = self.add_credential(credential(CredentialKind::ApiKey));
        self.endpoints
            .push(endpoint(name, provider, Some(cred), models));
        self.endpoints.len() - 1
    }

    /// Give the runner an API key credential for an endpoint, returning the credential's id
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint's id
    fn user_key(&mut self, endpoint: Uuid) -> Uuid {
        let cred = UserCredentialRef::new(CredentialKind::ApiKey, Some(ModelSecret::from(SECRET)));
        let id = cred.id;
        self.user.credentials.insert(endpoint, cred);
        id
    }

    /// Resolve this world's agents
    fn resolve(&self) -> Result<Vec<ReactionModelBinding>, ModelResolutionError> {
        // allow every endpoint unless a test set its own list
        let whitelist = self.whitelist.clone().unwrap_or_else(|| {
            let mut conf = ModelsConf::default();
            for provider in self
                .endpoints
                .iter()
                .map(|endpoint| &endpoint.meta.provider)
                .chain(
                    self.user
                        .endpoints
                        .values()
                        .map(|endpoint| &endpoint.provider),
                )
            {
                conf.allowed_endpoints
                    .insert(AllowedEndpoint::for_provider(provider));
            }
            conf
        });
        let inputs = ResolveInputs {
            agents: &self.agents,
            default_models: &self.default_models,
            agent_auth: &self.agent_auth,
            workflow_auth: &self.workflow_auth,
            endpoints: &self.endpoints,
            profiles: &self.profiles,
            credentials: &self.credentials,
            user: &self.user,
            whitelist: &whitelist,
            harness: &self.harness,
            harness_disabled: &self.disabled,
        };
        resolve(&inputs)
    }

    /// Resolve this world's one agent, which must succeed
    fn binding(&self) -> ReactionModelBinding {
        let bindings = self.resolve().expect("resolution failed");
        assert_eq!(bindings.len(), 1);
        bindings.into_iter().next().unwrap()
    }

    /// Resolve this world's agents, which must fail
    fn mismatch(&self) -> ModelResolutionError {
        self.resolve().expect_err("resolution succeeded")
    }
}

/// Get the reasons of an error's only agent's rejections
///
/// # Arguments
///
/// * `error` - The resolution error
fn reasons(error: &ModelResolutionError) -> Vec<ModelRejectReason> {
    error.agents[0]
        .candidates
        .iter()
        .map(|candidate| candidate.reason.clone())
        .collect()
}

/// An agent's first model at the user level is chosen over its second at the group level
#[test]
fn priority_wins_over_level() {
    // model one only through the runner's own endpoint, model two through a group endpoint
    let mut world = World::new(vec![agent("triage", vec![matcher("one"), matcher("two")])]);
    world.keyed_endpoint("corn-two", openai(), &["two"]);
    let own = Uuid::new_v4();
    world.user.endpoints.insert(
        own,
        UserModelEndpoint::new("mine", anthropic("2023-06-01"))
            .model(crate::models::ModelCatalogEntryRequest::new("one")),
    );
    let cred = world.user_key(own);
    // model one wins at the user level
    let binding = world.binding();
    assert_eq!(binding.model_id, "one");
    assert_eq!(binding.level, ModelAuthLevel::User);
    assert_eq!(binding.priority, Some(0));
    assert_eq!(binding.endpoint, ModelConfigRef::User(own));
    assert_eq!(binding.credential, Some(ModelConfigRef::User(cred)));
}

/// A group credential is used before the runner's own for one model, and an auth source
/// restricted to the user level skips the group
#[test]
fn group_credential_before_user() {
    // one group endpoint with a group credential, and the runner's key for it
    let mut world = World::new(vec![agent("triage", vec![matcher("gpt-5")])]);
    let index = world.keyed_endpoint("corn-openai", openai(), &["gpt-5"]);
    let id = world.endpoints[index].id;
    let group_cred = world.endpoints[index].meta.credential.unwrap();
    let user_cred = world.user_key(id);
    // the group level wins
    let binding = world.binding();
    assert_eq!(binding.level, ModelAuthLevel::Group);
    assert_eq!(binding.credential, Some(ModelConfigRef::Group(group_cred)));
    // an agent limited to the user level uses the runner's key for the same endpoint
    let agent = world.agents[0].agent;
    world.agent_auth.insert(
        agent,
        ModelAuthSource {
            levels: [ModelAuthLevel::User].into_iter().collect(),
        },
    );
    let binding = world.binding();
    assert_eq!(binding.level, ModelAuthLevel::User);
    assert_eq!(binding.endpoint, ModelConfigRef::Group(id));
    assert_eq!(binding.credential, Some(ModelConfigRef::User(user_cred)));
}

/// Banned endpoints, catalog models, profiles, and credentials are skipped
#[test]
fn bans_are_skipped() {
    // four endpoints, each with one thing banned, then a clean one
    let mut world = World::new(vec![agent("triage", vec![matcher("gpt-5")])]);
    for name in [
        "a-endpoint",
        "b-catalog",
        "c-profile",
        "d-credential",
        "e-clean",
    ] {
        world.keyed_endpoint(name, openai(), &["gpt-5"]);
    }
    let (id, endpoint_ban) = ban(ModelBanKind::Generic("down".to_owned()));
    world.endpoints[0].meta.bans.insert(id, endpoint_ban);
    let (id, catalog_ban) = ban(ModelBanKind::ModelRemoved);
    world.endpoints[1].meta.catalog[0]
        .bans
        .insert(id, catalog_ban);
    let mut banned_profile = profile("p", world.endpoints[2].id, "gpt-5", ModelApi::OpenAiChat);
    let (id, profile_ban) = ban(ModelBanKind::Generic("bad profile".to_owned()));
    banned_profile.meta.bans.insert(id, profile_ban);
    world.profiles.push(banned_profile);
    // the profile's endpoint is also unusable through its catalog, so only the profile is tried
    let (id, entry_ban) = ban(ModelBanKind::ModelRemoved);
    world.endpoints[2].meta.catalog[0]
        .bans
        .insert(id, entry_ban);
    let cred = world.endpoints[3].meta.credential.unwrap();
    let (id, cred_ban) = ban(ModelBanKind::CredentialRejected {
        status: 401,
        message: "invalid key".to_owned(),
    });
    world
        .credentials
        .get_mut(&cred)
        .unwrap()
        .bans
        .insert(id, cred_ban);
    // the clean endpoint is chosen
    let binding = world.binding();
    assert_eq!(
        binding.endpoint,
        ModelConfigRef::Group(world.endpoints[4].id)
    );
    // with it banned too, every ban is reported
    let (id, last) = ban(ModelBanKind::EndpointUnreachable("timeout".to_owned()));
    world.endpoints[4].meta.bans.insert(id, last);
    let error = world.mismatch();
    let banned = reasons(&error)
        .into_iter()
        .filter(|reason| matches!(reason, ModelRejectReason::Banned(_)))
        .count();
    assert_eq!(banned, 6);
    assert!(reasons(&error).contains(&ModelRejectReason::Banned(ModelBanKind::ModelRemoved)));
    assert!(reasons(&error).contains(&ModelRejectReason::Banned(
        ModelBanKind::CredentialRejected {
            status: 401,
            message: "invalid key".to_owned()
        }
    )));
}

/// Requirements skip models below a minimum, with an unknown limit, or without a capability
#[test]
fn requirements_skip_models() {
    // an agent needing a 150000 context window
    let mut needy = agent(
        "triage",
        vec![matcher("small"), matcher("unknown"), matcher("exact")],
    );
    needy.config.model_requirements = ModelRequirements {
        min_context_window: Some(150_000),
        ..ModelRequirements::default()
    };
    let mut world = World::new(vec![needy]);
    let index = world.keyed_endpoint("corn", openai(), &["exact", "small", "unknown"]);
    for catalog in &mut world.endpoints[index].meta.catalog {
        catalog.limits.context_window = match catalog.model_id.as_str() {
            "small" => Some(100_000),
            "exact" => Some(150_000),
            _ => None,
        };
    }
    // exactly the minimum is accepted after both others are skipped
    let binding = world.binding();
    assert_eq!(binding.model_id, "exact");
    // the skipped reasons name the requirement, and the unknown one says where to set it
    world.endpoints[index].meta.catalog[0].limits.context_window = Some(149_999);
    let error = world.mismatch();
    let missed = error.agents[0]
        .candidates
        .iter()
        .filter_map(|candidate| match &candidate.reason {
            ModelRejectReason::RequirementNotMet(why) => Some(why.clone()),
            _ => None,
        })
        .collect::<Vec<String>>();
    assert_eq!(missed.len(), 3);
    assert!(missed.iter().all(|why| why.contains("min_context_window")));
    assert!(missed[1].contains("catalog entry or profile"));
    // an unknown max output fails a minimum output, and a missing capability fails too
    let mut limited = agent("triage", vec![matcher("quiet"), matcher("plain")]);
    limited.config.model_requirements = ModelRequirements {
        min_output_tokens: Some(4_096),
        capabilities: ModelCapabilities {
            vision: true,
            ..ModelCapabilities::default()
        },
        ..ModelRequirements::default()
    };
    let mut world = World::new(vec![limited]);
    let index = world.keyed_endpoint("corn", openai(), &["plain", "quiet"]);
    world.endpoints[index].meta.catalog[1]
        .limits
        .max_output_tokens = None;
    let error = world.mismatch();
    let missed = error.agents[0]
        .candidates
        .iter()
        .filter_map(|candidate| match &candidate.reason {
            ModelRejectReason::RequirementNotMet(why) => Some(why.clone()),
            _ => None,
        })
        .collect::<Vec<String>>();
    assert!(
        missed[0].contains("min_output_tokens") && missed[0].contains("catalog entry or profile")
    );
    assert!(missed[1].contains("vision"));
}

/// A forced tool choice requirement skips a model that can't force a tool choice
#[test]
fn forced_tool_choice_requirement() {
    // an agent that forces its first tool call
    let mut forcing = agent("triage", vec![matcher("loose"), matcher("strict")]);
    forcing
        .config
        .model_requirements
        .capabilities
        .forced_tool_choice = true;
    let mut world = World::new(vec![forcing]);
    let index = world.keyed_endpoint("corn", openai(), &["loose", "strict"]);
    world.endpoints[index].meta.catalog[1]
        .capabilities
        .forced_tool_choice = true;
    // the model without it is skipped naming it, and the next resolves
    let binding = world.binding();
    assert_eq!(binding.model_id, "strict");
    world.endpoints[index].meta.catalog[1]
        .capabilities
        .forced_tool_choice = false;
    let error = world.mismatch();
    assert!(reasons(&error).iter().any(|reason| matches!(
        reason,
        ModelRejectReason::RequirementNotMet(why) if why.contains("forced_tool_choice")
    )));
}

/// A protocol or credential kind the harness doesn't list is skipped, and a harness without
/// per-agent models refuses agents on different models
#[test]
fn harness_capabilities_filter() {
    // a harness that only speaks the Responses API with API keys
    let mut world = World::new(vec![agent("triage", vec![matcher("gpt-5")])]);
    world.keyed_endpoint("corn", openai(), &["gpt-5"]);
    world.harness.model_apis = [ModelApi::OpenAiResponses].into_iter().collect();
    let error = world.mismatch();
    assert!(reasons(&error).contains(&ModelRejectReason::ApiNotSupported(ModelApi::OpenAiChat)));
    // a matcher asking for the Responses API passes
    world.agents[0].config.models[0].api = Some(ModelApi::OpenAiResponses);
    assert_eq!(world.binding().api, ModelApi::OpenAiResponses);
    // a credential kind the harness can't use is skipped
    let mut world = World::new(vec![agent("triage", vec![matcher("claude")])]);
    let cred = world.add_credential(credential(CredentialKind::AwsBearerToken));
    world.endpoints.push(endpoint(
        "bedrock",
        bedrock(BedrockEndpointKind::Runtime),
        Some(cred),
        &["claude"],
    ));
    world.harness.credential_kinds = [CredentialType::ApiKey].into_iter().collect();
    let error = world.mismatch();
    assert!(
        reasons(&error).contains(&ModelRejectReason::CredentialNotSupported(
            CredentialType::AwsBearerToken
        ))
    );
    // two agents on different models with a harness that can't do that name each agent
    let mut world = World::new(vec![
        agent("lead", vec![matcher("one")]),
        agent("helper", vec![matcher("two")]),
    ]);
    world.keyed_endpoint("corn", openai(), &["one", "two"]);
    world.harness.per_agent_models = false;
    let error = world.mismatch();
    assert_eq!(error.agents.len(), 2);
    for (mismatch, model) in error.agents.iter().zip(["one", "two"]) {
        assert_eq!(mismatch.candidates.len(), 1);
        assert_eq!(
            mismatch.candidates[0].reason,
            ModelRejectReason::MixedModelsUnsupported
        );
        assert_eq!(
            mismatch.candidates[0].matcher.as_ref().unwrap().model_id,
            model
        );
    }
}

/// Agents of a harness without per-agent models that resolve to the same model are accepted,
/// and a harness with per-agent models takes different ones
#[test]
fn per_agent_models() {
    // three agents on one model with a harness that has no per-agent models
    let agents = ["lead", "helper", "critic"]
        .iter()
        .map(|name| agent(name, vec![matcher("gpt-5")]))
        .collect();
    let mut world = World::new(agents);
    world.keyed_endpoint("corn", openai(), &["gpt-5", "o4"]);
    world.harness.per_agent_models = false;
    assert_eq!(world.resolve().unwrap().len(), 3);
    // a harness with per-agent models gives each agent its own model
    world.agents[1].config.models = vec![matcher("o4")];
    world.harness.per_agent_models = true;
    let bindings = world.resolve().unwrap();
    let helper = bindings
        .iter()
        .find(|binding| binding.agent == world.agents[1].agent)
        .unwrap();
    assert_eq!(helper.model_id, "o4");
    // the bindings are sorted by agent id
    let mut sorted = bindings.clone();
    sorted.sort_by_key(|binding| binding.agent);
    assert_eq!(bindings, sorted);
}

/// Disabled ids are skipped on their endpoint's level and by the harness at every level
#[test]
fn disabled_models() {
    // a group endpoint disabling the model, and the runner's own endpoint serving it
    let mut world = World::new(vec![agent("triage", vec![matcher("latest")])]);
    let index = world.keyed_endpoint("corn", openai(), &["latest"]);
    world.endpoints[index]
        .meta
        .disabled_models
        .insert("latest".to_owned());
    let own = Uuid::new_v4();
    world.user.endpoints.insert(
        own,
        UserModelEndpoint::new("mine", compatible("https://mine.local/v1"))
            .model(crate::models::ModelCatalogEntryRequest::new("latest")),
    );
    // it resolves from the runner's endpoint instead
    let binding = world.binding();
    assert_eq!(binding.endpoint, ModelConfigRef::User(own));
    // with the runner's endpoint disabling it too, the error names both
    let mine = world.user.endpoints.get_mut(&own).unwrap();
    mine.disabled_models.insert("latest".to_owned());
    let error = world.mismatch();
    let disabled = error.agents[0]
        .candidates
        .iter()
        .filter(|candidate| candidate.reason == ModelRejectReason::DisabledOnEndpoint)
        .map(|candidate| candidate.level.unwrap())
        .collect::<Vec<ModelAuthLevel>>();
    assert!(disabled.contains(&ModelAuthLevel::Group));
    assert!(disabled.contains(&ModelAuthLevel::User));
    // the harness disabling it skips it once with no level
    world
        .user
        .endpoints
        .get_mut(&own)
        .unwrap()
        .disabled_models
        .clear();
    world.disabled.insert("latest".to_owned());
    let error = world.mismatch();
    let harness = error.agents[0]
        .candidates
        .iter()
        .filter(|candidate| candidate.reason == ModelRejectReason::DisabledByHarness)
        .collect::<Vec<_>>();
    assert_eq!(harness.len(), 1);
    assert_eq!(harness[0].level, None);
}

/// An empty list tries the runner's default profiles, then catalog models by level, endpoint
/// name, and model id, so renaming the endpoint that won changes the choice
#[test]
fn empty_list_choice() {
    // two group endpoints serving a usable model each
    let mut world = World::new(vec![agent("triage", Vec::new())]);
    let alpha = world.keyed_endpoint("alpha", openai(), &["b-model", "a-model"]);
    let beta = world.keyed_endpoint("beta", openai(), &["c-model"]);
    // the first endpoint by name and its first model by id wins
    let binding = world.binding();
    assert_eq!(
        binding.endpoint,
        ModelConfigRef::Group(world.endpoints[alpha].id)
    );
    assert_eq!(binding.model_id, "a-model");
    assert_eq!(binding.priority, None);
    // renaming the winner to sort last changes the choice
    world.endpoints[alpha].name = "zulu".to_owned();
    let binding = world.binding();
    assert_eq!(
        binding.endpoint,
        ModelConfigRef::Group(world.endpoints[beta].id)
    );
    // a default profile is tried first
    let pinned = profile(
        "pinned",
        world.endpoints[alpha].id,
        "b-model",
        ModelApi::OpenAiResponses,
    );
    world.user.default_profiles = vec![pinned.id];
    world.profiles.push(pinned.clone());
    let binding = world.binding();
    assert_eq!(binding.profile, Some(pinned.id));
    assert_eq!(binding.model_id, "b-model");
    assert_eq!(binding.api, ModelApi::OpenAiResponses);
}

/// Default profiles that are deleted, banned, outside the run group, or fail a requirement are
/// skipped with a reason each before the catalog is tried
#[test]
fn default_profiles_skipped() {
    // an agent needing a 300000 context window and an endpoint whose catalog meets it
    let mut needy = agent("triage", Vec::new());
    needy.config.model_requirements.min_context_window = Some(300_000);
    let mut world = World::new(vec![needy]);
    let index = world.keyed_endpoint("corn", openai(), &["big", "small"]);
    let id = world.endpoints[index].id;
    world.endpoints[index].meta.catalog[0].limits.context_window = Some(400_000);
    // a deleted profile, a banned one, one on an endpoint outside the run group, and a small one
    let deleted = Uuid::new_v4();
    let mut banned = profile("banned", id, "big", ModelApi::OpenAiChat);
    let (ban_id, banned_ban) = ban(ModelBanKind::Generic("no".to_owned()));
    banned.meta.bans.insert(ban_id, banned_ban);
    let outside = profile("outside", Uuid::new_v4(), "big", ModelApi::OpenAiChat);
    let small = profile("small", id, "small", ModelApi::OpenAiChat);
    world.user.default_profiles = vec![deleted, banned.id, outside.id, small.id];
    world.profiles.extend([banned, outside, small]);
    // the catalog model is chosen after every profile is skipped
    let binding = world.binding();
    assert_eq!(binding.model_id, "big");
    assert_eq!(binding.profile, None);
    // with the catalog unusable too, the error has a reason for each profile
    world.endpoints[index].meta.catalog[0].limits.context_window = Some(1);
    let error = world.mismatch();
    let reasons = reasons(&error);
    let no_endpoint = reasons
        .iter()
        .filter(|reason| **reason == ModelRejectReason::NoEndpoint)
        .count();
    // the two reserved levels, the deleted profile, and the profile outside the run group
    assert_eq!(no_endpoint, 4);
    assert!(
        reasons.contains(&ModelRejectReason::Banned(ModelBanKind::Generic(
            "no".to_owned()
        )))
    );
    assert!(
        reasons
            .iter()
            .any(|reason| matches!(reason, ModelRejectReason::RequirementNotMet(_)))
    );
}

/// The workflow's default models apply to agents with no list of their own
#[test]
fn workflow_default_models() {
    // one agent with no models and one with its own
    let mut world = World::new(vec![
        agent("defaulted", Vec::new()),
        agent("own", vec![matcher("claude-sonnet-4-5")]),
    ]);
    world.default_models = vec![matcher("gpt-5")];
    world.keyed_endpoint("corn-openai", openai(), &["gpt-5", "o4"]);
    world.keyed_endpoint(
        "corn-anthropic",
        anthropic("2023-06-01"),
        &["claude-sonnet-4-5"],
    );
    // the runner's default profile names another model, which the workflow list overrides
    let other = profile("other", world.endpoints[0].id, "o4", ModelApi::OpenAiChat);
    world.user.default_profiles = vec![other.id];
    world.profiles.push(other);
    let bindings = world.resolve().unwrap();
    let model = |agent: Uuid| {
        bindings
            .iter()
            .find(|binding| binding.agent == agent)
            .unwrap()
            .model_id
            .clone()
    };
    assert_eq!(model(world.agents[0].agent), "gpt-5");
    assert_eq!(model(world.agents[1].agent), "claude-sonnet-4-5");
    // a failure of the defaulted agent lists the workflow's matchers
    world.default_models = vec![matcher("missing")];
    let error = world.mismatch();
    assert_eq!(error.agents.len(), 1);
    assert!(
        error.agents[0].candidates.iter().all(|candidate| candidate
            .matcher
            .as_ref()
            .unwrap()
            .model_id
            == "missing")
    );
}

/// A matcher's provider, protocol, and API versions narrow the endpoints considered, and
/// matching endpoints in one level are tried in name order
#[test]
fn matcher_narrowing() {
    // one model served by an Anthropic endpoint and a Bedrock Mantle endpoint
    let mut world = World::new(vec![agent("triage", vec![matcher("claude-sonnet-4-5")])]);
    let first = world.keyed_endpoint(
        "a-anthropic",
        anthropic("2023-06-01"),
        &["claude-sonnet-4-5"],
    );
    let key = world.add_credential(credential(CredentialKind::AwsBearerToken));
    world.endpoints.push(endpoint(
        "b-mantle",
        bedrock(BedrockEndpointKind::Mantle),
        Some(key),
        &["claude-sonnet-4-5"],
    ));
    // with no narrowing the first by name wins
    assert_eq!(
        world.binding().endpoint,
        ModelConfigRef::Group(world.endpoints[first].id)
    );
    // a Bedrock provider picks only the Mantle endpoint
    world.agents[0].config.models[0].provider = Some(ModelProviderKind::Bedrock);
    let binding = world.binding();
    assert_eq!(
        binding.endpoint,
        ModelConfigRef::Group(world.endpoints[1].id)
    );
    assert_eq!(binding.api, ModelApi::AnthropicMessages);
    // a protocol no endpoint serves the model with finds no endpoint
    let mut world2 = World::new(vec![agent(
        "triage",
        vec![ModelMatcher {
            model_id: "claude-sonnet-4-5".to_owned(),
            api: Some(ModelApi::OpenAiChat),
            ..ModelMatcher::default()
        }],
    )]);
    world2.keyed_endpoint("anthropic", anthropic("2023-06-01"), &["claude-sonnet-4-5"]);
    assert!(reasons(&world2.mismatch()).contains(&ModelRejectReason::NoEndpoint));
    // an API version matches the endpoint with that version and skips another
    let mut world3 = World::new(vec![agent(
        "triage",
        vec![ModelMatcher {
            model_id: "claude".to_owned(),
            api_versions: ["2023-06-01".to_owned()].into_iter().collect(),
            ..ModelMatcher::default()
        }],
    )]);
    world3.keyed_endpoint("a-new", anthropic("2024-01-01"), &["claude"]);
    let right = world3.keyed_endpoint("b-old", anthropic("2023-06-01"), &["claude"]);
    assert_eq!(
        world3.binding().endpoint,
        ModelConfigRef::Group(world3.endpoints[right].id)
    );
    // an Azure legacy endpoint compares its api_version
    let mut world4 = World::new(vec![agent(
        "triage",
        vec![ModelMatcher {
            model_id: "deploy".to_owned(),
            api_versions: ["2024-10-21".to_owned()].into_iter().collect(),
            ..ModelMatcher::default()
        }],
    )]);
    world4.keyed_endpoint(
        "azure",
        ModelProvider::AzureOpenAi {
            endpoint: "https://res.openai.azure.com".to_owned(),
            api_mode: crate::models::AzureApiMode::Legacy {
                api_version: "2024-10-21".to_owned(),
            },
            entra_scope: None,
        },
        &["deploy"],
    );
    assert_eq!(world4.binding().model_id, "deploy");
    // a Gemini endpoint compares its written api_version
    let mut world5 = World::new(vec![agent(
        "triage",
        vec![ModelMatcher {
            model_id: "gemini".to_owned(),
            api_versions: ["V1".to_owned()].into_iter().collect(),
            ..ModelMatcher::default()
        }],
    )]);
    world5
        .harness
        .model_apis
        .insert(ModelApi::GeminiGenerateContent);
    world5.keyed_endpoint(
        "gemini",
        ModelProvider::Gemini {
            base_url: None,
            api_version: crate::models::GeminiApiVersion::V1Beta,
        },
        &["gemini"],
    );
    assert!(world5.resolve().is_err());
    world5.agents[0].config.models[0].api_versions = ["V1Beta".to_owned()].into_iter().collect();
    assert_eq!(world5.binding().model_id, "gemini");
}

/// A profile's limits and capabilities replace its catalog entry's, and without a profile the
/// entry's limits and the provider's first protocol are used
#[test]
fn profiles_and_catalog_entries() {
    // an agent needing a 150000 context window and an entry with only 100000
    let mut needy = agent("triage", vec![matcher("gpt-5")]);
    needy.config.model_requirements.min_context_window = Some(150_000);
    let mut world = World::new(vec![needy]);
    let index = world.keyed_endpoint("corn", openai(), &["gpt-5"]);
    let id = world.endpoints[index].id;
    world.endpoints[index].meta.catalog[0].limits.context_window = Some(100_000);
    // the entry alone fails
    assert!(world.resolve().is_err());
    // a profile with a larger window and its own protocol and capabilities passes
    let mut big = profile("big", id, "gpt-5", ModelApi::OpenAiResponses);
    big.meta.limits = Some(ModelLimits {
        context_window: Some(200_000),
        max_output_tokens: Some(16_000),
        source: LimitsSource::Manual,
    });
    big.meta.capabilities = Some(ModelCapabilities {
        vision: true,
        ..ModelCapabilities::default()
    });
    world.profiles.push(big.clone());
    world.agents[0]
        .config
        .model_requirements
        .capabilities
        .vision = true;
    let binding = world.binding();
    assert_eq!(binding.profile, Some(big.id));
    assert_eq!(binding.api, ModelApi::OpenAiResponses);
    // without a profile each provider's first protocol is used
    for (provider, api) in [
        (openai(), ModelApi::OpenAiChat),
        (
            bedrock(BedrockEndpointKind::Mantle),
            ModelApi::AnthropicMessages,
        ),
        (
            bedrock(BedrockEndpointKind::Runtime),
            ModelApi::BedrockConverse,
        ),
    ] {
        let mut world = World::new(vec![agent("triage", vec![matcher("m")])]);
        let kind = match provider {
            ModelProvider::Bedrock { .. } => CredentialKind::AwsBearerToken,
            _ => CredentialKind::ApiKey,
        };
        let cred = world.add_credential(credential(kind));
        world
            .endpoints
            .push(endpoint("e", provider, Some(cred), &["m"]));
        assert_eq!(world.binding().api, api);
    }
}

/// A profile whose protocol its endpoint's provider doesn't serve, as a provider change can leave
/// behind, is never bound, whether the agent names its model or it's one of the runner's default
/// profiles
#[test]
fn profiles_need_a_protocol_their_provider_serves() {
    // an Anthropic endpoint listing m, and a profile that calls m over OpenAI chat
    let mut world = World::new(vec![agent("triage", vec![matcher("m")])]);
    let index = world.keyed_endpoint("corn", anthropic("2023-06-01"), &["m"]);
    let id = world.endpoints[index].id;
    let stale = profile("stale", id, "m", ModelApi::OpenAiChat);
    world.profiles.push(stale.clone());
    // the catalog entry is bound with the provider's protocol instead of the profile
    let binding = world.binding();
    assert_eq!(binding.profile, None);
    assert_eq!(binding.api, ModelApi::AnthropicMessages);
    // the same profile as a default profile is skipped as one the endpoint doesn't serve
    world.agents[0].config.models.clear();
    world.user.default_profiles = vec![stale.id];
    let binding = world.binding();
    assert_eq!(binding.profile, None);
    assert_eq!(binding.api, ModelApi::AnthropicMessages);
    // with no catalog entry left to fall back on, nothing is bound
    world.endpoints[index].meta.catalog.clear();
    let error = world.mismatch();
    assert!(reasons(&error).contains(&ModelRejectReason::NoEndpoint));
}

/// The reserved levels are always skipped, and the user level never uses a credential the
/// runner doesn't have
#[test]
fn reserved_and_user_levels() {
    // a group endpoint without a group credential
    let mut world = World::new(vec![agent("triage", vec![matcher("gpt-5")])]);
    world
        .endpoints
        .push(endpoint("corn", openai(), None, &["gpt-5"]));
    let error = world.mismatch();
    let candidates = &error.agents[0].candidates;
    // the reserved levels come first with no endpoint
    assert_eq!(candidates[0].level, Some(ModelAuthLevel::Harness));
    assert_eq!(candidates[0].reason, ModelRejectReason::NoEndpoint);
    assert_eq!(candidates[1].level, Some(ModelAuthLevel::Workflow));
    assert_eq!(candidates[1].reason, ModelRejectReason::NoEndpoint);
    // the group and user levels have no credential
    assert_eq!(candidates[2].reason, ModelRejectReason::NoCredential);
    assert_eq!(candidates[3].level, Some(ModelAuthLevel::User));
    assert_eq!(candidates[3].reason, ModelRejectReason::NoCredential);
    // a credential the runner holds for that endpoint resolves at the user level
    let id = world.endpoints[0].id;
    let cred = world.user_key(id);
    let binding = world.binding();
    assert_eq!(binding.level, ModelAuthLevel::User);
    assert_eq!(binding.credential, Some(ModelConfigRef::User(cred)));
}

/// Keyless gateways resolve without a credential, at the level that owns the endpoint, and
/// other providers without a credential are refused
#[test]
fn keyless_endpoints() {
    // a group OpenAI-compatible endpoint with no credential
    let mut world = World::new(vec![agent("triage", vec![matcher("llama")])]);
    world.endpoints.push(endpoint(
        "gateway",
        compatible("https://gw.local/v1"),
        None,
        &["llama"],
    ));
    let binding = world.binding();
    assert_eq!(binding.level, ModelAuthLevel::Group);
    assert_eq!(binding.credential, None);
    // the runner's own keyless endpoint resolves at the user level
    let mut world = World::new(vec![agent("triage", vec![matcher("llama")])]);
    let own = Uuid::new_v4();
    world.user.endpoints.insert(
        own,
        UserModelEndpoint::new("mine", compatible("https://mine.local/v1"))
            .model(crate::models::ModelCatalogEntryRequest::new("llama")),
    );
    let binding = world.binding();
    assert_eq!(binding.level, ModelAuthLevel::User);
    assert_eq!(binding.credential, None);
    // OpenAI, Anthropic, and Bedrock without a credential get NoCredential at each level
    for provider in [
        openai(),
        anthropic("2023-06-01"),
        bedrock(BedrockEndpointKind::Runtime),
    ] {
        let mut world = World::new(vec![agent("triage", vec![matcher("m")])]);
        world.endpoints.push(endpoint("e", provider, None, &["m"]));
        let reasons = reasons(&world.mismatch());
        assert_eq!(
            reasons
                .iter()
                .filter(|reason| **reason == ModelRejectReason::NoCredential)
                .count(),
            2
        );
    }
}

/// The levels tried are those both the agent's and the workflow's auth sources allow
#[test]
fn auth_sources_intersect() {
    // a model reachable at the group and user levels
    let mut world = World::new(vec![agent("triage", vec![matcher("gpt-5")])]);
    let index = world.keyed_endpoint("corn", openai(), &["gpt-5"]);
    let id = world.endpoints[index].id;
    world.user_key(id);
    let agent = world.agents[0].agent;
    let levels = |levels: &[ModelAuthLevel]| ModelAuthSource {
        levels: levels.iter().copied().collect(),
    };
    // the agent allows group and user and the workflow only user
    world.agent_auth.insert(
        agent,
        levels(&[ModelAuthLevel::Group, ModelAuthLevel::User]),
    );
    world.workflow_auth = levels(&[ModelAuthLevel::User]);
    assert_eq!(world.binding().level, ModelAuthLevel::User);
    // the agent allows only group and the workflow only user, so every level is refused
    world
        .agent_auth
        .insert(agent, levels(&[ModelAuthLevel::Group]));
    let error = world.mismatch();
    assert!(
        reasons(&error)
            .iter()
            .all(|reason| *reason == ModelRejectReason::LevelNotAllowed)
    );
    assert_eq!(reasons(&error).len(), 4);
    // both empty try every level
    world.agent_auth.clear();
    world.workflow_auth = ModelAuthSource::default();
    assert_eq!(world.binding().level, ModelAuthLevel::Group);
}

/// With several agents, one mismatch per unresolved agent lists every candidate in the order
/// tried, and the resolved agent has none
#[test]
fn several_agent_mismatches() {
    // three agents, two of which want models no endpoint serves
    let mut world = World::new(vec![
        agent("first", vec![matcher("gone"), matcher("missing")]),
        agent("second", vec![matcher("gpt-5")]),
        agent("third", vec![matcher("absent")]),
    ]);
    world.keyed_endpoint("corn", openai(), &["gpt-5"]);
    let error = world.mismatch();
    // one mismatch for each unresolved agent in agent order
    let names = error
        .agents
        .iter()
        .map(|agent| agent.name.as_str())
        .collect::<Vec<&str>>();
    assert_eq!(names, ["first", "third"]);
    // every candidate in the order tried: four levels for each model
    let first = &error.agents[0].candidates;
    assert_eq!(first.len(), 8);
    assert_eq!(first[0].matcher.as_ref().unwrap().model_id, "gone");
    assert_eq!(first[4].matcher.as_ref().unwrap().model_id, "missing");
    // the error round trips through serde
    let json = serde_json::to_string(&error).unwrap();
    let back: ModelResolutionError = serde_json::from_str(&json).unwrap();
    assert_eq!(back, error);
}

/// A deleted access key of an assumed role leaves the role unusable with a broken reference
#[test]
fn broken_role_reference_is_banned() {
    // an assumed role whose access key is gone
    let mut world = World::new(vec![agent("triage", vec![matcher("claude")])]);
    let source = Uuid::new_v4();
    let role = world.add_credential(credential(CredentialKind::AwsAssumeRole {
        role_arn: "arn:aws:iam::1:role/r".to_owned(),
        external_id: None,
        session_name: None,
        duration_seconds: None,
        source: AwsRoleSource::AccessKey(source),
    }));
    world.endpoints.push(endpoint(
        "bedrock",
        bedrock(BedrockEndpointKind::Runtime),
        Some(role),
        &["claude"],
    ));
    // the role is skipped as banned for its missing source
    let error = world.mismatch();
    assert!(
        reasons(&error).contains(&ModelRejectReason::Banned(ModelBanKind::BrokenReference(
            source
        )))
    );
    // a role whose access key is there resolves
    world.credentials.insert(
        source,
        credential(CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".to_owned(),
        }),
    );
    assert_eq!(
        world.binding().credential,
        Some(ModelConfigRef::Group(role))
    );
}

/// An endpoint off the deployment's allowed list is skipped at run creation
#[test]
fn whitelist_checked_at_resolution() {
    // two endpoints with only the second allowed
    let mut world = World::new(vec![agent("triage", vec![matcher("gpt-5")])]);
    world.keyed_endpoint("a-gone", compatible("https://gone.local/v1"), &["gpt-5"]);
    let kept = world.keyed_endpoint("b-kept", compatible("https://kept.local/v1"), &["gpt-5"]);
    let mut conf = ModelsConf::default();
    conf.allowed_endpoints.insert(AllowedEndpoint::for_provider(
        &world.endpoints[kept].meta.provider,
    ));
    world.whitelist = Some(conf);
    // the first is refused and the second chosen
    assert_eq!(
        world.binding().endpoint,
        ModelConfigRef::Group(world.endpoints[kept].id)
    );
    world.whitelist = Some(ModelsConf::default());
    assert!(reasons(&world.mismatch()).contains(&ModelRejectReason::NotAllowedByDeployment));
}

/// Rejections from an empty list have no matcher, harness disables have no level, missing
/// endpoints have no endpoint, and every other rejection has both
#[test]
fn rejection_fields() {
    // an empty list with a banned catalog model, and a harness disabling another
    let mut world = World::new(vec![agent("triage", Vec::new())]);
    let index = world.keyed_endpoint("corn", openai(), &["a-banned", "b-disabled"]);
    let (id, banned) = ban(ModelBanKind::ModelRemoved);
    world.endpoints[index].meta.catalog[0]
        .bans
        .insert(id, banned);
    world.disabled.insert("b-disabled".to_owned());
    let error = world.mismatch();
    for candidate in &error.agents[0].candidates {
        // every candidate from an empty list has no matcher
        assert!(candidate.matcher.is_none());
        match candidate.reason {
            ModelRejectReason::DisabledByHarness => assert!(candidate.level.is_none()),
            ModelRejectReason::NoEndpoint => {
                assert!(candidate.endpoint.is_none());
                assert!(candidate.level.is_some());
            }
            ModelRejectReason::LevelNotAllowed => assert!(candidate.level.is_some()),
            _ => {
                assert!(candidate.level.is_some());
                assert!(candidate.endpoint.is_some());
            }
        }
    }
    assert!(reasons(&error).contains(&ModelRejectReason::DisabledByHarness));
    assert!(reasons(&error).contains(&ModelRejectReason::Banned(ModelBanKind::ModelRemoved)));
}

/// The mismatch text has one line per agent and candidate naming the matcher, level, endpoint,
/// and reason, and the rendered text lists every allowed model with a reason per level
#[test]
fn mismatch_text() {
    // a banned group endpoint and no user credential
    let mut world = World::new(vec![agent(
        "Reverse Engineer",
        vec![matcher("claude-sonnet-4-5")],
    )]);
    let index = world.keyed_endpoint(
        "Corn Anthropic",
        anthropic("2023-06-01"),
        &["claude-sonnet-4-5"],
    );
    let (id, banned) = ban(ModelBanKind::CredentialRejected {
        status: 401,
        message: "invalid x-api-key".to_owned(),
    });
    world.endpoints[index].meta.bans.insert(id, banned);
    let error = world.mismatch();
    // one display line per candidate, each naming the agent, model, level, and reason
    let text = error.to_string();
    let lines = text.lines().collect::<Vec<&str>>();
    assert_eq!(lines.len(), error.agents[0].candidates.len());
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("Reverse Engineer: claude-sonnet-4-5 at "))
    );
    assert!(lines[2].contains("at group on endpoint"));
    assert!(lines[2].contains("banned (CredentialRejected: 401 invalid x-api-key)"));
    // the rendered text names the endpoint and group and has a line per level
    let mut names = MismatchNames {
        instance: "Malware Triage#r4".to_owned(),
        group: "corn".to_owned(),
        harness: "PalHarness v0.0.1".to_owned(),
        endpoints: BTreeMap::new(),
    };
    names
        .endpoints
        .insert(world.endpoints[index].id, "Corn Anthropic".to_owned());
    let rendered = error.render(&names);
    assert!(rendered.starts_with(
        "No usable model for agent \"Reverse Engineer\" in Malware Triage#r4.\nAllowed models, in priority order:\n  1. claude-sonnet-4-5\n     - harness, workflow: reserved, no auth source yet\n     - group corn: endpoint \"Corn Anthropic\" is banned (CredentialRejected: 401 invalid x-api-key)\n     - user: you have no usable credential for endpoint \"Corn Anthropic\""
    ));
    assert!(rendered.contains("ask an owner or manager of corn"));
}

/// Build a binding of an agent to a group endpoint and credential
///
/// # Arguments
///
/// * `agent` - The agent
/// * `endpoint` - The endpoint
/// * `credential` - The credential
fn binding(
    agent: Uuid,
    endpoint: Uuid,
    credential: Option<ModelConfigRef>,
) -> ReactionModelBinding {
    ReactionModelBinding {
        agent,
        priority: Some(0),
        level: ModelAuthLevel::Group,
        endpoint: ModelConfigRef::Group(endpoint),
        model_id: "m".to_owned(),
        api: ModelApi::OpenAiChat,
        profile: None,
        credential,
    }
}

/// Run models built from bindings carry each credential's secrets, files for the kinds SDKs
/// read from files, and the entry agent's values under exactly the harness's env names
#[test]
#[allow(clippy::too_many_lines)]
fn run_models_from_bindings() {
    // an assumed role and its access key, a GCP JSON, an Entra certificate, and an API key
    let source = Uuid::new_v4();
    let role = Uuid::new_v4();
    let gcp = Uuid::new_v4();
    let cert = Uuid::new_v4();
    let key = Uuid::new_v4();
    let mut credentials = HashMap::new();
    credentials.insert(
        source,
        credential(CredentialKind::AwsAccessKey {
            access_key_id: "AKIAEXAMPLE".to_owned(),
        }),
    );
    credentials.insert(
        role,
        ModelCredentialEntity {
            secrets: CredentialSecrets::default(),
            ..credential(CredentialKind::AwsAssumeRole {
                role_arn: "arn:aws:iam::1:role/r".to_owned(),
                external_id: Some("ext".to_owned()),
                session_name: None,
                duration_seconds: None,
                source: AwsRoleSource::AccessKey(source),
            })
        },
    );
    credentials.insert(
        gcp,
        credential(CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        }),
    );
    credentials.insert(
        cert,
        credential(CredentialKind::AzureEntraClient {
            tenant_id: "t".to_owned(),
            client_id: "c".to_owned(),
            auth: AzureClientAuth::Certificate,
        }),
    );
    credentials.insert(key, credential(CredentialKind::ApiKey));
    // one endpoint per agent
    let bedrock_ep = endpoint(
        "bedrock",
        bedrock(BedrockEndpointKind::Runtime),
        Some(role),
        &["m"],
    );
    let keyless_ep = endpoint("gw", compatible("https://gw.local/v1"), None, &["m"]);
    let openai_ep = endpoint("openai", openai(), Some(key), &["m"]);
    let endpoints = [&bedrock_ep, &keyless_ep, &openai_ep]
        .iter()
        .map(|record| (record.id, record.meta.clone()))
        .collect::<HashMap<Uuid, ModelEndpointEntity>>();
    let (lead, gw, helper) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let bindings = vec![
        binding(lead, bedrock_ep.id, Some(ModelConfigRef::Group(role))),
        binding(gw, keyless_ep.id, None),
        binding(helper, openai_ep.id, Some(ModelConfigRef::Group(key))),
    ];
    // the harness exports the AWS values, renaming the config file path
    let env = HarnessEnv {
        export: [
            HarnessEnvVar::AwsConfigFile,
            HarnessEnvVar::AwsProfile,
            HarnessEnvVar::AwsAccessKeyId,
            HarnessEnvVar::OpenAiApiKey,
        ]
        .into_iter()
        .collect(),
        names: [(HarnessEnvVar::AwsConfigFile, "MY_AWS_CONFIG".to_owned())]
            .into_iter()
            .collect(),
    };
    let profiles = HashMap::new();
    let user = UserModelSettings::default();
    let whitelist = allowing(endpoints.values().map(|endpoint| &endpoint.provider));
    let sources = DeliverySources {
        endpoints: &endpoints,
        profiles: &profiles,
        credentials: &credentials,
        user: &user,
        whitelist: &whitelist,
        harness_env: Some(&env),
        models_file: "/tmp/thorium/ephemeral/models.json",
    };
    let models = build_run_models(&bindings, lead, &sources).unwrap();
    // the role carries its metadata, no secret of its own, and its access key as its source
    let role_cred = models.models.agents[&lead].credential.as_ref().unwrap();
    assert!(role_cred.secrets.primary.is_none());
    assert_eq!(role_cred.file.as_deref(), Some("aws-config"));
    let source_cred = role_cred.source.as_ref().unwrap();
    assert_eq!(source_cred.primary().unwrap().expose(), SECRET);
    let config = role_cred.file_contents().unwrap();
    assert!(config.contains("role_arn = arn:aws:iam::1:role/r"));
    assert!(config.contains("source_profile = thorium-source"));
    assert!(config.contains(&format!("aws_secret_access_key = {SECRET}")));
    // the entry agent's values go under exactly the harness's names
    let names = models.env.keys().cloned().collect::<Vec<String>>();
    assert_eq!(names, ["AWS_PROFILE", "MY_AWS_CONFIG"]);
    assert_eq!(
        models.env["MY_AWS_CONFIG"].expose(),
        "/tmp/thorium/credentials/aws-config"
    );
    // a keyless binding has no credential, and its values would carry no secret
    assert!(models.models.agents[&gw].credential.is_none());
    let keyless_env = RunModels::harness_env(
        &HarnessEnv {
            export: [HarnessEnvVar::OpenAiApiKey].into_iter().collect(),
            names: BTreeMap::new(),
        },
        &models.models.agents[&gw],
        "/tmp/thorium/ephemeral/models.json",
    );
    assert!(keyless_env.is_empty());
    // the GCP JSON and Entra certificate are written as files, and an API key isn't
    let gcp_cred = crate::models::HarnessCredential::new(
        ModelConfigRef::Group(gcp),
        credentials[&gcp].kind.clone(),
        credentials[&gcp].secrets.clone(),
        None,
    );
    assert_eq!(gcp_cred.file, Some(format!("{gcp}.json")));
    let cert_cred = crate::models::HarnessCredential::new(
        ModelConfigRef::Group(cert),
        credentials[&cert].kind.clone(),
        credentials[&cert].secrets.clone(),
        None,
    );
    assert_eq!(cert_cred.file, Some(format!("{cert}.pem")));
    assert!(
        models.models.agents[&helper]
            .credential
            .as_ref()
            .unwrap()
            .file
            .is_none()
    );
}

/// A banned or deleted credential can't be delivered, with a refusal naming the ban's kind but
/// not its detail, while an endpoint banned by hand can
#[test]
fn run_models_refuse_banned_credentials() {
    // one endpoint with a banned endpoint ban and a clean API key
    let key = Uuid::new_v4();
    let mut credentials = HashMap::from([(key, credential(CredentialKind::ApiKey))]);
    let mut record = endpoint("openai", openai(), Some(key), &["m"]);
    let (id, endpoint_ban) = ban(ModelBanKind::Generic("down".to_owned()));
    record.meta.bans.insert(id, endpoint_ban);
    let endpoints = HashMap::from([(record.id, record.meta.clone())]);
    let agent = Uuid::new_v4();
    let bindings = vec![binding(agent, record.id, Some(ModelConfigRef::Group(key)))];
    let profiles = HashMap::new();
    let user = UserModelSettings::default();
    let whitelist = allowing([&record.meta.provider]);
    // an endpoint ban by hand doesn't stop delivery
    {
        let sources = DeliverySources {
            endpoints: &endpoints,
            profiles: &profiles,
            credentials: &credentials,
            user: &user,
            whitelist: &whitelist,
            harness_env: None,
            models_file: "/models.json",
        };
        let models = build_run_models(&bindings, agent, &sources).unwrap();
        // secrets travel as plain values
        let json = serde_json::to_string(&models).unwrap();
        assert!(json.contains(SECRET));
    }
    // a credential ban does, and its refusal names only the ban's kind, never the provider's
    // message, which can echo the key
    let (id, cred_ban) = ban(ModelBanKind::CredentialRejected {
        status: 401,
        message: "invalid x-api-key sk-...wxyz".to_owned(),
    });
    credentials.get_mut(&key).unwrap().bans.insert(id, cred_ban);
    {
        let sources = DeliverySources {
            endpoints: &endpoints,
            profiles: &profiles,
            credentials: &credentials,
            user: &user,
            whitelist: &whitelist,
            harness_env: None,
            models_file: "/models.json",
        };
        let refused = build_run_models(&bindings, agent, &sources).unwrap_err();
        assert_eq!(
            refused,
            format!("model credential {key} is banned (CredentialRejected)")
        );
        assert!(!refused.contains("401"));
        assert!(!refused.contains("wxyz"));
    }
    // and so does a deleted one
    credentials.clear();
    let sources = DeliverySources {
        endpoints: &endpoints,
        profiles: &profiles,
        credentials: &credentials,
        user: &user,
        whitelist: &whitelist,
        harness_env: None,
        models_file: "/models.json",
    };
    assert!(build_run_models(&bindings, agent, &sources).is_err());
}

/// Build the models a test world's run gets from its bindings
///
/// # Arguments
///
/// * `world` - The world whose configuration the bindings name
/// * `whitelist` - The deployment's allowed endpoint list at delivery time
/// * `bindings` - The run's recorded bindings
fn deliver(
    world: &World,
    whitelist: &ModelsConf,
    bindings: &[ReactionModelBinding],
) -> Result<RunModels, String> {
    // the group endpoints by id, as the run models read finds them
    let endpoints = world
        .endpoints
        .iter()
        .map(|record| (record.id, record.meta.clone()))
        .collect::<HashMap<Uuid, ModelEndpointEntity>>();
    let profiles = HashMap::new();
    let sources = DeliverySources {
        endpoints: &endpoints,
        profiles: &profiles,
        credentials: &world.credentials,
        user: &world.user,
        whitelist,
        harness_env: None,
        models_file: "/models.json",
    };
    build_run_models(bindings, bindings[0].agent, &sources)
}

/// A queued run whose endpoint the deployment stops allowing, by a `NotAllowedByDeployment` ban
/// or by leaving the allowed list before any scan bans it, can't get its models, whether the
/// endpoint is a group endpoint or the runner's own; an allowed endpoint, even one banned by
/// hand, is still delivered, and a retry binds the next candidate the deployment allows
#[test]
#[allow(clippy::too_many_lines)]
fn run_models_refuse_endpoints_the_deployment_no_longer_allows() {
    // model one on a group endpoint, and model two on the runner's own endpoint as a fallback
    let mut world = World::new(vec![agent("triage", vec![matcher("one"), matcher("two")])]);
    let index = world.keyed_endpoint("corn-gw", compatible("https://corn.local/v1"), &["one"]);
    let group_id = world.endpoints[index].id;
    let own = Uuid::new_v4();
    world.user.endpoints.insert(
        own,
        UserModelEndpoint::new("mine", compatible("https://mine.local/v1"))
            .model(crate::models::ModelCatalogEntryRequest::new("two")),
    );
    world.user_key(own);
    let both = allowing([
        &world.endpoints[index].meta.provider,
        &world.user.endpoints[&own].provider,
    ]);
    world.whitelist = Some(both.clone());
    // the run is queued with model one on the group endpoint
    let queued = world.resolve().unwrap();
    assert_eq!(queued[0].endpoint, ModelConfigRef::Group(group_id));
    // an allowed endpoint is delivered with its secret
    let models = deliver(&world, &both, &queued).unwrap();
    assert!(serde_json::to_string(&models).unwrap().contains(SECRET));
    // an endpoint ban by hand still leaves it deliverable
    let (generic_id, generic) = ban(ModelBanKind::Generic("maintenance".to_owned()));
    world.endpoints[index].meta.bans.insert(generic_id, generic);
    deliver(&world, &both, &queued).unwrap();
    world.endpoints[index].meta.bans.clear();
    // a scan's ban after the run was queued stops delivery
    let (scan_id, scan_ban) = ban(ModelBanKind::NotAllowedByDeployment);
    world.endpoints[index].meta.bans.insert(scan_id, scan_ban);
    assert_eq!(
        deliver(&world, &both, &queued).unwrap_err(),
        format!("model endpoint {group_id} is banned (NotAllowedByDeployment)")
    );
    world.endpoints[index].meta.bans.clear();
    // so does leaving the allowed list, before any scan bans the endpoint
    let only_own = allowing([&world.user.endpoints[&own].provider]);
    assert_eq!(
        deliver(&world, &only_own, &queued).unwrap_err(),
        format!("model endpoint {group_id} is no longer on the deployment's allowed endpoint list")
    );
    // a retry resolves again and binds model two on the runner's own endpoint
    world.whitelist = Some(only_own.clone());
    let retried = world.resolve().unwrap();
    assert_eq!(retried[0].endpoint, ModelConfigRef::User(own));
    assert_eq!(retried[0].model_id, "two");
    deliver(&world, &only_own, &retried).unwrap();
    // the runner's own endpoint is refused the same ways
    let (own_ban_id, own_ban) = ban(ModelBanKind::NotAllowedByDeployment);
    world
        .user
        .endpoints
        .get_mut(&own)
        .unwrap()
        .bans
        .insert(own_ban_id, own_ban);
    assert_eq!(
        deliver(&world, &only_own, &retried).unwrap_err(),
        format!("the runner's model endpoint {own} is banned (NotAllowedByDeployment)")
    );
    world.user.endpoints.get_mut(&own).unwrap().bans.clear();
    assert_eq!(
        deliver(&world, &ModelsConf::default(), &retried).unwrap_err(),
        format!(
            "the runner's model endpoint {own} is no longer on the deployment's allowed endpoint list"
        )
    );
    // allowing the group endpoint again delivers the queued run's models
    deliver(&world, &both, &queued).unwrap();
}

/// A models file entry without capabilities loads with every capability false, and one with
/// them keeps them
#[test]
fn harness_model_entry_capabilities() {
    // an entry written without capabilities
    let json = serde_json::json!({
        "provider": {"OpenAi": {"base_url": null, "organization": null, "project": null}},
        "model_id": "gpt-5",
        "api": "OpenAiChat",
        "credential": null,
    });
    let entry: HarnessModelEntry = serde_json::from_value(json).unwrap();
    assert_eq!(entry.capabilities, ModelCapabilities::default());
    // an entry with capabilities keeps them
    let mut with = entry.clone();
    with.capabilities.strict_tools = true;
    let back: HarnessModelEntry =
        serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
    assert!(back.capabilities.strict_tools);
}
