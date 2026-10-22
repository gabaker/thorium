//! The model values a harness's pod env may hold, and the checks on how a harness renames them
//!
//! A harness's env is a fixed allowlist: every value it can get is a [`HarnessEnvVar`], each is
//! off until it's exported, and an exported value may be renamed but nothing new can be added.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The env var names every harness pod already uses, which no harness value may take
const RESERVED_NAMES: [&str; 3] = ["THORIUM_URL", "THORIUM_TOKEN", "THORIUM_MODELS"];

/// The env var names the system reads that no rename may take, beyond the prefixes and suffix in
/// [`is_reserved_rename`]
const SYSTEM_NAMES: [&str; 2] = ["PATH", "HOME"];

/// The values the scaler puts in a harness's pod env, each under its standard name unless renamed
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct HarnessEnv {
    /// The values to export (empty = none of them)
    pub export: BTreeSet<HarnessEnvVar>,
    /// Variable names that replace the standard name of an exported value
    pub names: BTreeMap<HarnessEnvVar, String>,
}

/// A value that may be exported to a harness, documented with its standard variable name
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum HarnessEnvVar {
    /// The entry agent's exact model id (`THORIUM_MODEL`)
    ModelId,
    /// The path of the models file (`THORIUM_MODELS_FILE`, `HarnessInputs.models`)
    ModelsFile,
    /// The `OpenAI`, `LiteLLM`, or `OpenAI`-compatible API key (`OPENAI_API_KEY`)
    OpenAiApiKey,
    /// The `OpenAI`, `LiteLLM`, or `OpenAI`-compatible base url (`OPENAI_BASE_URL`)
    OpenAiBaseUrl,
    /// The Anthropic API key (`ANTHROPIC_API_KEY`)
    AnthropicApiKey,
    /// The Anthropic base url (`ANTHROPIC_BASE_URL`)
    AnthropicBaseUrl,
    /// The AWS access key id (`AWS_ACCESS_KEY_ID`)
    AwsAccessKeyId,
    /// The AWS secret access key (`AWS_SECRET_ACCESS_KEY`)
    AwsSecretAccessKey,
    /// The AWS session token (`AWS_SESSION_TOKEN`)
    AwsSessionToken,
    /// The AWS region (`AWS_REGION`)
    AwsRegion,
    /// The path of the AWS config file for an assumed role (`AWS_CONFIG_FILE`)
    AwsConfigFile,
    /// The AWS profile in that file (`AWS_PROFILE`)
    AwsProfile,
    /// The Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`)
    AwsBearerTokenBedrock,
    /// The path of the Google Cloud credentials JSON file (`GOOGLE_APPLICATION_CREDENTIALS`)
    GoogleApplicationCredentials,
    /// The Google Cloud project (`GOOGLE_CLOUD_PROJECT`)
    GoogleCloudProject,
    /// The Vertex AI location (`GOOGLE_CLOUD_LOCATION`)
    GoogleCloudLocation,
    /// The Gemini or Vertex express-mode API key (`GEMINI_API_KEY`)
    GeminiApiKey,
    /// The Azure `OpenAI` API key (`AZURE_OPENAI_API_KEY`)
    AzureOpenAiApiKey,
    /// The Azure `OpenAI` resource endpoint (`AZURE_OPENAI_ENDPOINT`)
    AzureOpenAiEndpoint,
    /// The Azure `OpenAI` legacy `api-version` (`OPENAI_API_VERSION`)
    AzureApiVersion,
    /// The Entra ID tenant (`AZURE_TENANT_ID`)
    AzureTenantId,
    /// The Entra ID application id (`AZURE_CLIENT_ID`)
    AzureClientId,
    /// The Entra ID client secret (`AZURE_CLIENT_SECRET`)
    AzureClientSecret,
    /// The path of the Entra ID certificate file (`AZURE_CLIENT_CERTIFICATE_PATH`)
    AzureClientCertificatePath,
}

impl HarnessEnvVar {
    /// Get the standard env var name this value is exported under unless it's renamed
    #[must_use]
    pub fn standard_name(&self) -> &'static str {
        // each value's name is the one its SDKs read
        match self {
            HarnessEnvVar::ModelId => "THORIUM_MODEL",
            HarnessEnvVar::ModelsFile => "THORIUM_MODELS_FILE",
            HarnessEnvVar::OpenAiApiKey => "OPENAI_API_KEY",
            HarnessEnvVar::OpenAiBaseUrl => "OPENAI_BASE_URL",
            HarnessEnvVar::AnthropicApiKey => "ANTHROPIC_API_KEY",
            HarnessEnvVar::AnthropicBaseUrl => "ANTHROPIC_BASE_URL",
            HarnessEnvVar::AwsAccessKeyId => "AWS_ACCESS_KEY_ID",
            HarnessEnvVar::AwsSecretAccessKey => "AWS_SECRET_ACCESS_KEY",
            HarnessEnvVar::AwsSessionToken => "AWS_SESSION_TOKEN",
            HarnessEnvVar::AwsRegion => "AWS_REGION",
            HarnessEnvVar::AwsConfigFile => "AWS_CONFIG_FILE",
            HarnessEnvVar::AwsProfile => "AWS_PROFILE",
            HarnessEnvVar::AwsBearerTokenBedrock => "AWS_BEARER_TOKEN_BEDROCK",
            HarnessEnvVar::GoogleApplicationCredentials => "GOOGLE_APPLICATION_CREDENTIALS",
            HarnessEnvVar::GoogleCloudProject => "GOOGLE_CLOUD_PROJECT",
            HarnessEnvVar::GoogleCloudLocation => "GOOGLE_CLOUD_LOCATION",
            HarnessEnvVar::GeminiApiKey => "GEMINI_API_KEY",
            HarnessEnvVar::AzureOpenAiApiKey => "AZURE_OPENAI_API_KEY",
            HarnessEnvVar::AzureOpenAiEndpoint => "AZURE_OPENAI_ENDPOINT",
            HarnessEnvVar::AzureApiVersion => "OPENAI_API_VERSION",
            HarnessEnvVar::AzureTenantId => "AZURE_TENANT_ID",
            HarnessEnvVar::AzureClientId => "AZURE_CLIENT_ID",
            HarnessEnvVar::AzureClientSecret => "AZURE_CLIENT_SECRET",
            HarnessEnvVar::AzureClientCertificatePath => "AZURE_CLIENT_CERTIFICATE_PATH",
        }
    }
}

/// Check whether a name can be an env var name (`^[A-Za-z_][A-Za-z0-9_]*$`)
///
/// # Arguments
///
/// * `name` - The name to check
fn is_env_name(name: &str) -> bool {
    // the name starts with a letter or underscore and holds only letters, digits, and
    // underscores
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check whether a rename would take a name Thorium or the system reserves
///
/// The `THORIUM_` prefix, `PATH`, `HOME`, `LD_*` (the dynamic linker's), and `*_PROXY` names are
/// reserved, whatever their case, because a harness's pod depends on them and a renamed value
/// there would redirect how its processes load code, resolve programs, or reach the network.
///
/// # Arguments
///
/// * `name` - The name a value would be renamed to
fn is_reserved_rename(name: &str) -> bool {
    // names are compared in upper case so `http_proxy` is held to the same rule as `HTTP_PROXY`
    let upper = name.to_ascii_uppercase();
    upper.starts_with("THORIUM_")
        || upper.starts_with("LD_")
        || upper.ends_with("_PROXY")
        || SYSTEM_NAMES.contains(&upper.as_str())
}

impl HarnessEnv {
    /// Parse the JSON encoded harness env a request carries and check it
    ///
    /// # Arguments
    ///
    /// * `raw` - The JSON encoded harness env
    pub fn parse_request(raw: &str) -> Result<Self, String> {
        // parse the typed env, refusing unknown fields and values
        let env: HarnessEnv = serde_json::from_str(raw)
            .map_err(|err| format!("tool_harness_env is not a valid harness env: {err}"))?;
        // check the names it exports its values under
        env.validate()?;
        Ok(env)
    }

    /// Get the env var name an exported value is written under
    ///
    /// # Arguments
    ///
    /// * `var` - The exported value
    #[must_use]
    pub fn name_of(&self, var: HarnessEnvVar) -> &str {
        // a rename replaces the standard name
        self.names
            .get(&var)
            .map_or_else(|| var.standard_name(), String::as_str)
    }

    /// Check that every rename is of an exported value and that every exported value gets a
    /// name of its own the pod doesn't already use
    pub fn validate(&self) -> Result<(), String> {
        // a rename only applies to an exported value and must be an env var name
        for (var, name) in &self.names {
            if !self.export.contains(var) {
                return Err(format!(
                    "tool_harness_env renames {var:?}, which isn't exported"
                ));
            }
            if !is_env_name(name) {
                return Err(format!(
                    "tool_harness_env renames {var:?} to `{name}`, which isn't a valid env var name"
                ));
            }
            // a rename can't take a name Thorium or the system reserves
            if is_reserved_rename(name) {
                return Err(format!(
                    "tool_harness_env renames {var:?} to `{name}`, which is reserved: `THORIUM_` names, `PATH`, `HOME`, `LD_*`, and `*_PROXY` can't be used"
                ));
            }
        }
        // each exported value gets a name no other value and nothing Thorium sets uses
        let mut taken: HashMap<&str, HarnessEnvVar> = HashMap::with_capacity(self.export.len());
        for var in &self.export {
            let name = self.name_of(*var);
            if RESERVED_NAMES.contains(&name) {
                return Err(format!(
                    "tool_harness_env can't export {var:?} as `{name}`, which every harness pod already sets"
                ));
            }
            if let Some(other) = taken.insert(name, *var) {
                return Err(format!(
                    "tool_harness_env exports both {other:?} and {var:?} as `{name}`"
                ));
            }
        }
        Ok(())
    }
}
