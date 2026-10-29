//! Contains models for model endpoints, model profiles, and model credentials
//!
//! A [`ModelEndpointEntity`] says how to reach a model provider and which models it serves, a
//! [`ModelProfileEntity`] pins one model on one endpoint with a protocol and parameters, and a
//! [`ModelCredentialEntity`] holds the long-lived secret an endpoint authenticates with. Secrets
//! are held in [`ModelSecret`]s, whose `Debug` never prints them and which every API response
//! clears through [`RedactSecrets`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use strum::{AsRefStr, EnumDiscriminants, EnumString};
use uuid::Uuid;

use super::super::bans::Ban;

mod resolution;
#[cfg(test)]
mod tests;

pub use resolution::{
    AWS_CONFIG_FILE, AWS_PROFILE, AgentModelMismatch, CREDENTIALS_DIR, HarnessCredential,
    HarnessModelEntry, HarnessModels, MismatchNames, ModelCandidateRejection, ModelRejectReason,
    ModelResolutionError, RunModels,
};

/// The headers that carry a key for some provider and may never be set as extra headers
const AUTH_HEADERS: &[&str] = &["authorization", "x-api-key", "api-key", "x-goog-api-key"];

/// How many hex characters of a secret's SHA-256 its fingerprint keeps
#[cfg(any(feature = "api", feature = "client"))]
const FINGERPRINT_LEN: usize = 12;

/// The default `anthropic-version` header an Anthropic endpoint sends
fn default_anthropic_version() -> String {
    "2023-06-01".to_owned()
}

/// A model provider and how to connect to it
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq, EnumDiscriminants)]
// generate a type just containing the provider's name with no data
#[strum_discriminants(name(ModelProviderKind))]
#[strum_discriminants(derive(
    Serialize,
    Deserialize,
    AsRefStr,
    EnumString,
    strum::EnumIter,
    strum::Display,
    Hash,
    PartialOrd,
    Ord,
))]
#[cfg_attr(feature = "api", strum_discriminants(derive(utoipa::ToSchema)))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    strum_discriminants(derive(thorium_derive::DigestValue))
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelProvider {
    /// The `OpenAI` API
    OpenAi {
        /// The base url to use instead of `https://api.openai.com/v1`
        base_url: Option<String>,
        /// The `OpenAI` organization to bill (`OpenAI-Organization` header)
        organization: Option<String>,
        /// The `OpenAI` project to bill (`OpenAI-Project` header)
        project: Option<String>,
    },
    /// The Anthropic API
    Anthropic {
        /// The base url to use instead of `https://api.anthropic.com`
        base_url: Option<String>,
        /// The `anthropic-version` header to send
        #[serde(default = "default_anthropic_version")]
        version: String,
        /// The `anthropic-beta` features to enable
        #[serde(default)]
        betas: BTreeSet<String>,
        /// The Anthropic workspace this endpoint's keys belong to
        workspace_id: Option<String>,
    },
    /// AWS Bedrock
    Bedrock {
        /// The AWS region to call Bedrock in
        region: String,
        /// Which Bedrock endpoint family to call
        #[serde(default)]
        endpoint_kind: BedrockEndpointKind,
        /// The endpoint url to use instead of the regional default (for example a VPC endpoint)
        endpoint_url: Option<String>,
        /// The Bedrock workspace this endpoint belongs to
        workspace_id: Option<String>,
    },
    /// Google Cloud Vertex AI
    Vertex {
        /// The Google Cloud project (unset only for express-mode API keys)
        project_id: Option<String>,
        /// Where to call Vertex AI
        location: VertexLocation,
        /// The endpoint url to use instead of the location's default (for example Private
        /// Service Connect)
        endpoint_url: Option<String>,
    },
    /// The Gemini Developer API
    Gemini {
        /// The base url to use instead of `https://generativelanguage.googleapis.com`
        base_url: Option<String>,
        /// The API version path segment to call
        #[serde(default)]
        api_version: GeminiApiVersion,
    },
    /// Azure `OpenAI`
    AzureOpenAi {
        /// The resource endpoint (`https://{resource}.openai.azure.com`)
        endpoint: String,
        /// Which Azure `OpenAI` API surface to call
        api_mode: AzureApiMode,
        /// The Entra ID scope to request tokens for (unset = the default for `api_mode`)
        entra_scope: Option<String>,
    },
    /// A `LiteLLM` gateway
    LiteLlm {
        /// The base url of the gateway
        base_url: String,
        /// The header to send the key in instead of `Authorization: Bearer`
        key_header: Option<String>,
    },
    /// Any other server that speaks the `OpenAI` API (vLLM, Ollama, `OpenRouter`, Groq, Together, xAI)
    OpenAiCompatible {
        /// The base url of the server
        base_url: String,
        /// Extra non-secret headers to send with every request
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        /// The header to send the key in instead of `Authorization: Bearer`
        key_header: Option<String>,
    },
}

/// Check that a configured url is an `http` or `https` url
///
/// # Arguments
///
/// * `field` - The name of the field the url is in, for the error
/// * `raw` - The configured url
fn check_url(field: &str, raw: &str) -> Result<(), String> {
    // the url must parse on its own
    let parsed =
        url::Url::parse(raw).map_err(|err| format!("{field} `{raw}` is not a valid url: {err}"))?;
    // only web urls can reach a provider
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(format!(
            "{field} `{raw}` must be an http or https url, not {scheme}"
        )),
    }
}

/// Check that an optional url is an `http` or `https` url when it's set
///
/// # Arguments
///
/// * `field` - The name of the field the url is in, for the error
/// * `raw` - The configured url, if one is set
fn check_opt_url(field: &str, raw: Option<&String>) -> Result<(), String> {
    // an unset url uses the provider's default
    match raw {
        Some(raw) => check_url(field, raw),
        None => Ok(()),
    }
}

/// Check that an Entra scope names Azure AI, so a token is never minted for another resource
///
/// `https://ai.azure.com/.default`, `https://cognitiveservices.azure.com/.default`, and
/// `https://<resource>.cognitiveservices.azure.com/.default` are accepted.
///
/// # Arguments
///
/// * `scope` - The scope
fn check_entra_scope(scope: &str) -> Result<(), String> {
    // a resource scope names a single host label under the Cognitive Services domain
    let resource = scope
        .strip_prefix("https://")
        .and_then(|rest| rest.strip_suffix(".cognitiveservices.azure.com/.default"))
        .is_some_and(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    match scope {
        "https://ai.azure.com/.default" | "https://cognitiveservices.azure.com/.default" => Ok(()),
        _ if resource => Ok(()),
        _ => Err(format!(
            "AzureOpenAi.entra_scope `{scope}` must be https://ai.azure.com/.default or a https://cognitiveservices.azure.com/.default scope"
        )),
    }
}

/// Check that a required setting is set
///
/// # Arguments
///
/// * `field` - The name of the setting, for the error
/// * `value` - The setting's value
fn check_required(field: &str, value: &str) -> Result<(), String> {
    // a required setting can't be empty
    if value.trim().is_empty() {
        return Err(format!("{field} is required and can't be empty"));
    }
    Ok(())
}

/// Check that a value has no control characters, since it's written on one line of the AWS config
/// file a harness reads and a line break would let it add settings such as `credential_process`
///
/// # Arguments
///
/// * `field` - The name of the setting, for the error
/// * `value` - The setting's value
fn check_config_line(field: &str, value: &str) -> Result<(), String> {
    // a control character could end the line or corrupt it
    if value.chars().any(char::is_control) {
        return Err(format!("{field} can't contain control characters"));
    }
    Ok(())
}

/// Check that a region is a single host label, since it becomes part of a default url's host
///
/// # Arguments
///
/// * `field` - The name of the setting, for the error
/// * `region` - The region
fn check_region(field: &str, region: &str) -> Result<(), String> {
    // a region is required
    check_required(field, region)?;
    // a region is lowercase letters and digits in parts joined by single hyphens
    let valid = region.split('-').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{field} `{region}` must be a region name like `us-east-1`"
        ))
    }
}

/// Check that an id is a single url path segment, since it becomes part of a provider url
///
/// # Arguments
///
/// * `field` - The name of the setting, for the error
/// * `id` - The id
fn check_path_segment(field: &str, id: &str) -> Result<(), String> {
    // an id that's set can't be empty
    check_required(field, id)?;
    // an id can't leave its segment or start a query or fragment
    let escapes = id == "."
        || id == ".."
        || id.chars().any(|c| {
            matches!(c, '/' | '\\' | '?' | '#' | '%') || c.is_whitespace() || c.is_control()
        });
    if escapes {
        return Err(format!(
            "{field} `{id}` can't hold whitespace or any of / \\ ? # % and can't be . or .."
        ));
    }
    Ok(())
}

/// Check an optional id with [`check_path_segment`] when it's set
///
/// # Arguments
///
/// * `field` - The name of the setting, for the error
/// * `id` - The id, if it's set
fn check_opt_path_segment(field: &str, id: Option<&String>) -> Result<(), String> {
    // an unset id isn't sent
    match id {
        Some(id) => check_path_segment(field, id),
        None => Ok(()),
    }
}

impl ModelProvider {
    /// Get which provider this is without its settings
    #[must_use]
    pub fn kind(&self) -> ModelProviderKind {
        // the kind is this provider's discriminant
        ModelProviderKind::from(self)
    }

    /// Get the url this provider is configured to call, if it doesn't use its default
    ///
    /// This is the `base_url`, `endpoint`, or `endpoint_url` of the variant, the url the
    /// deployment's allowed endpoint list is matched against.
    #[must_use]
    pub fn configured_url(&self) -> Option<&str> {
        // each variant names its url differently
        match self {
            ModelProvider::OpenAi { base_url, .. }
            | ModelProvider::Anthropic { base_url, .. }
            | ModelProvider::Gemini { base_url, .. } => base_url.as_deref(),
            ModelProvider::Bedrock { endpoint_url, .. }
            | ModelProvider::Vertex { endpoint_url, .. } => endpoint_url.as_deref(),
            ModelProvider::AzureOpenAi { endpoint, .. } => Some(endpoint),
            ModelProvider::LiteLlm { base_url, .. }
            | ModelProvider::OpenAiCompatible { base_url, .. } => Some(base_url),
        }
    }

    /// Get the protocols this provider serves, with its default protocol first
    #[must_use]
    pub fn apis(&self) -> &'static [ModelApi] {
        // each provider serves a fixed list of protocols
        match self {
            ModelProvider::OpenAi { .. }
            | ModelProvider::AzureOpenAi { .. }
            | ModelProvider::OpenAiCompatible { .. } => {
                &[ModelApi::OpenAiChat, ModelApi::OpenAiResponses]
            }
            ModelProvider::Anthropic { .. } => &[ModelApi::AnthropicMessages],
            ModelProvider::Bedrock {
                endpoint_kind: BedrockEndpointKind::Runtime,
                ..
            } => &[ModelApi::BedrockConverse, ModelApi::BedrockInvoke],
            ModelProvider::Bedrock {
                endpoint_kind: BedrockEndpointKind::Mantle,
                ..
            } => &[
                ModelApi::AnthropicMessages,
                ModelApi::OpenAiChat,
                ModelApi::OpenAiResponses,
            ],
            ModelProvider::Vertex { .. } => {
                &[ModelApi::GeminiGenerateContent, ModelApi::VertexRawPredict]
            }
            ModelProvider::Gemini { .. } => &[
                ModelApi::GeminiGenerateContent,
                ModelApi::GeminiInteractions,
            ],
            ModelProvider::LiteLlm { .. } => &[
                ModelApi::OpenAiChat,
                ModelApi::OpenAiResponses,
                ModelApi::AnthropicMessages,
            ],
        }
    }

    /// Get the protocol used when neither a matcher nor a profile names one
    #[must_use]
    pub fn default_api(&self) -> ModelApi {
        // every provider lists its default protocol first
        self.apis()[0]
    }

    /// Get the credential kinds this provider accepts
    #[must_use]
    pub fn credential_types(&self) -> &'static [CredentialType] {
        // each provider accepts a fixed list of credential kinds
        match self {
            ModelProvider::OpenAi { .. }
            | ModelProvider::Anthropic { .. }
            | ModelProvider::Gemini { .. }
            | ModelProvider::LiteLlm { .. }
            | ModelProvider::OpenAiCompatible { .. } => &[CredentialType::ApiKey],
            ModelProvider::Bedrock { .. } => &[
                CredentialType::AwsAccessKey,
                CredentialType::AwsAssumeRole,
                CredentialType::AwsBearerToken,
            ],
            ModelProvider::Vertex { .. } => {
                &[CredentialType::GcpCredentialsJson, CredentialType::ApiKey]
            }
            ModelProvider::AzureOpenAi { .. } => {
                &[CredentialType::ApiKey, CredentialType::AzureEntraClient]
            }
        }
    }

    /// Whether this provider may be called without any credential
    ///
    /// Only the generic gateways may: self-hosted servers and gateways often accept
    /// unauthenticated calls.
    #[must_use]
    pub fn allows_keyless(&self) -> bool {
        // only the generic variants may skip a credential
        matches!(
            self,
            ModelProvider::LiteLlm { .. } | ModelProvider::OpenAiCompatible { .. }
        )
    }

    /// Make sure this provider serves a protocol
    ///
    /// # Arguments
    ///
    /// * `api` - The protocol to check
    pub fn check_api(&self, api: ModelApi) -> Result<(), String> {
        // the protocol must be one this provider serves
        if self.apis().contains(&api) {
            return Ok(());
        }
        Err(format!(
            "a {} endpoint doesn't serve {api:?}; use one of {:?}",
            self.kind(),
            self.apis()
        ))
    }

    /// Make sure this provider accepts a credential
    ///
    /// # Arguments
    ///
    /// * `kind` - The credential's kind
    pub fn check_credential(&self, kind: &CredentialKind) -> Result<(), String> {
        // the credential's kind must be one this provider accepts
        let cred_type = kind.cred_type();
        if !self.credential_types().contains(&cred_type) {
            return Err(format!(
                "a {} endpoint can't use a {cred_type} credential; use one of {:?}",
                self.kind(),
                self.credential_types()
            ));
        }
        // Vertex takes an API key only in express mode and service accounts only with a project
        if let ModelProvider::Vertex { project_id, .. } = self {
            match (cred_type, project_id) {
                (CredentialType::ApiKey, Some(_)) => {
                    return Err(
                        "a Vertex endpoint with a project_id can't use an ApiKey credential; API keys only work in express mode without a project_id".to_owned(),
                    );
                }
                (CredentialType::GcpCredentialsJson, None) => {
                    return Err(
                        "a Vertex endpoint needs a project_id to use a GcpCredentialsJson credential".to_owned(),
                    );
                }
                _ => (),
            }
        }
        Ok(())
    }

    /// Validate the settings of this provider that need nothing outside of it
    ///
    /// Required settings must be set, urls must be `http` or `https` urls, and no extra header may
    /// carry a key.
    pub fn validate(&self) -> Result<(), String> {
        // check each provider's own settings
        match self {
            ModelProvider::OpenAi { base_url, .. } => {
                check_opt_url("OpenAi.base_url", base_url.as_ref())
            }
            ModelProvider::Anthropic {
                base_url,
                version,
                workspace_id,
                ..
            } => {
                // the version header is always sent
                check_required("Anthropic.version", version)?;
                check_opt_path_segment("Anthropic.workspace_id", workspace_id.as_ref())?;
                check_opt_url("Anthropic.base_url", base_url.as_ref())
            }
            ModelProvider::Bedrock {
                region,
                endpoint_url,
                workspace_id,
                ..
            } => {
                // Bedrock is always called in a region, which names the default url's host
                check_region("Bedrock.region", region)?;
                check_opt_path_segment("Bedrock.workspace_id", workspace_id.as_ref())?;
                check_opt_url("Bedrock.endpoint_url", endpoint_url.as_ref())
            }
            ModelProvider::Vertex {
                project_id,
                location,
                endpoint_url,
            } => {
                // a single region must be named, and it names the default url's host
                if let VertexLocation::Region(region) = location {
                    check_region("Vertex.location.Region", region)?;
                }
                // the project is part of every url's path
                check_opt_path_segment("Vertex.project_id", project_id.as_ref())?;
                check_opt_url("Vertex.endpoint_url", endpoint_url.as_ref())
            }
            ModelProvider::Gemini { base_url, .. } => {
                check_opt_url("Gemini.base_url", base_url.as_ref())
            }
            ModelProvider::AzureOpenAi {
                endpoint,
                api_mode,
                entra_scope,
            } => {
                // the resource endpoint is required and must be a url
                check_required("AzureOpenAi.endpoint", endpoint)?;
                check_url("AzureOpenAi.endpoint", endpoint)?;
                // the legacy API needs a version to send
                if let AzureApiMode::Legacy { api_version } = api_mode {
                    check_required("AzureOpenAi.api_mode.Legacy.api_version", api_version)?;
                }
                // an Entra token is only ever minted for an Azure AI scope
                match entra_scope {
                    Some(scope) => check_entra_scope(scope),
                    None => Ok(()),
                }
            }
            ModelProvider::LiteLlm {
                base_url,
                key_header,
            } => {
                // a gateway always needs its url
                check_required("LiteLlm.base_url", base_url)?;
                check_url("LiteLlm.base_url", base_url)?;
                // a custom key header can't be empty
                if let Some(header) = key_header {
                    check_required("LiteLlm.key_header", header)?;
                }
                Ok(())
            }
            ModelProvider::OpenAiCompatible {
                base_url,
                extra_headers,
                key_header,
            } => {
                // a server always needs its url
                check_required("OpenAiCompatible.base_url", base_url)?;
                check_url("OpenAiCompatible.base_url", base_url)?;
                // a custom key header can't be empty
                if let Some(header) = key_header {
                    check_required("OpenAiCompatible.key_header", header)?;
                }
                // extra headers may never carry a key
                for name in extra_headers.keys() {
                    // compare header names without case
                    let lower = name.to_lowercase();
                    let is_key_header = key_header
                        .as_ref()
                        .is_some_and(|header| header.to_lowercase() == lower);
                    if AUTH_HEADERS.contains(&lower.as_str()) || is_key_header {
                        return Err(format!(
                            "OpenAiCompatible.extra_headers can't set `{name}`, which carries a key; put keys in a credential"
                        ));
                    }
                }
                Ok(())
            }
        }
    }
}

/// Which Bedrock endpoint family an endpoint calls
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum BedrockEndpointKind {
    /// The Bedrock runtime (`bedrock-runtime.{region}.amazonaws.com`: Converse and `InvokeModel`)
    #[default]
    Runtime,
    /// Bedrock Mantle (`bedrock-mantle.{region}.api.aws`: the Anthropic Messages and `OpenAI` APIs)
    Mantle,
}

/// Where an endpoint calls Vertex AI
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum VertexLocation {
    /// A single region such as `us-east5`
    Region(String),
    /// The global endpoint
    Global,
    /// A multi-region endpoint
    MultiRegion(VertexMultiRegion),
}

/// A Vertex AI multi-region
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum VertexMultiRegion {
    /// The United States multi-region
    Us,
    /// The European Union multi-region
    Eu,
}

/// The Gemini Developer API version an endpoint calls
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum GeminiApiVersion {
    /// The stable `v1` API
    V1,
    /// The `v1beta` API (the default of Google's SDKs)
    #[default]
    V1Beta,
}

/// Which Azure `OpenAI` API surface an endpoint calls
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum AzureApiMode {
    /// The `/openai/v1` API (default Entra scope `https://ai.azure.com/.default`)
    V1,
    /// The deployment-based API with an `api-version` query parameter (default Entra scope
    /// `https://cognitiveservices.azure.com/.default`)
    Legacy {
        /// The `api-version` to send
        api_version: String,
    },
}

/// The wire protocol a harness uses to call a model
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ModelApi {
    /// `OpenAI` Chat Completions
    OpenAiChat,
    /// `OpenAI` Responses
    OpenAiResponses,
    /// Anthropic Messages
    AnthropicMessages,
    /// Bedrock Converse
    BedrockConverse,
    /// Bedrock `InvokeModel`
    BedrockInvoke,
    /// Gemini generateContent
    GeminiGenerateContent,
    /// Gemini Interactions
    GeminiInteractions,
    /// Vertex AI rawPredict (partner models such as Claude on Vertex)
    VertexRawPredict,
}

impl ModelApi {
    /// Parse a protocol from its name as a form field carries it
    ///
    /// # Arguments
    ///
    /// * `raw` - The protocol's name (for example `OpenAiChat`)
    pub fn parse_name(raw: &str) -> Result<Self, String> {
        // a protocol is named by its variant
        serde_json::from_value(serde_json::Value::String(raw.to_owned()))
            .map_err(|_| format!("`{raw}` is not a model protocol"))
    }

    /// Get this protocol's name as a form field carries it
    #[must_use]
    pub fn name(&self) -> String {
        // name the protocol by its variant
        format!("{self:?}")
    }
}

/// What kind of credential this is and its non-secret metadata
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq, EnumDiscriminants)]
// generate a type just containing the credential kind's name with no data
#[strum_discriminants(name(CredentialType))]
#[strum_discriminants(derive(
    Serialize,
    Deserialize,
    AsRefStr,
    EnumString,
    strum::EnumIter,
    strum::Display,
    Hash,
    PartialOrd,
    Ord,
))]
#[cfg_attr(feature = "api", strum_discriminants(derive(utoipa::ToSchema)))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    strum_discriminants(derive(thorium_derive::DigestValue))
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum CredentialKind {
    /// An API key (the primary secret)
    ApiKey,
    /// An AWS access key pair (the primary secret is the secret access key; `session_token` is
    /// optional)
    AwsAccessKey {
        /// The AWS access key id
        access_key_id: String,
    },
    /// An AWS role assumed with another credential's access key (holds no secret itself)
    AwsAssumeRole {
        /// The ARN of the role to assume
        role_arn: String,
        /// The external id the role's trust policy requires
        external_id: Option<String>,
        /// The role session name (unset = `thorium-{reaction}`)
        session_name: Option<String>,
        /// How long each assumed-role session lasts in seconds (unset = the SDK default)
        duration_seconds: Option<u32>,
        /// The credential used to call `sts:AssumeRole`
        source: AwsRoleSource,
    },
    /// A long-term Bedrock API key (`ABSK…`, the primary secret)
    AwsBearerToken,
    /// A Google Cloud `service_account` or `external_account` JSON file (the primary secret)
    GcpCredentialsJson {
        /// The service account to impersonate with these credentials
        impersonate_service_account: Option<String>,
    },
    /// An Entra ID application (the primary secret is its client secret or PEM certificate)
    AzureEntraClient {
        /// The Entra ID tenant
        tenant_id: String,
        /// The application (client) id
        client_id: String,
        /// Whether the primary secret is a client secret or a certificate
        #[serde(default)]
        auth: AzureClientAuth,
    },
}

impl CredentialKind {
    /// Get the name of this credential's kind without its metadata
    #[must_use]
    pub fn cred_type(&self) -> CredentialType {
        // the type is this kind's discriminant
        CredentialType::from(self)
    }

    /// Whether this kind of credential holds a primary secret
    #[must_use]
    pub fn needs_secret(&self) -> bool {
        // only an assumed role borrows another credential's secret
        !matches!(self, CredentialKind::AwsAssumeRole { .. })
    }

    /// Get the access key credential an assumed role uses, if this is an assumed role
    #[must_use]
    pub fn role_source(&self) -> Option<Uuid> {
        // only assumed roles have a source
        match self {
            CredentialKind::AwsAssumeRole {
                source: AwsRoleSource::AccessKey(source),
                ..
            } => Some(*source),
            _ => None,
        }
    }

    /// Validate the metadata of this credential that needs nothing outside of it
    pub fn validate(&self) -> Result<(), String> {
        // check each kind's required metadata
        match self {
            CredentialKind::AwsAccessKey { access_key_id } => {
                check_required("AwsAccessKey.access_key_id", access_key_id)?;
                check_config_line("AwsAccessKey.access_key_id", access_key_id)
            }
            CredentialKind::AwsAssumeRole {
                role_arn,
                external_id,
                session_name,
                ..
            } => {
                // every value of an assumed role is a line of the harness's AWS config file
                check_required("AwsAssumeRole.role_arn", role_arn)?;
                check_config_line("AwsAssumeRole.role_arn", role_arn)?;
                if let Some(external_id) = external_id {
                    check_config_line("AwsAssumeRole.external_id", external_id)?;
                }
                if let Some(session_name) = session_name {
                    check_config_line("AwsAssumeRole.session_name", session_name)?;
                }
                Ok(())
            }
            CredentialKind::AzureEntraClient {
                tenant_id,
                client_id,
                ..
            } => {
                check_required("AzureEntraClient.tenant_id", tenant_id)?;
                check_required("AzureEntraClient.client_id", client_id)
            }
            CredentialKind::ApiKey
            | CredentialKind::AwsBearerToken
            | CredentialKind::GcpCredentialsJson { .. } => Ok(()),
        }
    }

    /// Validate the secrets this credential holds against its kind
    ///
    /// An access key's secret and session token are written on lines of the AWS config file an
    /// assumed role's harness reads, so they can't contain control characters. Other kinds' secrets
    /// (a GCP JSON file or a PEM certificate) span lines and are written to files of their own.
    ///
    /// # Arguments
    ///
    /// * `secrets` - The credential's secrets
    pub fn validate_secrets(&self, secrets: &CredentialSecrets) -> Result<(), String> {
        // only an access key's secrets are written into a config file
        if matches!(self, CredentialKind::AwsAccessKey { .. }) {
            if let Some(secret) = &secrets.primary {
                check_config_line("AwsAccessKey secret", secret.expose())?;
            }
            if let Some(token) = &secrets.session_token {
                check_config_line("AwsAccessKey session token", token.expose())?;
            }
        }
        Ok(())
    }
}

/// The credential an assumed AWS role is reached with
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum AwsRoleSource {
    /// An `AwsAccessKey` credential in the same scope (a group credential entity whose groups cover
    /// this credential's for a group credential, or another of the same user's credentials for a
    /// user credential)
    AccessKey(Uuid),
}

/// How an Entra ID application authenticates
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum AzureClientAuth {
    /// The primary secret is a client secret
    #[default]
    ClientSecret,
    /// The primary secret is a PEM certificate with its private key
    Certificate,
}

/// A secret value; `Debug` never prints it and every API response clears it
#[derive(Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[serde(transparent)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", schema(value_type = String, format = Password))]
pub struct ModelSecret(String);

impl fmt::Debug for ModelSecret {
    /// Write this secret without its value
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never print the value itself
        write!(f, "ModelSecret(<redacted>)")
    }
}

impl From<String> for ModelSecret {
    /// Wrap a secret value
    ///
    /// # Arguments
    ///
    /// * `value` - The secret value
    fn from(value: String) -> Self {
        ModelSecret(value)
    }
}

impl From<&str> for ModelSecret {
    /// Wrap a secret value
    ///
    /// # Arguments
    ///
    /// * `value` - The secret value
    fn from(value: &str) -> Self {
        ModelSecret(value.to_owned())
    }
}

impl ModelSecret {
    /// Get the secret value to send it somewhere that needs it
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether this secret has no value
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Get this secret's fingerprint: the first 12 lowercase hex characters of its SHA-256
    #[cfg(any(feature = "api", feature = "client"))]
    #[must_use]
    pub fn fingerprint(&self) -> String {
        use sha2::Digest;
        // hash the secret and keep the start of its hex digest
        let digest = sha2::Sha256::digest(self.0.as_bytes());
        let mut hex = hex::encode(digest);
        hex.truncate(FINGERPRINT_LEN);
        hex
    }
}

/// The secret values of a credential (cleared from every API response)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct CredentialSecrets {
    /// The primary secret (API key, AWS secret access key, Bedrock API key, GCP JSON, or Entra
    /// client secret or certificate); unset in responses and for `AwsAssumeRole`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<ModelSecret>,
    /// The AWS session token for temporary access keys; unset in responses
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<ModelSecret>,
    /// The first 12 hex characters of the primary secret's SHA-256, set by the API on every write
    /// and kept in responses so users can tell keys apart
    pub fingerprint: Option<String>,
}

/// Clears every secret value from a value before it is returned by the API
pub trait RedactSecrets {
    /// Clear every `ModelSecret` in this value, keeping fingerprints
    fn redact_secrets(&mut self);
}

impl RedactSecrets for CredentialSecrets {
    /// Clear the primary secret and session token, keeping the fingerprint
    fn redact_secrets(&mut self) {
        // drop both secret values
        self.primary = None;
        self.session_token = None;
    }
}

impl RedactSecrets for ModelCredentialEntity {
    /// Clear this credential's secret values, keeping its fingerprint
    fn redact_secrets(&mut self) {
        // only the secrets hold secret values
        self.secrets.redact_secrets();
    }
}

impl RedactSecrets for super::Entity {
    /// Clear the secret values of a model credential entity; other kinds hold none
    fn redact_secrets(&mut self) {
        // only model credentials hold secrets
        if let super::EntityMetadata::ModelCredential(credential) = &mut self.metadata {
            credential.redact_secrets();
        }
    }
}

/// The token limits of a model
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelLimits {
    /// The max number of input tokens the model accepts
    pub context_window: Option<u64>,
    /// The max number of tokens the model can produce in one response
    pub max_output_tokens: Option<u64>,
    /// Where these limits came from
    #[serde(default)]
    pub source: LimitsSource,
}

/// Where a model's limits came from
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum LimitsSource {
    /// Entered by a user; a refresh never changes them
    #[default]
    Manual,
    /// Reported by the provider's model listing
    Refreshed {
        /// When the listing reported them
        at: DateTime<Utc>,
    },
}

/// What a model can do (on a requirement, `true` means required)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
// each capability is an independent flag a requirement can ask for
#[allow(clippy::struct_excessive_bools)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(default)]
pub struct ModelCapabilities {
    /// The model can call tools
    pub tool_calling: bool,
    /// The model accepts images
    pub vision: bool,
    /// The model accepts PDF documents
    pub pdf_input: bool,
    /// The model can produce output that follows a JSON schema
    pub structured_output: bool,
    /// The model supports extended thinking or reasoning
    pub thinking: bool,
    /// The model accepts strict tool schemas (the API enforces the input schema exactly)
    pub strict_tools: bool,
    /// The model's API can turn parallel tool calls off for it
    pub parallel_tool_calls: bool,
    /// The model accepts a forced tool choice (any tool, or one named tool)
    pub forced_tool_choice: bool,
}

/// Sampling and generation parameters a profile applies to every call
///
/// Unknown keys are ignored when these are read, so a client or API that knows a parameter
/// added later still works with one that doesn't. Clients reading parameters a user wrote use
/// [`ModelParameters::from_value_strict`] to refuse misspelled keys instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelParameters {
    /// The sampling temperature
    pub temperature: Option<f64>,
    /// The nucleus sampling probability mass
    pub top_p: Option<f64>,
    /// The max tokens to produce per response (at most the model's `max_output_tokens`)
    pub max_output_tokens: Option<u64>,
    /// The token budget for extended thinking (requires the `thinking` capability)
    pub thinking_budget_tokens: Option<u64>,
    /// Sequences that stop generation
    #[serde(default)]
    pub stop_sequences: Vec<String>,
}

/// Model parameters as a user writes them, refusing any key that isn't a known parameter
#[cfg(feature = "client")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictModelParameters {
    /// The sampling temperature
    temperature: Option<f64>,
    /// The nucleus sampling probability mass
    top_p: Option<f64>,
    /// The max tokens to produce per response
    max_output_tokens: Option<u64>,
    /// The token budget for extended thinking
    thinking_budget_tokens: Option<u64>,
    /// Sequences that stop generation
    #[serde(default)]
    stop_sequences: Vec<String>,
}

impl ModelParameters {
    /// Read parameters a user wrote, refusing any key that isn't a known parameter
    ///
    /// The API ignores unknown keys so newer clients keep working against older servers;
    /// clients use this to catch a misspelled parameter before sending it.
    ///
    /// # Arguments
    ///
    /// * `value` - The parameters as a JSON value
    ///
    /// # Errors
    ///
    /// Returns an error naming the key if a key isn't a known parameter, or if a value has the
    /// wrong type
    #[cfg(feature = "client")]
    pub fn from_value_strict(value: serde_json::Value) -> Result<Self, crate::Error> {
        // read every known key and refuse any other
        let StrictModelParameters {
            temperature,
            top_p,
            max_output_tokens,
            thinking_budget_tokens,
            stop_sequences,
        } = serde_json::from_value(value)
            .map_err(|err| crate::Error::new(format!("Invalid model parameters: {err}")))?;
        // build the parameters from every known key
        Ok(ModelParameters {
            temperature,
            top_p,
            max_output_tokens,
            thinking_budget_tokens,
            stop_sequences,
        })
    }

    /// Make sure these parameters fit the model they're applied to
    ///
    /// # Arguments
    ///
    /// * `limits` - The model's effective limits
    /// * `capabilities` - The model's effective capabilities
    pub fn check_fit(
        &self,
        limits: &ModelLimits,
        capabilities: &ModelCapabilities,
    ) -> Result<(), String> {
        // a thinking budget needs a model that can think
        if self.thinking_budget_tokens.is_some() && !capabilities.thinking {
            return Err(
                "parameters.thinking_budget_tokens needs a model with the thinking capability"
                    .to_owned(),
            );
        }
        // the output cap can't exceed what the model can produce, when that's known
        if let (Some(wanted), Some(most)) = (self.max_output_tokens, limits.max_output_tokens)
            && wanted > most
        {
            return Err(format!(
                "parameters.max_output_tokens {wanted} is above the model's max_output_tokens {most}"
            ));
        }
        Ok(())
    }
}

/// One model an endpoint serves
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelCatalogEntry {
    /// The provider's exact model id (for Azure, the deployment name)
    pub model_id: String,
    /// The model's token limits
    #[serde(default)]
    pub limits: ModelLimits,
    /// What the model can do
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// The reasons this model can't be used, by ban id (any ban makes it unusable)
    #[serde(default)]
    pub bans: HashMap<Uuid, ModelBan>,
    /// How this entry got into the catalog
    pub source: CatalogEntrySource,
}

/// How a catalog entry got into an endpoint's catalog
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum CatalogEntrySource {
    /// Added by a user (never banned as `ModelRemoved`)
    Manual,
    /// Added by a refresh
    Refreshed {
        /// When a refresh first listed this model
        first_seen: DateTime<Utc>,
    },
}

/// The state of an endpoint's most recent refresh
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelRefreshStatus {
    /// Where the most recent refresh is
    pub state: ModelRefreshState,
    /// When the most recent refresh was requested
    pub requested: Option<DateTime<Utc>>,
    /// When the most recent refresh finished
    pub finished: Option<DateTime<Utc>>,
    /// Why the most recent refresh failed
    pub error: Option<String>,
}

/// Where an endpoint's most recent refresh is
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelRefreshState {
    /// This endpoint has never been refreshed
    #[default]
    Never,
    /// A refresh event is queued
    Pending,
    /// The most recent refresh listed the provider's models
    Succeeded,
    /// The most recent refresh failed
    Failed,
}

/// The most models one refresh report may list, far more than any provider serves
pub const MAX_LISTED_MODELS: usize = 10_000;

/// The result of listing one endpoint's models, sent by the workflows worker and merged into the
/// catalog by the API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelRefreshReport {
    /// When the listing finished
    pub finished: DateTime<Utc>,
    /// Every model the provider listed (empty when `failure` is set)
    #[serde(default)]
    pub models: Vec<ListedModel>,
    /// Why the listing failed (`CredentialRejected`, `CredentialExpired`, or `EndpointUnreachable`)
    pub failure: Option<ModelBanKind>,
    /// The credential the listing signed in with, the only one the report bans or clears (none
    /// for a listing without a credential)
    #[serde(default)]
    pub credential: Option<ModelConfigRef>,
}

impl ModelRefreshReport {
    /// Build the report of a listing that returned models
    ///
    /// # Arguments
    ///
    /// * `models` - Every model the provider listed
    #[must_use]
    pub fn listed(models: Vec<ListedModel>) -> Self {
        ModelRefreshReport {
            finished: Utc::now(),
            models,
            failure: None,
            credential: None,
        }
    }

    /// Build the report of a listing that failed
    ///
    /// # Arguments
    ///
    /// * `failure` - Why the listing failed
    #[must_use]
    pub fn failed(failure: ModelBanKind) -> Self {
        ModelRefreshReport {
            finished: Utc::now(),
            models: Vec::new(),
            failure: Some(failure),
            credential: None,
        }
    }

    /// Name the credential the listing signed in with
    ///
    /// # Arguments
    ///
    /// * `credential` - The credential, if the listing used one
    #[must_use]
    pub fn with_credential(mut self, credential: Option<ModelConfigRef>) -> Self {
        self.credential = credential;
        self
    }
}

/// One model a provider's listing returned
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ListedModel {
    /// The provider's exact model id
    pub model_id: String,
    /// The max input tokens, if the listing reports it
    pub context_window: Option<u64>,
    /// The max output tokens, if the listing reports it
    pub max_output_tokens: Option<u64>,
    /// What the model can do, if the listing reports it
    pub capabilities: Option<ModelCapabilities>,
}

impl ListedModel {
    /// Build a listed model that reports only its id
    ///
    /// # Arguments
    ///
    /// * `model_id` - The provider's exact model id
    #[must_use]
    pub fn new(model_id: impl Into<String>) -> Self {
        ListedModel {
            model_id: model_id.into(),
            context_window: None,
            max_output_tokens: None,
            capabilities: None,
        }
    }
}

/// A particular reason a model endpoint, catalog model, profile, or credential can't be used
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelBan {
    /// The unique id for this ban
    pub id: Uuid,
    /// The time in UTC that the ban was made
    pub time_banned: DateTime<Utc>,
    /// The kind of ban this is
    pub ban_kind: ModelBanKind,
}

impl ModelBan {
    /// Create a new ban made now
    ///
    /// # Arguments
    ///
    /// * `ban_kind` - The kind of ban to make
    #[must_use]
    pub fn new(ban_kind: ModelBanKind) -> Self {
        ModelBan {
            id: Uuid::new_v4(),
            time_banned: Utc::now(),
            ban_kind,
        }
    }

    /// Create a new ban set by hand with a message
    ///
    /// # Arguments
    ///
    /// * `msg` - Why this item is banned
    #[must_use]
    pub fn generic(msg: impl Into<String>) -> Self {
        // a ban by hand is a generic ban
        Self::new(ModelBanKind::Generic(msg.into()))
    }
}

/// The various kinds of bans model configuration can have
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelBanKind {
    /// A ban set by hand with a message (never cleared automatically)
    Generic(String),
    /// The provider's model listing omits this catalog model
    ModelRemoved,
    /// The provider rejected the credential
    CredentialRejected {
        /// The HTTP status the provider returned
        status: u16,
        /// The error message the provider returned
        message: String,
    },
    /// The credential's `expires` time has passed, or the provider reported it expired
    CredentialExpired,
    /// The endpoint could not be reached (the error)
    EndpointUnreachable(String),
    /// A referenced entity is missing or unusable (the referenced id)
    BrokenReference(Uuid),
    /// The endpoint isn't on the deployment's allowed endpoint list
    NotAllowedByDeployment,
}

impl ModelBanKind {
    /// Get the name of this kind of ban
    #[must_use]
    pub fn name(&self) -> &'static str {
        // name each kind by its variant
        match self {
            ModelBanKind::Generic(_) => "Generic",
            ModelBanKind::ModelRemoved => "ModelRemoved",
            ModelBanKind::CredentialRejected { .. } => "CredentialRejected",
            ModelBanKind::CredentialExpired => "CredentialExpired",
            ModelBanKind::EndpointUnreachable(_) => "EndpointUnreachable",
            ModelBanKind::BrokenReference(_) => "BrokenReference",
            ModelBanKind::NotAllowedByDeployment => "NotAllowedByDeployment",
        }
    }
}

impl fmt::Display for ModelBanKind {
    /// Write this ban kind and its detail
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the kind with the detail it carries
        match self {
            ModelBanKind::Generic(msg) => write!(f, "Generic: {msg}"),
            ModelBanKind::CredentialRejected { status, message } => {
                write!(f, "CredentialRejected: {status} {message}")
            }
            ModelBanKind::EndpointUnreachable(err) => write!(f, "EndpointUnreachable: {err}"),
            ModelBanKind::BrokenReference(id) => write!(f, "BrokenReference: {id}"),
            other => write!(f, "{}", other.name()),
        }
    }
}

/// A model ban together with the item it's on, for notifications that name the item
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedModelBan {
    /// The item the ban is on, when it isn't the entity or user the notification is for
    pub scope: Option<String>,
    /// The ban
    pub ban: ModelBan,
}

impl ScopedModelBan {
    /// Wrap a ban on the entity or user a notification is for
    ///
    /// # Arguments
    ///
    /// * `ban` - The ban
    #[must_use]
    pub fn top(ban: ModelBan) -> Self {
        ScopedModelBan { scope: None, ban }
    }

    /// Wrap a ban on an item inside the entity or user a notification is for
    ///
    /// # Arguments
    ///
    /// * `scope` - The item the ban is on
    /// * `ban` - The ban
    #[must_use]
    pub fn scoped(scope: impl Into<String>, ban: ModelBan) -> Self {
        ScopedModelBan {
            scope: Some(scope.into()),
            ban,
        }
    }
}

/// Implement the `Ban` trait for model bans on each kind that holds them
macro_rules! impl_model_ban {
    ($($holder:ty),+) => {
        $(
            impl Ban<$holder> for ModelBan {
                /// Get this ban's id
                fn id(&self) -> &Uuid {
                    &self.id
                }

                /// Describe this ban for a notification
                fn msg(&self) -> String {
                    // say why the item can't be used
                    format!("Banned: {}", self.ban_kind)
                }

                /// Get when this ban was made
                fn time_banned(&self) -> &DateTime<Utc> {
                    &self.time_banned
                }
            }

            impl Ban<$holder> for ScopedModelBan {
                /// Get the wrapped ban's id
                fn id(&self) -> &Uuid {
                    &self.ban.id
                }

                /// Describe this ban and the item it's on for a notification
                fn msg(&self) -> String {
                    // name the item when the ban is inside the notification's subject
                    match &self.scope {
                        Some(scope) => format!("{scope} is banned: {}", self.ban.ban_kind),
                        None => format!("Banned: {}", self.ban.ban_kind),
                    }
                }

                /// Get when the wrapped ban was made
                fn time_banned(&self) -> &DateTime<Utc> {
                    &self.ban.time_banned
                }
            }
        )+
    };
}

impl_model_ban!(
    ModelEndpointEntity,
    ModelProfileEntity,
    ModelCredentialEntity,
    super::super::User
);

/// An update to a model ban list containing bans to be added or removed
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelBanUpdate {
    /// The list of bans to be added
    #[serde(default)]
    pub bans_added: Vec<ModelBan>,
    /// The list of bans to be removed
    #[serde(default)]
    pub bans_removed: Vec<Uuid>,
}

impl ModelBanUpdate {
    /// Add a ban to this update
    ///
    /// # Arguments
    ///
    /// * `ban` - The ban to add
    #[must_use]
    pub fn add_ban(mut self, ban: ModelBan) -> Self {
        self.bans_added.push(ban);
        self
    }

    /// Remove a ban with this update
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the ban to remove
    #[must_use]
    pub fn remove_ban(mut self, id: Uuid) -> Self {
        self.bans_removed.push(id);
        self
    }

    /// Whether this update changes nothing
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bans_added.is_empty() && self.bans_removed.is_empty()
    }
}

/// A group's connection to a model provider and the models it serves
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelEndpointEntity {
    /// The provider and how to connect to it (must be on the deployment's allowed endpoint list)
    pub provider: ModelProvider,
    /// The group credential this endpoint authenticates with (mirrored by an `AuthenticatesWith`
    /// association); unset means each runner's own user credential is used
    pub credential: Option<Uuid>,
    /// The models this endpoint serves, sorted by `model_id`
    #[serde(default)]
    pub catalog: Vec<ModelCatalogEntry>,
    /// The reasons this endpoint can't be used, by ban id
    #[serde(default)]
    pub bans: HashMap<Uuid, ModelBan>,
    /// The state of this endpoint's most recent refresh
    #[serde(default)]
    pub refresh: ModelRefreshStatus,
    /// Exact model ids this endpoint never resolves to, even when its catalog lists them
    #[serde(default)]
    pub disabled_models: BTreeSet<String>,
    /// When this endpoint was last modified (including catalog and ban changes)
    pub modified: DateTime<Utc>,
}

impl ModelEndpointEntity {
    /// Get the catalog entry for a model id, if this endpoint serves it
    ///
    /// # Arguments
    ///
    /// * `model_id` - The exact model id to find
    #[must_use]
    pub fn catalog_entry(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        // the catalog is sorted by model id
        self.catalog
            .binary_search_by(|entry| entry.model_id.as_str().cmp(model_id))
            .ok()
            .map(|index| &self.catalog[index])
    }

    /// Add a catalog entry, or replace the entry with the same model id, keeping the catalog sorted
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to add or replace
    pub fn upsert_entry(&mut self, entry: ModelCatalogEntry) {
        // find where this model id is or belongs
        match self
            .catalog
            .binary_search_by(|existing| existing.model_id.cmp(&entry.model_id))
        {
            Ok(index) => self.catalog[index] = entry,
            Err(index) => self.catalog.insert(index, entry),
        }
    }
}

/// A named way to call one model on one endpoint
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelProfileEntity {
    /// The endpoint this profile calls (mirrored by a `Serves` association)
    pub endpoint: Uuid,
    /// The exact model id (must be in the endpoint's catalog)
    pub model_id: String,
    /// The protocol to call the model with (must be allowed for the endpoint's provider)
    pub api: ModelApi,
    /// The parameters applied to every call
    #[serde(default)]
    pub parameters: ModelParameters,
    /// Limits that replace the catalog entry's
    pub limits: Option<ModelLimits>,
    /// Capabilities that replace the catalog entry's
    pub capabilities: Option<ModelCapabilities>,
    /// The reasons this profile can't be used, by ban id
    #[serde(default)]
    pub bans: HashMap<Uuid, ModelBan>,
    /// When this profile was last modified
    pub modified: DateTime<Utc>,
}

/// A group's credential for model endpoints (its secrets are cleared from every response)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelCredentialEntity {
    /// What kind of credential this is and its non-secret metadata (fixed for its life)
    pub kind: CredentialKind,
    /// The secret values
    #[serde(default)]
    pub secrets: CredentialSecrets,
    /// When this credential stops working, if known
    pub expires: Option<DateTime<Utc>>,
    /// When the secret was last replaced
    pub rotated: Option<DateTime<Utc>>,
    /// The reasons this credential can't be used, by ban id
    #[serde(default)]
    pub bans: HashMap<Uuid, ModelBan>,
    /// When this credential was last modified
    pub modified: DateTime<Utc>,
}

/// A catalog entry added by hand
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelCatalogEntryRequest {
    /// The provider's exact model id (for Azure, the deployment name)
    pub model_id: String,
    /// The max input tokens
    pub context_window: Option<u64>,
    /// The max output tokens
    pub max_output_tokens: Option<u64>,
    /// What the model can do
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

impl ModelCatalogEntryRequest {
    /// Create a request for a catalog entry with no known limits or capabilities
    ///
    /// # Arguments
    ///
    /// * `model_id` - The provider's exact model id
    #[must_use]
    pub fn new(model_id: impl Into<String>) -> Self {
        ModelCatalogEntryRequest {
            model_id: model_id.into(),
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        }
    }

    /// Convert this request into a catalog entry added by hand
    #[must_use]
    pub fn into_entry(self) -> ModelCatalogEntry {
        // entries added by hand have manual limits
        ModelCatalogEntry {
            model_id: self.model_id,
            limits: ModelLimits {
                context_window: self.context_window,
                max_output_tokens: self.max_output_tokens,
                source: LimitsSource::Manual,
            },
            capabilities: self.capabilities,
            bans: HashMap::default(),
            source: CatalogEntrySource::Manual,
        }
    }
}

/// Build a sorted catalog from entries added by hand, refusing empty or repeated model ids
///
/// # Arguments
///
/// * `models` - The entries to add
pub fn build_catalog(
    models: Vec<ModelCatalogEntryRequest>,
) -> Result<Vec<ModelCatalogEntry>, String> {
    // track the ids we've seen to refuse repeats
    let mut seen = BTreeSet::new();
    let mut catalog = Vec::with_capacity(models.len());
    for model in models {
        // a model id can't be empty
        if model.model_id.trim().is_empty() {
            return Err("a catalog entry's model_id can't be empty".to_owned());
        }
        // a model id can only be listed once
        if !seen.insert(model.model_id.clone()) {
            return Err(format!(
                "model_id `{}` is listed more than once",
                model.model_id
            ));
        }
        catalog.push(model.into_entry());
    }
    // keep the catalog sorted by model id
    catalog.sort_by(|left, right| left.model_id.cmp(&right.model_id));
    Ok(catalog)
}

/// A request to create a model endpoint
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelEndpointEntityRequest {
    /// The provider and how to connect to it
    pub provider: ModelProvider,
    /// The group credential to authenticate with
    pub credential: Option<Uuid>,
    /// Catalog entries to add by hand
    #[serde(default)]
    pub models: Vec<ModelCatalogEntryRequest>,
    /// Exact model ids this endpoint never resolves to
    #[serde(default)]
    pub disabled_models: BTreeSet<String>,
}

impl ModelEndpointEntityRequest {
    /// Create a request for an endpoint with no credential, catalog, or disabled models
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider and how to connect to it
    #[must_use]
    pub fn new(provider: ModelProvider) -> Self {
        ModelEndpointEntityRequest {
            provider,
            credential: None,
            models: Vec::new(),
            disabled_models: BTreeSet::new(),
        }
    }

    /// Set the group credential this endpoint authenticates with
    ///
    /// # Arguments
    ///
    /// * `credential` - The id of the credential
    #[must_use]
    pub fn credential(mut self, credential: Uuid) -> Self {
        self.credential = Some(credential);
        self
    }

    /// Add a catalog entry by hand
    ///
    /// # Arguments
    ///
    /// * `model` - The catalog entry to add
    #[must_use]
    pub fn model(mut self, model: ModelCatalogEntryRequest) -> Self {
        self.models.push(model);
        self
    }

    /// Disable a model id on this endpoint
    ///
    /// # Arguments
    ///
    /// * `model_id` - The exact model id to disable
    #[must_use]
    pub fn disable(mut self, model_id: impl Into<String>) -> Self {
        self.disabled_models.insert(model_id.into());
        self
    }

    /// Get the form fields this request sends, by name
    ///
    /// # Errors
    ///
    /// Returns an error if the provider or catalog can't be serialized
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the kind, then this endpoint's metadata
        let mut fields = vec![
            (
                "kind",
                super::EntityKinds::ModelEndpoint.as_str().to_owned(),
            ),
            (
                "metadata[model_endpoint_provider]",
                serde_json::to_string(&self.provider)?,
            ),
        ];
        // the credential is optional
        if let Some(credential) = self.credential {
            fields.push((
                "metadata[model_endpoint_credential]",
                credential.to_string(),
            ));
        }
        // the catalog is sent as one JSON field
        if !self.models.is_empty() {
            fields.push((
                "metadata[model_endpoint_models]",
                serde_json::to_string(&self.models)?,
            ));
        }
        // each disabled model id is a list entry
        for model_id in &self.disabled_models {
            fields.push((
                "metadata[model_endpoint_disabled_models][]",
                model_id.clone(),
            ));
        }
        Ok(fields)
    }

    /// Add this endpoint request's metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // add each of our fields to the form
        let form = self
            .form_fields()?
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }
}

/// A request to create a model profile
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelProfileEntityRequest {
    /// The endpoint this profile calls
    pub endpoint: Uuid,
    /// The exact model id
    pub model_id: String,
    /// The protocol to call the model with
    pub api: ModelApi,
    /// The JSON encoded `ModelParameters`
    pub parameters: Option<String>,
    /// The JSON encoded `ModelLimits` that replace the catalog entry's
    pub limits: Option<String>,
    /// Capabilities that replace the catalog entry's
    pub capabilities: Option<ModelCapabilities>,
}

impl ModelProfileEntityRequest {
    /// Create a request for a profile with default parameters
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint the profile calls
    /// * `model_id` - The exact model id
    /// * `api` - The protocol to call the model with
    #[must_use]
    pub fn new(endpoint: Uuid, model_id: impl Into<String>, api: ModelApi) -> Self {
        ModelProfileEntityRequest {
            endpoint,
            model_id: model_id.into(),
            api,
            parameters: None,
            limits: None,
            capabilities: None,
        }
    }

    /// Set the parameters this profile applies to every call
    ///
    /// # Arguments
    ///
    /// * `parameters` - The parameters to apply
    ///
    /// # Errors
    ///
    /// Returns an error if the parameters can't be serialized
    pub fn parameters(mut self, parameters: &ModelParameters) -> Result<Self, serde_json::Error> {
        // parameters hold floats, so they travel as JSON
        self.parameters = Some(serde_json::to_string(parameters)?);
        Ok(self)
    }

    /// Set the limits that replace the catalog entry's
    ///
    /// # Arguments
    ///
    /// * `context_window` - The max input tokens
    /// * `max_output_tokens` - The max output tokens
    ///
    /// # Errors
    ///
    /// Returns an error if the limits can't be serialized
    pub fn limits(
        mut self,
        context_window: Option<u64>,
        max_output_tokens: Option<u64>,
    ) -> Result<Self, serde_json::Error> {
        // limits set by hand are always manual
        let limits = ModelLimits {
            context_window,
            max_output_tokens,
            source: LimitsSource::Manual,
        };
        self.limits = Some(serde_json::to_string(&limits)?);
        Ok(self)
    }

    /// Get the form fields this request sends, by name
    ///
    /// # Errors
    ///
    /// Returns an error if the capabilities can't be serialized
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the kind, then this profile's metadata
        let mut fields = vec![
            ("kind", super::EntityKinds::ModelProfile.as_str().to_owned()),
            (
                "metadata[model_profile_endpoint]",
                self.endpoint.to_string(),
            ),
            ("metadata[model_profile_model_id]", self.model_id.clone()),
            ("metadata[model_profile_api]", self.api.name()),
        ];
        // the optional JSON fields
        if let Some(parameters) = &self.parameters {
            fields.push(("metadata[model_profile_parameters]", parameters.clone()));
        }
        if let Some(limits) = &self.limits {
            fields.push(("metadata[model_profile_limits]", limits.clone()));
        }
        if let Some(capabilities) = &self.capabilities {
            fields.push((
                "metadata[model_profile_capabilities]",
                serde_json::to_string(capabilities)?,
            ));
        }
        Ok(fields)
    }

    /// Add this profile request's metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // add each of our fields to the form
        let form = self
            .form_fields()?
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }
}

/// A request to create a group model credential
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelCredentialEntityRequest {
    /// What kind of credential this is and its non-secret metadata
    pub kind: CredentialKind,
    /// The primary secret (required for every kind except `AwsAssumeRole`)
    pub secret: Option<ModelSecret>,
    /// The AWS session token for temporary access keys
    pub session_token: Option<ModelSecret>,
    /// When this credential stops working, if known
    pub expires: Option<DateTime<Utc>>,
}

impl ModelCredentialEntityRequest {
    /// Create a request for a credential
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of credential and its metadata
    /// * `secret` - The primary secret, unset for an assumed role
    #[must_use]
    pub fn new(kind: CredentialKind, secret: Option<ModelSecret>) -> Self {
        ModelCredentialEntityRequest {
            kind,
            secret,
            session_token: None,
            expires: None,
        }
    }

    /// Get the form fields this request sends, by name
    ///
    /// The secret values are sent as their plain values, since this form only travels to the API.
    ///
    /// # Errors
    ///
    /// Returns an error if the credential kind can't be serialized
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the kind, then this credential's metadata
        let mut fields = vec![
            (
                "kind",
                super::EntityKinds::ModelCredential.as_str().to_owned(),
            ),
            (
                "metadata[model_credential_kind]",
                serde_json::to_string(&self.kind)?,
            ),
        ];
        // the secrets are optional
        if let Some(secret) = &self.secret {
            fields.push((
                "metadata[model_credential_secret]",
                secret.expose().to_owned(),
            ));
        }
        if let Some(token) = &self.session_token {
            fields.push((
                "metadata[model_credential_session_token]",
                token.expose().to_owned(),
            ));
        }
        // the expiration is optional
        if let Some(expires) = &self.expires {
            fields.push(("metadata[model_credential_expires]", expires.to_rfc3339()));
        }
        Ok(fields)
    }

    /// Add this credential request's metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // add each of our fields to the form
        let form = self
            .form_fields()?
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }
}

/// Get the form fields a model entity update sends, by name
///
/// Other kinds of updates send no model fields.
///
/// # Arguments
///
/// * `update` - The update to send
///
/// # Errors
///
/// Returns an error if a JSON field can't be serialized
#[allow(clippy::too_many_lines)]
pub fn update_form_fields(
    update: &super::EntityMetadataUpdate,
) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
    // collect the fields that are set
    let mut fields = Vec::new();
    match update {
        super::EntityMetadataUpdate::ModelEndpoint {
            provider,
            credential,
            clear_credential,
            add_models,
            remove_models,
            bans,
            model_bans,
            disable_models,
            enable_models,
            refresh_report,
        } => {
            // the provider and credential
            if let Some(provider) = provider {
                fields.push((
                    "metadata[model_endpoint_provider]",
                    serde_json::to_string(provider)?,
                ));
            }
            if let Some(credential) = credential {
                fields.push((
                    "metadata[model_endpoint_credential]",
                    credential.to_string(),
                ));
            }
            if let Some(clear) = clear_credential {
                fields.push((
                    "metadata[clear_model_endpoint_credential]",
                    clear.to_string(),
                ));
            }
            // the catalog edits
            if !add_models.is_empty() {
                fields.push((
                    "metadata[add_model_endpoint_models]",
                    serde_json::to_string(add_models)?,
                ));
            }
            for model_id in remove_models {
                fields.push(("metadata[remove_model_endpoint_models][]", model_id.clone()));
            }
            // the disabled model edits
            for model_id in disable_models {
                fields.push((
                    "metadata[disable_model_endpoint_models][]",
                    model_id.clone(),
                ));
            }
            for model_id in enable_models {
                fields.push(("metadata[enable_model_endpoint_models][]", model_id.clone()));
            }
            // the ban edits
            if !bans.is_empty() {
                fields.push((
                    "metadata[model_endpoint_bans]",
                    serde_json::to_string(bans)?,
                ));
            }
            if !model_bans.is_empty() {
                fields.push((
                    "metadata[model_endpoint_model_bans]",
                    serde_json::to_string(model_bans)?,
                ));
            }
            // the listing result to merge
            if let Some(report) = refresh_report {
                fields.push((
                    "metadata[model_endpoint_refresh_report]",
                    serde_json::to_string(report)?,
                ));
            }
        }
        super::EntityMetadataUpdate::ModelProfile {
            endpoint,
            model_id,
            api,
            parameters,
            limits,
            clear_limits,
            capabilities,
            clear_capabilities,
            bans,
        } => {
            // the endpoint, model, and protocol
            if let Some(endpoint) = endpoint {
                fields.push(("metadata[model_profile_endpoint]", endpoint.to_string()));
            }
            if let Some(model_id) = model_id {
                fields.push(("metadata[model_profile_model_id]", model_id.clone()));
            }
            if let Some(api) = api {
                fields.push(("metadata[model_profile_api]", api.name()));
            }
            // the JSON encoded settings
            if let Some(parameters) = parameters {
                fields.push((
                    "metadata[model_profile_parameters]",
                    serde_json::to_string(parameters)?,
                ));
            }
            if let Some(limits) = limits {
                fields.push((
                    "metadata[model_profile_limits]",
                    serde_json::to_string(limits)?,
                ));
            }
            if let Some(clear) = clear_limits {
                fields.push(("metadata[clear_model_profile_limits]", clear.to_string()));
            }
            if let Some(capabilities) = capabilities {
                fields.push((
                    "metadata[model_profile_capabilities]",
                    serde_json::to_string(capabilities)?,
                ));
            }
            if let Some(clear) = clear_capabilities {
                fields.push((
                    "metadata[clear_model_profile_capabilities]",
                    clear.to_string(),
                ));
            }
            // the ban edits
            if !bans.is_empty() {
                fields.push(("metadata[model_profile_bans]", serde_json::to_string(bans)?));
            }
        }
        super::EntityMetadataUpdate::ModelCredential {
            secret,
            session_token,
            clear_session_token,
            expires,
            clear_expires,
            bans,
        } => {
            // the secrets are sent as their plain values
            if let Some(secret) = secret {
                fields.push((
                    "metadata[model_credential_secret]",
                    secret.expose().to_owned(),
                ));
            }
            if let Some(token) = session_token {
                fields.push((
                    "metadata[model_credential_session_token]",
                    token.expose().to_owned(),
                ));
            }
            if let Some(clear) = clear_session_token {
                fields.push((
                    "metadata[clear_model_credential_session_token]",
                    clear.to_string(),
                ));
            }
            // the expiration
            if let Some(expires) = expires {
                fields.push(("metadata[model_credential_expires]", expires.to_rfc3339()));
            }
            if let Some(clear) = clear_expires {
                fields.push((
                    "metadata[clear_model_credential_expires]",
                    clear.to_string(),
                ));
            }
            // the ban edits
            if !bans.is_empty() {
                fields.push((
                    "metadata[model_credential_bans]",
                    serde_json::to_string(bans)?,
                ));
            }
        }
        // other kinds have no model fields
        _ => (),
    }
    Ok(fields)
}

/// One model an agent accepts (all set fields must match)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub struct ModelMatcher {
    /// The provider's exact model id
    pub model_id: String,
    /// The provider the model must be served by (unset = any)
    pub provider: Option<ModelProviderKind>,
    /// The protocol the model must be called with (unset = the profile's or the provider's default)
    pub api: Option<ModelApi>,
    /// The API versions accepted (Anthropic `version`, Azure `api_version`, or Gemini
    /// `api_version` as written in the endpoint; empty = any)
    #[serde(default)]
    pub api_versions: BTreeSet<String>,
}

/// What every model an agent runs on must provide
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(default)]
pub struct ModelRequirements {
    /// The smallest acceptable context window (a model with no known context window fails it)
    pub min_context_window: Option<u64>,
    /// The smallest acceptable max output tokens (a model with no known value fails it)
    pub min_output_tokens: Option<u64>,
    /// The capabilities that must be present (`true` = required)
    pub capabilities: ModelCapabilities,
}

/// A level that can supply model auth, in precedence order (the derived `Ord` is the precedence)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelAuthLevel {
    /// Auth the harness supplies itself (reserved; nothing supplies it)
    Harness,
    /// Auth configured on the workflow (reserved; nothing configures it)
    Workflow,
    /// The endpoints in the run group (`Reaction.group`) with their group credentials
    Group,
    /// The runner's own credentials and endpoints
    User,
}

/// Which levels may supply model auth for an agent or workflow
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelAuthSource {
    /// The levels allowed (empty = every level)
    #[serde(default)]
    pub levels: BTreeSet<ModelAuthLevel>,
}

/// A reference to group-level or user-level model configuration
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ModelConfigRef {
    /// A group entity (endpoint, profile, or credential) by id
    Group(Uuid),
    /// An endpoint or credential in the runner's own model settings by id
    User(Uuid),
}

cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use crate::models::EntityMetadataForm;
        use crate::utils::ApiError;
        use crate::bad;

        /// Parse a JSON form field into a typed value, refusing invalid JSON with a `400`
        ///
        /// # Arguments
        ///
        /// * `field` - The name of the form field, for the error
        /// * `raw` - The field's JSON value
        pub(crate) fn parse_json_field<T: serde::de::DeserializeOwned>(
            field: &str,
            raw: &str,
        ) -> Result<T, ApiError> {
            // parse the typed value
            match serde_json::from_str(raw) {
                Ok(value) => Ok(value),
                Err(err) => bad!(format!("{field} is not valid: {err}")),
            }
        }

        impl ModelEndpointEntityRequest {
            /// Take the model endpoint fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // the provider is required
                let Some(raw_provider) = form.model_endpoint_provider.take() else {
                    return bad!("A model endpoint needs a model_endpoint_provider".to_owned());
                };
                let provider = parse_json_field("model_endpoint_provider", &raw_provider)?;
                // the catalog is optional
                let models = match form.model_endpoint_models.take() {
                    Some(raw) => parse_json_field("model_endpoint_models", &raw)?,
                    None => Vec::new(),
                };
                Ok(ModelEndpointEntityRequest {
                    provider,
                    credential: form.model_endpoint_credential.take(),
                    models,
                    disabled_models: std::mem::take(&mut form.model_endpoint_disabled_models),
                })
            }
        }

        impl ModelProfileEntityRequest {
            /// Take the model profile fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // the endpoint is required
                let Some(endpoint) = form.model_profile_endpoint.take() else {
                    return bad!("A model profile needs a model_profile_endpoint".to_owned());
                };
                // the model id is required
                let Some(model_id) = form.model_profile_model_id.take() else {
                    return bad!("A model profile needs a model_profile_model_id".to_owned());
                };
                // the protocol is required
                let Some(api) = form.model_profile_api.take() else {
                    return bad!("A model profile needs a model_profile_api".to_owned());
                };
                // the capabilities are optional JSON
                let capabilities = match form.model_profile_capabilities.take() {
                    Some(raw) => Some(parse_json_field("model_profile_capabilities", &raw)?),
                    None => None,
                };
                Ok(ModelProfileEntityRequest {
                    endpoint,
                    model_id,
                    api,
                    parameters: form.model_profile_parameters.take(),
                    limits: form.model_profile_limits.take(),
                    capabilities,
                })
            }
        }

        impl ModelCredentialEntityRequest {
            /// Take the model credential fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // the credential kind is required
                let Some(raw_kind) = form.model_credential_kind.take() else {
                    return bad!("A model credential needs a model_credential_kind".to_owned());
                };
                let kind = parse_json_field("model_credential_kind", &raw_kind)?;
                Ok(ModelCredentialEntityRequest {
                    kind,
                    secret: form.model_credential_secret.take(),
                    session_token: form.model_credential_session_token.take(),
                    expires: form.model_credential_expires.take(),
                })
            }
        }
    }
}

impl super::EntityMetadataUpdate {
    /// Create a model endpoint update that changes nothing yet
    #[must_use]
    pub fn model_endpoint() -> Self {
        super::EntityMetadataUpdate::ModelEndpoint {
            provider: None,
            credential: None,
            clear_credential: None,
            add_models: Vec::new(),
            remove_models: Vec::new(),
            bans: ModelBanUpdate::default(),
            model_bans: BTreeMap::new(),
            disable_models: BTreeSet::new(),
            enable_models: BTreeSet::new(),
            refresh_report: None,
        }
    }

    /// Set the listing result a model endpoint update merges into the endpoint's catalog
    ///
    /// Only admins may send one; the workflows worker sends one after each refresh.
    ///
    /// # Arguments
    ///
    /// * `report` - The listing result
    #[must_use]
    pub fn refresh_report(mut self, report: ModelRefreshReport) -> Self {
        // only endpoints are refreshed
        if let super::EntityMetadataUpdate::ModelEndpoint { refresh_report, .. } = &mut self {
            *refresh_report = Some(report);
        }
        self
    }

    /// Create a model profile update that changes nothing yet
    #[must_use]
    pub fn model_profile() -> Self {
        super::EntityMetadataUpdate::ModelProfile {
            endpoint: None,
            model_id: None,
            api: None,
            parameters: None,
            limits: None,
            clear_limits: None,
            capabilities: None,
            clear_capabilities: None,
            bans: ModelBanUpdate::default(),
        }
    }

    /// Create a model credential update that changes nothing yet
    #[must_use]
    pub fn model_credential() -> Self {
        super::EntityMetadataUpdate::ModelCredential {
            secret: None,
            session_token: None,
            clear_session_token: None,
            expires: None,
            clear_expires: None,
            bans: ModelBanUpdate::default(),
        }
    }

    /// Set the ban changes of a model endpoint, profile, or credential update
    ///
    /// # Arguments
    ///
    /// * `update` - The ban changes to apply
    #[must_use]
    pub fn model_bans_update(mut self, update: ModelBanUpdate) -> Self {
        // every model kind has its own bans
        match &mut self {
            super::EntityMetadataUpdate::ModelEndpoint { bans, .. }
            | super::EntityMetadataUpdate::ModelProfile { bans, .. }
            | super::EntityMetadataUpdate::ModelCredential { bans, .. } => *bans = update,
            _ => (),
        }
        self
    }

    /// Set the ban changes of one catalog entry in a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `model_id` - The catalog entry's model id
    /// * `update` - The ban changes to apply
    #[must_use]
    pub fn catalog_bans(mut self, model_id: impl Into<String>, update: ModelBanUpdate) -> Self {
        // only endpoints have catalogs
        if let super::EntityMetadataUpdate::ModelEndpoint { model_bans, .. } = &mut self {
            model_bans.insert(model_id.into(), update);
        }
        self
    }

    /// Set the new provider of a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `new` - The new provider
    #[must_use]
    pub fn endpoint_provider(mut self, new: ModelProvider) -> Self {
        // only endpoints have providers
        if let super::EntityMetadataUpdate::ModelEndpoint { provider, .. } = &mut self {
            *provider = Some(new);
        }
        self
    }

    /// Set the new group credential of a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `id` - The new credential
    #[must_use]
    pub fn endpoint_credential(mut self, id: Uuid) -> Self {
        // only endpoints have credentials
        if let super::EntityMetadataUpdate::ModelEndpoint { credential, .. } = &mut self {
            *credential = Some(id);
        }
        self
    }

    /// Clear the group credential in a model endpoint update
    #[must_use]
    pub fn clear_endpoint_credential(mut self) -> Self {
        // only endpoints have credentials
        if let super::EntityMetadataUpdate::ModelEndpoint {
            clear_credential, ..
        } = &mut self
        {
            *clear_credential = Some(true);
        }
        self
    }

    /// Add or replace a catalog entry in a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `model` - The catalog entry to add or replace
    #[must_use]
    pub fn add_catalog_model(mut self, model: ModelCatalogEntryRequest) -> Self {
        // only endpoints have catalogs
        if let super::EntityMetadataUpdate::ModelEndpoint { add_models, .. } = &mut self {
            add_models.push(model);
        }
        self
    }

    /// Remove a catalog entry in a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `model_id` - The model id to remove
    #[must_use]
    pub fn remove_catalog_model(mut self, model_id: impl Into<String>) -> Self {
        // only endpoints have catalogs
        if let super::EntityMetadataUpdate::ModelEndpoint { remove_models, .. } = &mut self {
            remove_models.push(model_id.into());
        }
        self
    }

    /// Disable a model id in a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `model_id` - The model id to disable
    #[must_use]
    pub fn disable_model(mut self, model_id: impl Into<String>) -> Self {
        // only endpoints disable models
        if let super::EntityMetadataUpdate::ModelEndpoint { disable_models, .. } = &mut self {
            disable_models.insert(model_id.into());
        }
        self
    }

    /// Enable a disabled model id in a model endpoint update
    ///
    /// # Arguments
    ///
    /// * `model_id` - The model id to enable
    #[must_use]
    pub fn enable_model(mut self, model_id: impl Into<String>) -> Self {
        // only endpoints disable models
        if let super::EntityMetadataUpdate::ModelEndpoint { enable_models, .. } = &mut self {
            enable_models.insert(model_id.into());
        }
        self
    }

    /// Set the new endpoint of a model profile update
    ///
    /// # Arguments
    ///
    /// * `id` - The new endpoint
    #[must_use]
    pub fn profile_endpoint(mut self, id: Uuid) -> Self {
        // only profiles call endpoints
        if let super::EntityMetadataUpdate::ModelProfile { endpoint, .. } = &mut self {
            *endpoint = Some(id);
        }
        self
    }

    /// Set the new model id of a model profile update
    ///
    /// # Arguments
    ///
    /// * `id` - The new model id
    #[must_use]
    pub fn profile_model(mut self, id: impl Into<String>) -> Self {
        // only profiles call models
        if let super::EntityMetadataUpdate::ModelProfile { model_id, .. } = &mut self {
            *model_id = Some(id.into());
        }
        self
    }

    /// Set the new protocol of a model profile update
    ///
    /// # Arguments
    ///
    /// * `new` - The new protocol
    #[must_use]
    pub fn profile_api(mut self, new: ModelApi) -> Self {
        // only profiles have protocols
        if let super::EntityMetadataUpdate::ModelProfile { api, .. } = &mut self {
            *api = Some(new);
        }
        self
    }

    /// Set the new parameters of a model profile update
    ///
    /// # Arguments
    ///
    /// * `new` - The new parameters
    #[must_use]
    pub fn profile_parameters(mut self, new: ModelParameters) -> Self {
        // only profiles have parameters
        if let super::EntityMetadataUpdate::ModelProfile { parameters, .. } = &mut self {
            *parameters = Some(new);
        }
        self
    }

    /// Set the new limits of a model profile update
    ///
    /// # Arguments
    ///
    /// * `new` - The new limits
    #[must_use]
    pub fn profile_limits(mut self, new: ModelLimits) -> Self {
        // only profiles override limits
        if let super::EntityMetadataUpdate::ModelProfile { limits, .. } = &mut self {
            *limits = Some(new);
        }
        self
    }

    /// Clear the limits in a model profile update
    #[must_use]
    pub fn clear_profile_limits(mut self) -> Self {
        // only profiles override limits
        if let super::EntityMetadataUpdate::ModelProfile { clear_limits, .. } = &mut self {
            *clear_limits = Some(true);
        }
        self
    }

    /// Set the new capabilities of a model profile update
    ///
    /// # Arguments
    ///
    /// * `new` - The new capabilities
    #[must_use]
    pub fn profile_capabilities(mut self, new: ModelCapabilities) -> Self {
        // only profiles override capabilities
        if let super::EntityMetadataUpdate::ModelProfile { capabilities, .. } = &mut self {
            *capabilities = Some(new);
        }
        self
    }

    /// Clear the capabilities in a model profile update
    #[must_use]
    pub fn clear_profile_capabilities(mut self) -> Self {
        // only profiles override capabilities
        if let super::EntityMetadataUpdate::ModelProfile {
            clear_capabilities, ..
        } = &mut self
        {
            *clear_capabilities = Some(true);
        }
        self
    }

    /// Set the new secret of a model credential update, which rotates it
    ///
    /// # Arguments
    ///
    /// * `new` - The new primary secret
    #[must_use]
    pub fn credential_secret(mut self, new: impl Into<ModelSecret>) -> Self {
        // only credentials hold secrets
        if let super::EntityMetadataUpdate::ModelCredential { secret, .. } = &mut self {
            *secret = Some(new.into());
        }
        self
    }

    /// Set the new session token of a model credential update
    ///
    /// # Arguments
    ///
    /// * `new` - The new session token
    #[must_use]
    pub fn credential_session_token(mut self, new: impl Into<ModelSecret>) -> Self {
        // only credentials hold session tokens
        if let super::EntityMetadataUpdate::ModelCredential { session_token, .. } = &mut self {
            *session_token = Some(new.into());
        }
        self
    }

    /// Clear the session token in a model credential update
    #[must_use]
    pub fn clear_credential_session_token(mut self) -> Self {
        // only credentials hold session tokens
        if let super::EntityMetadataUpdate::ModelCredential {
            clear_session_token,
            ..
        } = &mut self
        {
            *clear_session_token = Some(true);
        }
        self
    }

    /// Set when a credential stops working in a model credential update
    ///
    /// # Arguments
    ///
    /// * `new` - When the credential stops working
    #[must_use]
    pub fn credential_expires(mut self, new: DateTime<Utc>) -> Self {
        // only credentials expire
        if let super::EntityMetadataUpdate::ModelCredential { expires, .. } = &mut self {
            *expires = Some(new);
        }
        self
    }

    /// Clear the expiration in a model credential update
    #[must_use]
    pub fn clear_credential_expires(mut self) -> Self {
        // only credentials expire
        if let super::EntityMetadataUpdate::ModelCredential { clear_expires, .. } = &mut self {
            *clear_expires = Some(true);
        }
        self
    }
}

cfg_if::cfg_if! {
    if #[cfg(any(feature = "api", feature = "client"))] {
        use crate::models::backends::NotificationSupport;
        use crate::models::{KeySupport, NotificationType};

        /// Implement notification keys for a model entity kind, keyed by the entity's id
        macro_rules! impl_model_notifications {
            ($holder:ty, $kind:expr) => {
                impl NotificationSupport for $holder {
                    /// Provide this kind's notification type
                    fn notification_type() -> NotificationType {
                        $kind
                    }
                }

                impl KeySupport for $holder {
                    /// The entity's id
                    type Key = String;

                    /// Model entities have no extra components for their keys
                    type ExtraKey = ();

                    /// Build the key for this entity
                    ///
                    /// # Arguments
                    ///
                    /// * `key` - The entity's id
                    /// * `_extra` - Unused; these keys have no extra components
                    fn build_key(key: Self::Key, _extra: &Self::ExtraKey) -> String {
                        key
                    }

                    /// Build a URL component for this entity
                    ///
                    /// # Arguments
                    ///
                    /// * `key` - The entity's id
                    /// * `_extra` - Unused; these keys have no extra components
                    fn key_url(key: &Self::Key, _extra: Option<&Self::ExtraKey>) -> String {
                        key.clone()
                    }
                }
            };
        }

        impl_model_notifications!(ModelEndpointEntity, NotificationType::ModelEndpoints);
        impl_model_notifications!(ModelProfileEntity, NotificationType::ModelProfiles);
        impl_model_notifications!(ModelCredentialEntity, NotificationType::ModelCredentials);
    }
}
