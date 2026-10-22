//! The result of resolving a run's models and the values a run's pods get from it
//!
//! Resolution either picks one model for every agent of a workflow run or fails with a
//! [`ModelResolutionError`] that says why each candidate was skipped. A resolved run's models are
//! delivered to its harness as [`RunModels`]: every agent's [`HarnessModelEntry`] with its
//! credential's secrets, and the entry agent's values under the harness's env names.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use uuid::Uuid;

use super::{
    AzureApiMode, AzureClientAuth, CredentialKind, CredentialSecrets, CredentialType, ModelApi,
    ModelAuthLevel, ModelBanKind, ModelCapabilities, ModelConfigRef, ModelLimits, ModelMatcher,
    ModelParameters, ModelProvider, ModelSecret, VertexLocation, VertexMultiRegion,
};
use crate::models::{HarnessEnv, HarnessEnvVar};

/// The directory the thorium-agent writes credential files to in a harness's pod
pub const CREDENTIALS_DIR: &str = "/tmp/thorium/credentials";

/// The name of the AWS config file the thorium-agent writes for an assumed role
pub const AWS_CONFIG_FILE: &str = "aws-config";

/// The AWS profile in the AWS config file that assumes the role
pub const AWS_PROFILE: &str = "thorium";

/// The AWS profile in the AWS config file that holds the access key the role is assumed with
const AWS_SOURCE_PROFILE: &str = "thorium-source";

/// Why no model could be resolved for one or more agents of a run
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelResolutionError {
    /// Every agent that has no usable model
    pub agents: Vec<AgentModelMismatch>,
}

/// An agent with no usable model and why each candidate failed
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AgentModelMismatch {
    /// The agent's id
    pub agent: Uuid,
    /// The agent's name
    pub name: String,
    /// Every candidate tried, in the order tried
    pub candidates: Vec<ModelCandidateRejection>,
}

/// One candidate model that could not be used and why
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelCandidateRejection {
    /// The matcher being tried (unset when the agent's effective list is empty)
    pub matcher: Option<ModelMatcher>,
    /// The level being tried (unset when the reason applies to every level)
    pub level: Option<ModelAuthLevel>,
    /// The endpoint being tried, if one was found
    pub endpoint: Option<ModelConfigRef>,
    /// Why this candidate could not be used
    pub reason: ModelRejectReason,
}

/// Why a candidate model could not be used
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelRejectReason {
    /// The agent's or workflow's auth source doesn't allow this level
    LevelNotAllowed,
    /// No endpoint at this level serves the model
    NoEndpoint,
    /// The endpoint, catalog model, profile, or credential is banned
    Banned(ModelBanKind),
    /// The endpoint needs a credential and has no usable one at this level
    NoCredential,
    /// The model misses a requirement (which one)
    RequirementNotMet(String),
    /// The harness can't call this protocol
    ApiNotSupported(ModelApi),
    /// The harness can't use this credential kind
    CredentialNotSupported(CredentialType),
    /// The endpoint isn't on the deployment's allowed endpoint list
    NotAllowedByDeployment,
    /// The harness runs every agent on one model, and this agent resolved to a different one
    MixedModelsUnsupported,
    /// The endpoint at this level lists the model id in its `disabled_models`
    DisabledOnEndpoint,
    /// The run's harness lists the model id in its `disabled_models`
    DisabledByHarness,
}

/// The names a resolution error is rendered with
#[derive(Debug, Clone, Default)]
pub struct MismatchNames {
    /// The name of the workflow instance the run is of
    pub instance: String,
    /// The name of the run group
    pub group: String,
    /// The name of the entry agent's harness
    pub harness: String,
    /// The names of the endpoints a rejection may name, by id
    pub endpoints: BTreeMap<Uuid, String>,
}

/// Get the name of a level as the mismatch text writes it
///
/// # Arguments
///
/// * `level` - The level
fn level_name(level: ModelAuthLevel) -> &'static str {
    // name each level in lowercase
    match level {
        ModelAuthLevel::Harness => "harness",
        ModelAuthLevel::Workflow => "workflow",
        ModelAuthLevel::Group => "group",
        ModelAuthLevel::User => "user",
    }
}

/// Get the id an endpoint or credential reference points at
///
/// # Arguments
///
/// * `config` - The reference
fn config_id(config: ModelConfigRef) -> Uuid {
    // both kinds of reference hold one id
    match config {
        ModelConfigRef::Group(id) | ModelConfigRef::User(id) => id,
    }
}

/// Describe a matcher: its model id and any field that narrows it
///
/// # Arguments
///
/// * `matcher` - The matcher to describe
fn matcher_text(matcher: &ModelMatcher) -> String {
    // collect what narrows the model id
    let mut narrows = Vec::new();
    if let Some(provider) = &matcher.provider {
        narrows.push(format!("provider {provider}"));
    }
    if let Some(api) = &matcher.api {
        narrows.push(format!("api {api:?}"));
    }
    if !matcher.api_versions.is_empty() {
        let versions = matcher
            .api_versions
            .iter()
            .cloned()
            .collect::<Vec<String>>()
            .join(", ");
        narrows.push(format!("api versions {versions}"));
    }
    // a bare model id needs no parentheses
    if narrows.is_empty() {
        matcher.model_id.clone()
    } else {
        format!("{} ({})", matcher.model_id, narrows.join(", "))
    }
}

impl ModelCandidateRejection {
    /// Describe the endpoint this rejection names, by name when it's known
    ///
    /// # Arguments
    ///
    /// * `names` - The known endpoint names
    fn endpoint_text(&self, names: &BTreeMap<Uuid, String>) -> Option<String> {
        // name the endpoint, falling back to its id
        self.endpoint.map(|endpoint| {
            let id = config_id(endpoint);
            match (names.get(&id), endpoint) {
                (Some(name), ModelConfigRef::Group(_)) => format!("endpoint \"{name}\""),
                (Some(name), ModelConfigRef::User(_)) => format!("your endpoint \"{name}\""),
                (None, ModelConfigRef::Group(_)) => format!("endpoint {id}"),
                (None, ModelConfigRef::User(_)) => format!("your endpoint {id}"),
            }
        })
    }

    /// Describe why this candidate was skipped
    ///
    /// # Arguments
    ///
    /// * `names` - The known endpoint names
    /// * `harness` - The name of the run's harness
    #[must_use]
    pub fn reason_text(&self, names: &BTreeMap<Uuid, String>, harness: &str) -> String {
        // describe the endpoint the reason is about
        let endpoint = self
            .endpoint_text(names)
            .unwrap_or_else(|| "the endpoint".to_owned());
        // the reserved levels have no source of auth at all
        let reserved = matches!(
            self.level,
            Some(ModelAuthLevel::Harness | ModelAuthLevel::Workflow)
        );
        match &self.reason {
            ModelRejectReason::LevelNotAllowed => {
                "not allowed by the agent's or workflow's auth source".to_owned()
            }
            ModelRejectReason::NoEndpoint if reserved => "reserved, no auth source yet".to_owned(),
            ModelRejectReason::NoEndpoint => {
                "no endpoint at this level serves this model".to_owned()
            }
            ModelRejectReason::Banned(kind) => format!("{endpoint} is banned ({kind})"),
            ModelRejectReason::NoCredential if self.level == Some(ModelAuthLevel::User) => {
                format!("you have no usable credential for {endpoint}")
            }
            ModelRejectReason::NoCredential => format!("{endpoint} has no usable group credential"),
            ModelRejectReason::RequirementNotMet(requirement) => {
                format!("{endpoint}: {requirement}")
            }
            ModelRejectReason::ApiNotSupported(api) => {
                format!("{endpoint}: {harness} doesn't speak {api:?}")
            }
            ModelRejectReason::CredentialNotSupported(kind) => {
                format!("{endpoint}: {harness} can't use a {kind} credential")
            }
            ModelRejectReason::NotAllowedByDeployment => {
                format!("{endpoint} isn't on the deployment's allowed endpoint list")
            }
            ModelRejectReason::MixedModelsUnsupported => format!(
                "{endpoint}: {harness} runs every agent on one model, and this agent resolved to a different one"
            ),
            ModelRejectReason::DisabledOnEndpoint => format!("disabled on {endpoint}"),
            ModelRejectReason::DisabledByHarness => format!("disabled by {harness}"),
        }
    }

    /// Describe the level this rejection is about, naming the run group for the group level
    ///
    /// # Arguments
    ///
    /// * `group` - The name of the run group
    fn level_text(&self, group: &str) -> String {
        // a rejection with no level applies to every level
        match self.level {
            Some(ModelAuthLevel::Group) => format!("group {group}"),
            Some(level) => level_name(level).to_owned(),
            None => "every level".to_owned(),
        }
    }
}

impl AgentModelMismatch {
    /// Render this agent's rejections as one block of the mismatch text
    ///
    /// # Arguments
    ///
    /// * `names` - The names to render with
    fn render(&self, names: &MismatchNames) -> Vec<String> {
        // name the agent and the instance
        let mut lines = vec![format!(
            "No usable model for agent \"{}\" in {}.",
            self.name, names.instance
        )];
        // the models any agent of a harness without per-agent models resolved to differ
        if self
            .candidates
            .iter()
            .all(|candidate| candidate.reason == ModelRejectReason::MixedModelsUnsupported)
        {
            for candidate in &self.candidates {
                let model = candidate
                    .matcher
                    .as_ref()
                    .map_or_else(|| "a model".to_owned(), matcher_text);
                lines.push(format!(
                    "  {model} at {}: {}",
                    candidate.level_text(&names.group),
                    candidate.reason_text(&names.endpoints, &names.harness)
                ));
            }
            return lines;
        }
        // an empty list tries every model the auth source reaches
        let any = self
            .candidates
            .iter()
            .all(|candidate| candidate.matcher.is_none());
        if any {
            lines.push(
                "Allowed models: any model the auth source reaches, in the order tried:".to_owned(),
            );
        } else {
            lines.push("Allowed models, in priority order:".to_owned());
        }
        // group the candidates by the matcher being tried, keeping the order tried
        let mut index = 0;
        let mut current: Option<&Option<ModelMatcher>> = None;
        let mut pending: Option<(String, String)> = None;
        for candidate in &self.candidates {
            // start a new numbered model when the matcher changes
            if current != Some(&candidate.matcher) {
                if let Some((level, reason)) = pending.take() {
                    lines.push(format!("     - {level}: {reason}"));
                }
                current = Some(&candidate.matcher);
                index += 1;
                let model = candidate
                    .matcher
                    .as_ref()
                    .map_or_else(|| "any model".to_owned(), matcher_text);
                lines.push(format!("  {index}. {model}"));
            }
            let level = candidate.level_text(&names.group);
            let reason = candidate.reason_text(&names.endpoints, &names.harness);
            // the reserved levels share one line when they fail the same way
            match pending.take() {
                Some((prior, prior_reason))
                    if prior == "harness" && level == "workflow" && prior_reason == reason =>
                {
                    pending = Some((format!("{prior}, {level}"), reason));
                }
                Some((prior, prior_reason)) => {
                    lines.push(format!("     - {prior}: {prior_reason}"));
                    pending = Some((level, reason));
                }
                None => pending = Some((level, reason)),
            }
        }
        if let Some((level, reason)) = pending {
            lines.push(format!("     - {level}: {reason}"));
        }
        lines
    }
}

impl ModelResolutionError {
    /// Render this error as the text a refused run returns, naming endpoints, the run group, and
    /// the harness
    ///
    /// # Arguments
    ///
    /// * `names` - The names to render with
    #[must_use]
    pub fn render(&self, names: &MismatchNames) -> String {
        // render each unresolved agent in agent order
        let mut lines = Vec::new();
        for agent in &self.agents {
            lines.extend(agent.render(names));
        }
        // say how to fix the run
        lines.push(format!(
            "Fix one of these, or ask an owner or manager of {} to fix its model endpoints and credentials.",
            names.group
        ));
        lines.join("\n")
    }
}

impl fmt::Display for ModelResolutionError {
    /// Write one line per agent and candidate, naming the matcher, level, endpoint, and reason
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // no names are known here, so endpoints are named by id
        let names = BTreeMap::new();
        let mut first = true;
        for agent in &self.agents {
            for candidate in &agent.candidates {
                // separate the lines
                if !first {
                    writeln!(f)?;
                }
                first = false;
                let model = candidate
                    .matcher
                    .as_ref()
                    .map_or_else(|| "any model".to_owned(), matcher_text);
                let level = candidate.level.map_or("every level", level_name);
                let endpoint = candidate
                    .endpoint_text(&names)
                    .unwrap_or_else(|| "no endpoint".to_owned());
                write!(
                    f,
                    "{}: {model} at {level} on {endpoint}: {}",
                    agent.name,
                    candidate.reason_text(&names, "the harness")
                )?;
            }
        }
        Ok(())
    }
}

/// The model each agent of a run uses, as the thorium-agent writes it to the harness's models file
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HarnessModels {
    /// The model each agent uses, by agent id
    pub agents: BTreeMap<Uuid, HarnessModelEntry>,
}

/// How one agent calls its model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HarnessModelEntry {
    /// The provider and how to connect to it
    pub provider: ModelProvider,
    /// The provider's exact model id
    pub model_id: String,
    /// The protocol to call the model with
    pub api: ModelApi,
    /// The parameters to apply to every call
    #[serde(default)]
    pub parameters: ModelParameters,
    /// The model's effective token limits
    #[serde(default)]
    pub limits: ModelLimits,
    /// What the model can do (from its profile or catalog entry), read by the per-API renderers
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// The credential to authenticate with (unset for a keyless gateway)
    pub credential: Option<HarnessCredential>,
}

/// A credential as delivered to a harness, with its secret values
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HarnessCredential {
    /// Which credential this is
    pub id: ModelConfigRef,
    /// What kind of credential this is and its non-secret metadata
    pub kind: CredentialKind,
    /// The secret values (set; this struct only travels over admin routes and inside the pod)
    pub secrets: CredentialSecrets,
    /// The file the thorium-agent writes this credential to, for kinds SDKs read from a file
    /// (`GcpCredentialsJson`, `AzureEntraClient` certificates, and the AWS config for `AwsAssumeRole`)
    pub file: Option<String>,
    /// The access key an `AwsAssumeRole` credential assumes its role with
    #[cfg_attr(feature = "api", schema(no_recursion))]
    pub source: Option<Box<HarnessCredential>>,
}

/// A run's resolved models and the env vars that carry them (admins only)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct RunModels {
    /// The model each agent uses
    pub models: HarnessModels,
    /// The entry agent's credential under the harness's `HarnessEnv` names, by variable name
    #[serde(default)]
    pub env: BTreeMap<String, ModelSecret>,
}

impl HarnessCredential {
    /// Build a credential as delivered to a harness, naming the file it's written to
    ///
    /// # Arguments
    ///
    /// * `id` - Which credential this is
    /// * `kind` - What kind of credential this is
    /// * `secrets` - The credential's secret values
    /// * `source` - The access key an assumed role uses
    #[must_use]
    pub fn new(
        id: ModelConfigRef,
        kind: CredentialKind,
        secrets: CredentialSecrets,
        source: Option<HarnessCredential>,
    ) -> Self {
        // kinds SDKs read from a file get a file name
        let file = Self::file_name(id, &kind);
        HarnessCredential {
            id,
            kind,
            secrets,
            file,
            source: source.map(Box::new),
        }
    }

    /// Get the name of the file a credential is written to under the credentials directory, if
    /// SDKs read its kind from a file
    ///
    /// # Arguments
    ///
    /// * `id` - Which credential this is
    /// * `kind` - What kind of credential this is
    #[must_use]
    pub fn file_name(id: ModelConfigRef, kind: &CredentialKind) -> Option<String> {
        // only some kinds are read from a file
        let id = config_id(id);
        match kind {
            CredentialKind::GcpCredentialsJson { .. } => Some(format!("{id}.json")),
            CredentialKind::AzureEntraClient {
                auth: AzureClientAuth::Certificate,
                ..
            } => Some(format!("{id}.pem")),
            CredentialKind::AwsAssumeRole { .. } => Some(AWS_CONFIG_FILE.to_owned()),
            _ => None,
        }
    }

    /// Get the path this credential's file is written to, if it has one
    ///
    /// # Arguments
    ///
    /// * `dir` - The credentials directory
    #[must_use]
    pub fn path(&self, dir: &str) -> Option<String> {
        // the file sits in the credentials directory
        self.file
            .as_ref()
            .map(|file| format!("{}/{file}", dir.trim_end_matches('/')))
    }

    /// Get the primary secret of this credential
    #[must_use]
    pub fn primary(&self) -> Option<&ModelSecret> {
        self.secrets.primary.as_ref()
    }

    /// Get the contents of this credential's file, if it has one
    ///
    /// A GCP credentials JSON and an Entra certificate are their primary secrets, and an assumed
    /// role's file is an AWS config with a `thorium` profile that assumes the role with the source
    /// access key's profile.
    #[must_use]
    pub fn file_contents(&self) -> Option<String> {
        // each file kind holds a different part of the credential
        match &self.kind {
            CredentialKind::GcpCredentialsJson { .. } => {
                self.primary().map(|secret| secret.expose().to_owned())
            }
            CredentialKind::AzureEntraClient {
                auth: AzureClientAuth::Certificate,
                ..
            } => self.primary().map(|secret| secret.expose().to_owned()),
            CredentialKind::AwsAssumeRole {
                role_arn,
                external_id,
                session_name,
                duration_seconds,
                ..
            } => {
                // the source access key assumes the role
                let source = self.source.as_ref()?;
                let CredentialKind::AwsAccessKey { access_key_id } = &source.kind else {
                    return None;
                };
                let secret = source.primary()?;
                // the profile that assumes the role
                let mut lines = vec![
                    format!("[profile {AWS_PROFILE}]"),
                    format!("role_arn = {role_arn}"),
                    format!("source_profile = {AWS_SOURCE_PROFILE}"),
                ];
                if let Some(external_id) = external_id {
                    lines.push(format!("external_id = {external_id}"));
                }
                if let Some(session_name) = session_name {
                    lines.push(format!("role_session_name = {session_name}"));
                }
                if let Some(duration) = duration_seconds {
                    lines.push(format!("duration_seconds = {duration}"));
                }
                // the profile holding the source access key
                lines.push(String::new());
                lines.push(format!("[profile {AWS_SOURCE_PROFILE}]"));
                lines.push(format!("aws_access_key_id = {access_key_id}"));
                lines.push(format!("aws_secret_access_key = {}", secret.expose()));
                if let Some(token) = &source.secrets.session_token {
                    lines.push(format!("aws_session_token = {}", token.expose()));
                }
                lines.push(String::new());
                Some(lines.join("\n"))
            }
            _ => None,
        }
    }
}

/// Get the location of a Vertex AI endpoint as `GOOGLE_CLOUD_LOCATION` takes it
///
/// # Arguments
///
/// * `location` - The endpoint's location
fn vertex_location(location: &VertexLocation) -> String {
    // regions are written as is and the others by their names
    match location {
        VertexLocation::Region(region) => region.clone(),
        VertexLocation::Global => "global".to_owned(),
        VertexLocation::MultiRegion(VertexMultiRegion::Us) => "us".to_owned(),
        VertexLocation::MultiRegion(VertexMultiRegion::Eu) => "eu".to_owned(),
    }
}

impl HarnessModelEntry {
    /// Get the value one harness env var takes for this model, if it applies to its provider
    /// and credential kind
    ///
    /// # Arguments
    ///
    /// * `var` - The env var
    /// * `models_file` - The path of the harness's models file
    /// * `credentials_dir` - The directory credential files are written to
    #[must_use]
    // one arm per value and provider keeps the table readable, even where two arms give the
    // same value
    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    pub fn env_value(
        &self,
        var: HarnessEnvVar,
        models_file: &str,
        credentials_dir: &str,
    ) -> Option<String> {
        // read the credential's kind and primary secret once
        let credential = self.credential.as_ref();
        let kind = credential.map(|credential| &credential.kind);
        let primary = credential
            .and_then(HarnessCredential::primary)
            .map(|secret| secret.expose().to_owned());
        let api_key = matches!(kind, Some(CredentialKind::ApiKey));
        match (var, &self.provider) {
            // the model and models file apply to every model
            (HarnessEnvVar::ModelId, _) => Some(self.model_id.clone()),
            (HarnessEnvVar::ModelsFile, _) => Some(models_file.to_owned()),
            // OpenAI and the OpenAI compatible gateways
            (
                HarnessEnvVar::OpenAiApiKey,
                ModelProvider::OpenAi { .. }
                | ModelProvider::LiteLlm { .. }
                | ModelProvider::OpenAiCompatible { .. },
            ) if api_key => primary,
            (HarnessEnvVar::OpenAiBaseUrl, ModelProvider::OpenAi { base_url, .. }) => {
                base_url.clone()
            }
            (
                HarnessEnvVar::OpenAiBaseUrl,
                ModelProvider::LiteLlm { base_url, .. }
                | ModelProvider::OpenAiCompatible { base_url, .. },
            ) => Some(base_url.clone()),
            // Anthropic
            (HarnessEnvVar::AnthropicApiKey, ModelProvider::Anthropic { .. }) if api_key => primary,
            (HarnessEnvVar::AnthropicBaseUrl, ModelProvider::Anthropic { base_url, .. }) => {
                base_url.clone()
            }
            // Bedrock
            (HarnessEnvVar::AwsRegion, ModelProvider::Bedrock { region, .. }) => {
                Some(region.clone())
            }
            (HarnessEnvVar::AwsAccessKeyId, ModelProvider::Bedrock { .. }) => match kind {
                Some(CredentialKind::AwsAccessKey { access_key_id }) => Some(access_key_id.clone()),
                _ => None,
            },
            (HarnessEnvVar::AwsSecretAccessKey, ModelProvider::Bedrock { .. }) => match kind {
                Some(CredentialKind::AwsAccessKey { .. }) => primary,
                _ => None,
            },
            (HarnessEnvVar::AwsSessionToken, ModelProvider::Bedrock { .. }) => match kind {
                Some(CredentialKind::AwsAccessKey { .. }) => credential
                    .and_then(|credential| credential.secrets.session_token.as_ref())
                    .map(|token| token.expose().to_owned()),
                _ => None,
            },
            (HarnessEnvVar::AwsConfigFile, ModelProvider::Bedrock { .. }) => match kind {
                Some(CredentialKind::AwsAssumeRole { .. }) => {
                    credential.and_then(|credential| credential.path(credentials_dir))
                }
                _ => None,
            },
            (HarnessEnvVar::AwsProfile, ModelProvider::Bedrock { .. }) => match kind {
                Some(CredentialKind::AwsAssumeRole { .. }) => Some(AWS_PROFILE.to_owned()),
                _ => None,
            },
            (HarnessEnvVar::AwsBearerTokenBedrock, ModelProvider::Bedrock { .. }) => match kind {
                Some(CredentialKind::AwsBearerToken) => primary,
                _ => None,
            },
            // Vertex AI and the Gemini Developer API
            (HarnessEnvVar::GoogleApplicationCredentials, ModelProvider::Vertex { .. }) => {
                match kind {
                    Some(CredentialKind::GcpCredentialsJson { .. }) => {
                        credential.and_then(|credential| credential.path(credentials_dir))
                    }
                    _ => None,
                }
            }
            (HarnessEnvVar::GoogleCloudProject, ModelProvider::Vertex { project_id, .. }) => {
                project_id.clone()
            }
            (HarnessEnvVar::GoogleCloudLocation, ModelProvider::Vertex { location, .. }) => {
                Some(vertex_location(location))
            }
            (
                HarnessEnvVar::GeminiApiKey,
                ModelProvider::Gemini { .. } | ModelProvider::Vertex { .. },
            ) if api_key => primary,
            // Azure OpenAI
            (HarnessEnvVar::AzureOpenAiApiKey, ModelProvider::AzureOpenAi { .. }) if api_key => {
                primary
            }
            (HarnessEnvVar::AzureOpenAiEndpoint, ModelProvider::AzureOpenAi { endpoint, .. }) => {
                Some(endpoint.clone())
            }
            (HarnessEnvVar::AzureApiVersion, ModelProvider::AzureOpenAi { api_mode, .. }) => {
                match api_mode {
                    AzureApiMode::Legacy { api_version } => Some(api_version.clone()),
                    AzureApiMode::V1 => None,
                }
            }
            (HarnessEnvVar::AzureTenantId, ModelProvider::AzureOpenAi { .. }) => match kind {
                Some(CredentialKind::AzureEntraClient { tenant_id, .. }) => Some(tenant_id.clone()),
                _ => None,
            },
            (HarnessEnvVar::AzureClientId, ModelProvider::AzureOpenAi { .. }) => match kind {
                Some(CredentialKind::AzureEntraClient { client_id, .. }) => Some(client_id.clone()),
                _ => None,
            },
            (HarnessEnvVar::AzureClientSecret, ModelProvider::AzureOpenAi { .. }) => match kind {
                Some(CredentialKind::AzureEntraClient {
                    auth: AzureClientAuth::ClientSecret,
                    ..
                }) => primary,
                _ => None,
            },
            (HarnessEnvVar::AzureClientCertificatePath, ModelProvider::AzureOpenAi { .. }) => {
                match kind {
                    Some(CredentialKind::AzureEntraClient {
                        auth: AzureClientAuth::Certificate,
                        ..
                    }) => credential.and_then(|credential| credential.path(credentials_dir)),
                    _ => None,
                }
            }
            // every other value doesn't apply to this provider or credential
            _ => None,
        }
    }
}

impl RunModels {
    /// Get the env values a harness exports for its entry agent's model, under their names
    ///
    /// Only the values the harness exports are included, and a value that doesn't apply to the
    /// model's provider and credential kind is left out.
    ///
    /// # Arguments
    ///
    /// * `env` - The values the harness exports and their names
    /// * `entry` - The entry agent's model
    /// * `models_file` - The path of the harness's models file
    #[must_use]
    pub fn harness_env(
        env: &HarnessEnv,
        entry: &HarnessModelEntry,
        models_file: &str,
    ) -> BTreeMap<String, ModelSecret> {
        // add each exported value that applies under its name
        let mut values = BTreeMap::new();
        for var in &env.export {
            if let Some(value) = entry.env_value(*var, models_file, CREDENTIALS_DIR) {
                values.insert(env.name_of(*var).to_owned(), ModelSecret::from(value));
            }
        }
        values
    }
}
