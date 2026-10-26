use std::path::{Path, PathBuf};
use url::Url;

use super::Keys;
use crate::Error;

#[cfg(feature = "python")]
use pyo3::pyclass;

/// Normalize a URL into the host string reqwest compares against when matching `NO_PROXY`
///
/// reqwest's `NO_PROXY` matching tests the request's host (no scheme, port, or path), so a
/// `no_proxy` entry meant to bypass a given URL has to be reduced to just that host. This parses
/// `url` with the same library reqwest uses, so the result matches the host the client actually
/// connects to. Returns `None` if `url` has no host (e.g. it isn't a valid URL).
///
/// This is only meant for inputs that are URLs; raw `NO_PROXY` entries (bare domains, leading-dot
/// domains, IPs, CIDRs) are already in the form reqwest expects and should be used as-is.
///
/// # Arguments
///
/// * `url` - The URL whose host should be used as a `no_proxy` entry
#[must_use]
pub fn no_proxy_host(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
}

/// The settings to use when cloning repos
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitSettings {
    /// The path to the SSH keys to use
    pub ssh_keys: PathBuf,
}

impl GitSettings {
    /// Create a new [`GitSettings`]
    ///
    /// # Arguments
    ///
    /// * `ssh_keys` - The path to SSH keys to set
    #[must_use]
    pub fn new(ssh_keys: impl Into<PathBuf>) -> Self {
        Self {
            ssh_keys: ssh_keys.into(),
        }
    }
}

/// Help serde default our timeout to 600 seconds
pub fn default_client_timeout() -> u64 {
    600
}

/// The config options for our [`reqwest::Client`]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "python", thorium_derive::pyclass(get_all))]
pub struct ClientSettings {
    /// Ignore invalid certificates
    #[serde(default)]
    pub invalid_certs: bool,
    /// Ignore invalid hostnames when verifing certificates
    #[serde(default)]
    pub invalid_hostnames: bool,
    /// The certificate authorities to trust
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificate_authorities: Vec<PathBuf>,
    /// The number of seconds to wait before timing out
    #[serde(default = "default_client_timeout")]
    pub timeout: u64,
    /// Disable all proxies and connect to hosts directly
    ///
    /// When false (the default), proxy settings are read from the environment
    /// (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`, honoring `NO_PROXY`). Setting this to true
    /// ignores those and connects directly, which is useful for in-cluster traffic where
    /// every host is reachable without a proxy.
    #[serde(default)]
    pub disable_proxy: bool,
    /// An explicit proxy URL to route all (http + https) traffic through
    ///
    /// This takes precedence over environment proxy settings but is ignored when
    /// `disable_proxy` is true. The URL may embed credentials (e.g.
    /// `http://user:pass@host:port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// A comma-separated list of hosts/domains/CIDRs that bypass the explicit `proxy`
    ///
    /// This mirrors the `NO_PROXY` environment variable and only applies when `proxy` is
    /// set; environment-based proxies already honor `NO_PROXY` on their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
}

impl Default for ClientSettings {
    /// Default client settings to a sane default
    fn default() -> Self {
        ClientSettings {
            invalid_certs: false,
            invalid_hostnames: false,
            certificate_authorities: Vec::default(),
            timeout: default_client_timeout(),
            disable_proxy: false,
            proxy: None,
            no_proxy: None,
        }
    }
}

/// Provide a default default editor for serde
#[must_use]
pub fn default_default_editor() -> String {
    "vi".to_string()
}

/// The LLM providers Thorctl's AI features can talk to
///
/// This names which provider block in [`AISettings`] is active; each
/// provider's own settings live in the block named after it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum AiProviderKind {
    /// The OpenAI API (or any endpoint speaking its protocol)
    OpenAi,
    /// A LiteLLM (OpenAI-compatible) gateway
    LiteLlm,
    /// The Anthropic API
    Anthropic,
    /// AWS Bedrock, authenticating through the AWS credential chain
    Bedrock,
}

impl AiProviderKind {
    /// The lowercase config-file name of this provider's block
    #[must_use]
    pub fn name(self) -> &'static str {
        // match the serde rename so errors name the key the user writes
        match self {
            AiProviderKind::OpenAi => "openai",
            AiProviderKind::LiteLlm => "litellm",
            AiProviderKind::Anthropic => "anthropic",
            AiProviderKind::Bedrock => "bedrock",
        }
    }
}

impl std::fmt::Display for AiProviderKind {
    /// Format the provider as its config-file block name
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write the name into
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Optional pricing overrides for AI cost estimation, in dollars per million tokens
///
/// Thorctl ships a built-in price table for well-known models; this block
/// overrides it, or supplies prices for models the table does not know (e.g. a
/// LiteLLM alias). Each field is independent, so a partial block tweaks just
/// the rates it names:
///
/// ```yaml
/// ai:
///   litellm:
///     endpoint: http://gateway:4000
///     model: my-sonnet-alias
///     pricing: { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct AiPricing {
    /// $/MTok for fresh (uncached) input tokens
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// $/MTok for output tokens
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    /// $/MTok for input tokens read from the provider's prompt cache
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// $/MTok for input tokens written into the provider's prompt cache
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// The settings for talking to the OpenAI API
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OpenAiSettings {
    /// The base url to talk to, or `None` for the official OpenAI API
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The API key to authenticate with
    pub api_key: String,
    /// The model to talk to
    pub model: String,
    /// Optional cost-estimation pricing overrides for this model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<AiPricing>,
}

/// The settings for talking to a LiteLLM (OpenAI-compatible) gateway
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LiteLlmSettings {
    /// The base url of the gateway
    pub endpoint: String,
    /// The API key to authenticate with, or `None` for a no-auth gateway
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The model alias to talk to
    pub model: String,
    /// Optional cost-estimation pricing overrides for this model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<AiPricing>,
}

/// The settings for talking to the Anthropic API
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnthropicSettings {
    /// The base url to talk to, or `None` for the official Anthropic API
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The API key to authenticate with
    pub api_key: String,
    /// The model to talk to
    pub model: String,
    /// Optional cost-estimation pricing overrides for this model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<AiPricing>,
}

/// The settings for talking to AWS Bedrock
///
/// Bedrock authenticates through the AWS credential chain by default; an
/// explicit access key pair can be set to pin credentials instead. These
/// settings also steer which region/profile the chain resolves and whether to
/// talk to the Mantle (Anthropic Messages API) endpoint.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BedrockSettings {
    /// The AWS region to talk to Bedrock in, or `None` to use the chain's
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The AWS profile to resolve credentials from, or `None` for the default
    /// environment-driven chain (ignored when `region` is set)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Talk to Bedrock's Mantle (Anthropic Messages API) endpoint instead of
    /// the legacy Converse API; required for models only served there
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mantle: bool,
    /// An explicit AWS access key id, overriding the credential chain when
    /// paired with `secret_access_key`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    /// The secret access key for `access_key_id`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
    /// The session token accompanying the explicit access key pair, if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// The model id to talk to
    pub model: String,
    /// Optional cost-estimation pricing overrides for this model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<AiPricing>,
}

/// The active AI provider's settings, resolved from an [`AISettings`] block
///
/// This is the output of [`AISettings::resolve`]: exactly one provider with
/// its full settings, so a consumer never has to re-derive which block is
/// active.
#[derive(Debug, Clone)]
pub enum ResolvedAiProvider {
    /// The OpenAI API is active
    OpenAi(OpenAiSettings),
    /// A LiteLLM gateway is active
    LiteLlm(LiteLlmSettings),
    /// The Anthropic API is active
    Anthropic(AnthropicSettings),
    /// AWS Bedrock is active
    Bedrock(BedrockSettings),
}

impl ResolvedAiProvider {
    /// The model the active provider talks to
    #[must_use]
    pub fn model(&self) -> &str {
        // every provider block carries a model; hand back the active one's
        match self {
            ResolvedAiProvider::OpenAi(settings) => &settings.model,
            ResolvedAiProvider::LiteLlm(settings) => &settings.model,
            ResolvedAiProvider::Anthropic(settings) => &settings.model,
            ResolvedAiProvider::Bedrock(settings) => &settings.model,
        }
    }

    /// The active provider's cost-estimation pricing overrides, if any
    #[must_use]
    pub fn pricing(&self) -> Option<AiPricing> {
        // every provider block carries optional pricing; hand back the active one's
        match self {
            ResolvedAiProvider::OpenAi(settings) => settings.pricing,
            ResolvedAiProvider::LiteLlm(settings) => settings.pricing,
            ResolvedAiProvider::Anthropic(settings) => settings.pricing,
            ResolvedAiProvider::Bedrock(settings) => settings.pricing,
        }
    }
}

/// The settings to use when using AI
///
/// Which LLM provider to talk to is explicit: each provider's settings live in
/// a block named after it (`openai`, `litellm`, `anthropic`, `bedrock`) and
/// `provider` selects the active one (optional while only one block exists).
/// The legacy flat `endpoint`/`api_key`/`model` keys from before provider
/// blocks existed keep loading as a `litellm` provider.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AISettings {
    /// The provider to talk to, or `None` to infer it when unambiguous
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AiProviderKind>,
    /// The settings for the OpenAI API
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAiSettings>,
    /// The settings for a LiteLLM (OpenAI-compatible) gateway
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub litellm: Option<LiteLlmSettings>,
    /// The settings for the Anthropic API
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<AnthropicSettings>,
    /// The settings for AWS Bedrock
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock: Option<BedrockSettings>,
    /// The legacy endpoint to talk to an AI model at (treated as `litellm`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The legacy API key to use when talking to our AI model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The legacy model to use
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The Ghidra installation directory ThorPal analyzes binaries with
    ///
    /// Agents that use the Ghidra tools fail to start when this is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ghidra_install: Option<PathBuf>,
    /// The directory ThorPal creates/opens Ghidra projects under
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ghidra_projects: Option<PathBuf>,
    /// The path to a ThorPal workflow to use instead of the builtin one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<PathBuf>,
    /// The path to a markdown report template ThorPal uses instead of the builtin one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_template: Option<PathBuf>,
    /// The directory ThorPal's persistent cross-session Rhai script library
    /// lives in (defaults to `~/.thorium/scripts`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_dir: Option<PathBuf>,
    /// The directory ThorPal's persistent cross-run memory library lives in
    /// (defaults to `~/.thorium/memories`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memories_dir: Option<PathBuf>,
    /// A YARA rules file or directory ThorPal scans each sample with at seed
    /// time (matches become seeded findings); unset disables the scan
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yara_rules: Option<PathBuf>,
}

impl AISettings {
    /// The legacy flat `endpoint`/`api_key`/`model` keys as a litellm provider
    ///
    /// Configs written before provider blocks existed carried an
    /// OpenAI-compatible gateway as three flat keys; they keep loading as a
    /// `litellm` provider. `None` when the legacy trio is incomplete.
    fn legacy_litellm(&self) -> Option<LiteLlmSettings> {
        // the legacy block required all of endpoint, api_key, and model
        match (&self.endpoint, &self.api_key, &self.model) {
            (Some(endpoint), Some(api_key), Some(model)) => Some(LiteLlmSettings {
                endpoint: endpoint.clone(),
                api_key: Some(api_key.clone()),
                model: model.clone(),
                // the legacy trio predates pricing overrides
                pricing: None,
            }),
            _ => None,
        }
    }

    /// Migrate the legacy flat keys into a nested `litellm` block in place
    ///
    /// Loading never rewrites a legacy config, but when Thorctl is about to
    /// write the config back out anyway (e.g. `thorctl config`) the legacy
    /// flat `endpoint`/`api_key`/`model` trio is moved into a `litellm` block
    /// (unless one already exists) and the selector is set, so the written
    /// file uses the current shape. A no-op for already-migrated configs.
    pub fn migrate_legacy(&mut self) {
        // only migrate when the legacy trio is complete and litellm is free
        if self.litellm.is_none()
            && let Some(litellm) = self.legacy_litellm()
        {
            // move the legacy trio into its own provider block
            self.litellm = Some(litellm);
            // keep the active provider unchanged: the legacy trio was litellm
            self.provider.get_or_insert(AiProviderKind::LiteLlm);
            // drop the legacy keys so they are not written back out
            self.endpoint = None;
            self.api_key = None;
            self.model = None;
        }
    }

    /// Resolve the active provider and its settings from this block
    ///
    /// Resolution is explicit-first: a set `provider` selects its block (with
    /// the legacy flat keys satisfying `litellm` when its block is absent).
    /// Without a selector, a single configured block is unambiguous and wins;
    /// the legacy flat keys alone resolve as `litellm`. Anything else -- no
    /// provider configured at all, several blocks with no selector, or a
    /// selector naming a missing block -- is an error telling the user what to
    /// add.
    pub fn resolve(&self) -> Result<ResolvedAiProvider, Error> {
        // gather which provider blocks are actually configured
        let configured: Vec<AiProviderKind> = [
            self.openai.as_ref().map(|_| AiProviderKind::OpenAi),
            self.litellm.as_ref().map(|_| AiProviderKind::LiteLlm),
            self.anthropic.as_ref().map(|_| AiProviderKind::Anthropic),
            self.bedrock.as_ref().map(|_| AiProviderKind::Bedrock),
        ]
        .into_iter()
        .flatten()
        .collect();
        // determine the active provider kind
        let kind = match self.provider {
            // an explicit selector always wins
            Some(kind) => kind,
            // without a selector a single configured block is unambiguous
            None => match configured.as_slice() {
                [kind] => *kind,
                // the legacy flat keys alone resolve as a litellm provider
                [] if self.legacy_litellm().is_some() => AiProviderKind::LiteLlm,
                [] => {
                    return Err(Error::new(
                        "No AI provider is configured: add an `openai`, `litellm`, \
                        `anthropic`, or `bedrock` block to the `ai` section of your \
                        Thorctl config",
                    ));
                }
                many => {
                    // name the configured blocks so the fix is obvious
                    let names: Vec<&str> = many.iter().map(|kind| kind.name()).collect();
                    return Err(Error::new(format!(
                        "Multiple AI provider blocks are configured ({}): set \
                        `ai.provider` to the one to use",
                        names.join(", "),
                    )));
                }
            },
        };
        // hand back the selected provider's settings
        let resolved = match kind {
            AiProviderKind::OpenAi => self.openai.clone().map(ResolvedAiProvider::OpenAi),
            AiProviderKind::LiteLlm => self
                .litellm
                .clone()
                // the legacy flat keys satisfy a litellm selection too
                .or_else(|| self.legacy_litellm())
                .map(ResolvedAiProvider::LiteLlm),
            AiProviderKind::Anthropic => self.anthropic.clone().map(ResolvedAiProvider::Anthropic),
            AiProviderKind::Bedrock => self.bedrock.clone().map(ResolvedAiProvider::Bedrock),
        };
        // a selector naming a missing block is a config error
        resolved.ok_or_else(|| {
            Error::new(format!(
                "The AI provider is set to `{kind}` but the `ai` section has no \
                `{kind}` block: add one to your Thorctl config",
            ))
        })
    }
}

/// The container CLI tool thorctl shells out to for image operations
///
/// podman is CLI-compatible with docker for the verbs thorctl uses (pull, save,
/// load, tag, push, build), so the runtime only changes which binary is invoked.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ContainerRuntime {
    /// The docker CLI (`docker`)
    #[default]
    Docker,
    /// The podman CLI (`podman`)
    Podman,
}

impl ContainerRuntime {
    /// The executable name to invoke for this runtime
    #[must_use]
    pub fn binary(self) -> &'static str {
        // map each runtime to the CLI binary thorctl spawns
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}

impl std::fmt::Display for ContainerRuntime {
    /// Format the runtime as its binary name (used in user-facing messages)
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write the binary name into
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.binary())
    }
}

/// An activated scoped token for Thorctl to authenticate with
#[derive(Serialize, Deserialize, Clone)]
pub struct ActiveScopedToken {
    /// The name of the activated scoped token
    pub name: String,
    /// The value of the activated scoped token
    pub token: String,
}

impl std::fmt::Debug for ActiveScopedToken {
    /// Write this activated scoped token without its value
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never print the token's value itself
        f.debug_struct("ActiveScopedToken")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A config for running Thorctl in user mode
///
/// This will not give the user the ability to deploy clusters/agents
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CtlConf {
    /// The settings tied to talking to the API
    pub keys: Keys,
    /// The git settings to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSettings>,
    /// The settings for thorctls client
    #[serde(default)]
    pub client: ClientSettings,
    /// Skip the warning about possibly insecure connections
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_insecure_warning: Option<bool>,
    /// Skip automatic check for Thorctl updates with the API
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_update: Option<bool>,
    /// The default editor Thorctl will use
    #[serde(default = "default_default_editor")]
    pub default_editor: String,
    /// The container CLI thorctl uses for image operations (auto-detected when unset)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_runtime: Option<ContainerRuntime>,
    /// The settings to use when using AI
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AISettings>,
    /// The currently activated scoped token if one is active
    ///
    /// When set Thorctl will authenticate with this scoped token instead of
    /// the users primary credentials for all commands except scoped token
    /// management.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_token: Option<ActiveScopedToken>,
}

impl CtlConf {
    /// Create a new [`CtlConf`] from the given [`Keys`]
    ///
    /// # Arguments
    ///
    /// * `keys` - The Thorium keys to set
    #[must_use]
    pub fn new(keys: Keys) -> Self {
        Self {
            keys,
            git: None,
            skip_update: None,
            client: ClientSettings::default(),
            skip_insecure_warning: None,
            default_editor: default_default_editor(),
            container_runtime: None,
            ai: None,
            scoped_token: None,
        }
    }

    /// Check if our api url ends in '/api' and update it if needed
    ///
    /// # Arguments
    ///
    /// * `path` - The path to write our fixed config to
    fn fix_api_url(&mut self, path: &Path) -> Result<(), Error> {
        // check if our api url ends in api
        if self.keys.api.ends_with("/api") {
            // remove the /api from our config
            let trimmed = self.keys.api.trim_end_matches("/api");
            // update the api url in our config
            self.keys.api = trimmed.to_owned();
            // write the new configuration file
            let conf_file = std::fs::File::create(path)?;
            serde_norway::to_writer(conf_file, &self)?;
        }
        Ok(())
    }

    /// Assemble a [`CtlConf`] from a config file body overlaid with `THORCTL_*` environment values
    ///
    /// The file is the trusted, persistent configuration; the environment overlay is a convenience
    /// for ad-hoc overrides. Security-relevant toggles must NOT be silently flippable by the
    /// environment: `invalid_certs`/`invalid_hostnames` disable TLS validation and
    /// `skip_insecure_warning` suppresses the only interactive guardrail, so an attacker who can
    /// influence the environment (a hostile `.envrc`, CI job, or sourced script) without write
    /// access to the config file could otherwise disable certificate validation with no warning
    /// and no persistent record. Centralizing the overlay here lets the fix filter those
    /// keys out of the environment source in one place.
    ///
    /// # Arguments
    ///
    /// * `file_body` - The raw YAML body of the config file
    /// * `env` - The `THORCTL_*` environment values to overlay, already stripped of the prefix
    fn assemble_config(file_body: &str, env: config::Environment) -> Result<Self, Error> {
        // build the Thorctl config from the file body first, then the environment overlay
        let mut config: CtlConf = config::Config::builder()
            // load from the file body first
            .add_source(config::File::from_str(file_body, config::FileFormat::Yaml))
            // overlay the environment for ad-hoc, non-security-relevant overrides
            .add_source(env)
            .build()?
            .try_deserialize()?;
        // the `config` crate's environment source has no per-key filter, so instead parse the
        // trusted file body on its own and force the security-relevant toggles back to their file
        // values. This guarantees `client.invalid_certs`, `client.invalid_hostnames`, and
        // `skip_insecure_warning` can only be set in the persistent config file — the environment can
        // never silently disable TLS validation or suppress the insecure-connection warning.
        let file_only: CtlConf = config::Config::builder()
            .add_source(config::File::from_str(file_body, config::FileFormat::Yaml))
            .build()?
            .try_deserialize()?;
        // restore the TLS validation toggles from the trusted file
        config.client.invalid_certs = file_only.client.invalid_certs;
        config.client.invalid_hostnames = file_only.client.invalid_hostnames;
        // restore the insecure-warning suppressor from the trusted file
        config.skip_insecure_warning = file_only.skip_insecure_warning;
        Ok(config)
    }

    /// Loads a [`CtlConf`] from the given path
    ///
    /// # Arguments
    ///
    /// * `path` - The path to load this config from
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        // get the path to our config
        let path = path.as_ref();
        // read the raw config file body from disk, wrapping a failure with the
        // config path and the step that failed so the user is not left with a
        // bare "No such file or directory" that names neither; the I/O error
        // kind is kept so callers can still tell a missing file apart
        let file_body = std::fs::read_to_string(path).map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!(
                    "failed to read the thorctl config at '{}': {err}. Run `thorctl login` \
                     to create one, or pass --config with the path to an existing config",
                    path.display(),
                ),
            )
        })?;
        // build the environment overlay for THORCTL_* variables
        let env = config::Environment::with_prefix("THORCTL").separator("__");
        // assemble the config from the file body overlaid with the environment
        let mut config = Self::assemble_config(&file_body, env)?;
        // fix our config if needed
        config.fix_api_url(path)?;
        Ok(config)
    }
}

#[cfg(test)]
mod conf_tests {
    use super::CtlConf;

    /// Build a `THORCTL_*` environment overlay from an explicit key/value map
    ///
    /// This drives `assemble_config` deterministically without mutating the real process
    /// environment, matching how `from_path` builds its overlay (prefix `THORCTL`, `__` separator).
    ///
    /// # Arguments
    ///
    /// * `pairs` - The already-prefixed environment variable name/value pairs to overlay
    fn env_overlay(pairs: &[(&str, &str)]) -> config::Environment {
        // collect the pairs into the map the config crate expects
        let map = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        // build a THORCTL-prefixed overlay sourced from our explicit map
        config::Environment::with_prefix("THORCTL")
            .separator("__")
            .source(Some(map))
    }

    /// the `THORCTL_*` environment overlay must NOT be able to disable TLS certificate
    /// validation. A config file with validation ON must stay ON even when the environment tries to
    /// flip `client.invalid_certs`.
    #[test]
    fn env_cannot_disable_tls_validation() {
        // a minimal config file that keeps TLS validation enabled
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n";
        // an attacker-controlled environment that tries to accept invalid certificates. The overlay
        // uses the `__` prefix separator `from_path` configures, so the key is `THORCTL__CLIENT__...`
        let env = env_overlay(&[("THORCTL__CLIENT__INVALID_CERTS", "true")]);
        // assemble the config exactly as `from_path` would
        let conf = CtlConf::assemble_config(file_body, env)
            .expect("config with a valid file body should assemble");
        // the environment must not have been able to disable certificate validation
        assert!(
            !conf.client.invalid_certs,
            "THORCTL_CLIENT__INVALID_CERTS disabled TLS validation via the environment"
        );
    }

    /// the `THORCTL_*` environment overlay must NOT be able to suppress the interactive
    /// insecure-connection warning, which is the only guardrail left when validation is off.
    #[test]
    fn env_cannot_suppress_insecure_warning() {
        // a minimal config file that leaves the insecure warning enabled
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n";
        // an attacker-controlled environment that tries to suppress the insecure warning
        let env = env_overlay(&[("THORCTL__SKIP_INSECURE_WARNING", "true")]);
        // assemble the config exactly as `from_path` would
        let conf = CtlConf::assemble_config(file_body, env)
            .expect("config with a valid file body should assemble");
        // the environment must not have been able to suppress the insecure warning
        assert!(
            !conf.skip_insecure_warning.unwrap_or(false),
            "THORCTL_SKIP_INSECURE_WARNING suppressed the warning via the environment"
        );
    }

    /// An activated scoped token and auth keys keep their token values and passwords out of their
    /// debug output while printing their other fields
    #[test]
    fn token_values_stay_out_of_debug() {
        // an activated scoped token with a recognizable value
        let value = "tok-ganymede-8c2a71";
        let active = super::ActiveScopedToken {
            name: "harvester".to_owned(),
            token: value.to_owned(),
        };
        let printed = format!("{active:?}");
        assert!(
            !printed.contains(value),
            "active token value printed: {}",
            printed.len()
        );
        assert!(printed.contains("harvester"));
        // keys holding a token, as a run's keys file does
        let keys = crate::client::Keys::new_token("https://thorium.example", value);
        let printed = format!("{keys:?}");
        assert!(
            !printed.contains(value),
            "keys token value printed: {}",
            printed.len()
        );
        assert!(printed.contains("https://thorium.example"));
        // keys holding a username and password for basic auth
        let password = "pw-himalia-91f0c3";
        let keys = crate::client::Keys {
            api: "https://thorium.example".to_owned(),
            username: Some("himalia".to_owned()),
            password: Some(password.to_owned()),
            token: None,
        };
        let printed = format!("{keys:?}");
        assert!(
            !printed.contains(password),
            "keys password printed: {}",
            printed.len()
        );
        assert!(printed.contains("himalia"));
    }

    /// A client config that can't be read reports its path and keeps the I/O error's kind
    #[test]
    fn missing_config_names_its_path() {
        // a path under a temp directory that is never created, so the file can't exist
        let path = std::env::temp_dir()
            .join(format!("thorium-missing-conf-{}", std::process::id()))
            .join("callisto.yml");
        // try to load the missing config
        let err = CtlConf::from_path(&path).expect_err("a missing config should not load");
        // the error message must name the file that couldn't be read
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "error does not name the config path: {err}"
        );
        // the error must stay an I/O error whose kind is still NotFound
        match err {
            crate::Error::IO(io_err) => assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected an I/O error, got {other}"),
        }
    }

    /// an `ai` block written before the ThorPal settings existed must still load,
    /// leaving the Ghidra install unset
    #[test]
    fn ai_block_without_ghidra_loads() {
        // a config file whose ai block only has the original fields
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  endpoint: https://llm.example\n  api_key: k\n  model: m\n";
        // assemble the config with no environment overrides
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with a legacy ai block should assemble");
        // the ai block loads with no ThorPal settings
        let ai = conf.ai.expect("ai block should be present");
        assert!(ai.ghidra_install.is_none());
        assert!(ai.ghidra_projects.is_none());
        assert!(ai.workflow.is_none());
    }

    /// the ThorPal settings in the `ai` block load from the file and the Ghidra
    /// install can be overridden from the environment
    #[test]
    fn ai_block_ghidra_settings_load() {
        // a config file whose ai block sets every ThorPal setting
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  endpoint: https://llm.example\n  api_key: k\n  model: m\n  \
                         ghidra_install: /opt/ghidra\n  ghidra_projects: /tmp/projects\n  \
                         workflow: /tmp/workflow.yaml\n";
        // assemble the config with no environment overrides
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with ThorPal settings should assemble");
        let ai = conf.ai.expect("ai block should be present");
        // every ThorPal setting is read from the file
        assert_eq!(
            ai.ghidra_install.as_deref(),
            Some(std::path::Path::new("/opt/ghidra"))
        );
        assert_eq!(
            ai.ghidra_projects.as_deref(),
            Some(std::path::Path::new("/tmp/projects"))
        );
        assert_eq!(
            ai.workflow.as_deref(),
            Some(std::path::Path::new("/tmp/workflow.yaml"))
        );
        // the environment may point at a different Ghidra install
        let env = env_overlay(&[("THORCTL__AI__GHIDRA_INSTALL", "/usr/share/ghidra")]);
        let conf = CtlConf::assemble_config(file_body, env)
            .expect("config with a ghidra env override should assemble");
        let ai = conf.ai.expect("ai block should be present");
        assert_eq!(
            ai.ghidra_install.as_deref(),
            Some(std::path::Path::new("/usr/share/ghidra"))
        );
    }

    /// a legacy flat `ai` block (endpoint/api_key/model) resolves as a litellm
    /// provider so configs from before provider blocks existed keep working
    #[test]
    fn legacy_flat_ai_block_resolves_as_litellm() {
        // a config file whose ai block only has the legacy flat keys
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  endpoint: https://llm.example\n  api_key: k\n  model: m\n";
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with a legacy ai block should assemble");
        let ai = conf.ai.expect("ai block should be present");
        // the legacy trio resolves silently as a litellm provider
        match ai.resolve().expect("legacy flat keys should resolve") {
            super::ResolvedAiProvider::LiteLlm(litellm) => {
                assert_eq!(litellm.endpoint, "https://llm.example");
                assert_eq!(litellm.api_key.as_deref(), Some("k"));
                assert_eq!(litellm.model, "m");
            }
            other => panic!("legacy flat keys resolved to the wrong provider: {other:?}"),
        }
    }

    /// a nested provider block loads and the `provider` selector picks it
    #[test]
    fn nested_provider_block_resolves() {
        // a config file with two provider blocks and an explicit selector
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  provider: anthropic\n  anthropic:\n    api_key: ak\n    model: am\n  \
                         litellm:\n    endpoint: https://llm.example\n    model: lm\n";
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with nested provider blocks should assemble");
        let ai = conf.ai.expect("ai block should be present");
        // the selector picks the anthropic block over the litellm one
        match ai.resolve().expect("the selected provider should resolve") {
            super::ResolvedAiProvider::Anthropic(anthropic) => {
                assert_eq!(anthropic.api_key, "ak");
                assert_eq!(anthropic.model, "am");
                assert!(anthropic.endpoint.is_none());
            }
            other => panic!("the selector picked the wrong provider: {other:?}"),
        }
    }

    /// the `THORCTL__AI__PROVIDER` environment variable switches the active
    /// provider and `THORCTL__AI__<PROVIDER>__*` overrides its settings
    #[test]
    fn provider_env_overrides_work() {
        // a config file selecting litellm with an anthropic block also present
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  provider: litellm\n  anthropic:\n    api_key: ak\n    model: am\n  \
                         litellm:\n    endpoint: https://llm.example\n    model: lm\n";
        // the environment flips the selector and overrides the anthropic key
        let env = env_overlay(&[
            ("THORCTL__AI__PROVIDER", "anthropic"),
            ("THORCTL__AI__ANTHROPIC__API_KEY", "env-key"),
        ]);
        let conf = CtlConf::assemble_config(file_body, env)
            .expect("config with provider env overrides should assemble");
        let ai = conf.ai.expect("ai block should be present");
        // the env-selected provider resolves with the env-overridden key
        match ai
            .resolve()
            .expect("the env-selected provider should resolve")
        {
            super::ResolvedAiProvider::Anthropic(anthropic) => {
                assert_eq!(anthropic.api_key, "env-key");
                assert_eq!(anthropic.model, "am");
            }
            other => panic!("the env selector picked the wrong provider: {other:?}"),
        }
    }

    /// several provider blocks with no selector is ambiguous, and a selector
    /// naming a missing block is an error naming that block
    #[test]
    fn provider_resolution_errors() {
        // two provider blocks and no selector cannot be resolved
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  anthropic:\n    api_key: ak\n    model: am\n  \
                         litellm:\n    endpoint: https://llm.example\n    model: lm\n";
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with two provider blocks should assemble");
        let err = conf.ai.expect("ai block should be present").resolve();
        assert!(err.is_err(), "an ambiguous provider resolved anyway");
        // a selector naming a block that does not exist is an error naming it
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  provider: bedrock\n  litellm:\n    \
                         endpoint: https://llm.example\n    model: lm\n";
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with a dangling selector should assemble");
        let err = conf
            .ai
            .expect("ai block should be present")
            .resolve()
            .expect_err("a selector naming a missing block should error");
        assert!(err.to_string().contains("bedrock"));
    }

    /// migrating a legacy flat ai block moves the trio into a `litellm` block,
    /// sets the selector, and drops the flat keys; already-nested configs are
    /// left alone
    #[test]
    fn legacy_flat_ai_block_migrates_on_demand() {
        // a config file whose ai block only has the legacy flat keys
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  endpoint: https://llm.example\n  api_key: k\n  model: m\n";
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with a legacy ai block should assemble");
        let mut ai = conf.ai.expect("ai block should be present");
        // migrate the legacy keys as the config writers do before serializing
        ai.migrate_legacy();
        // the trio moved into a litellm block and the selector was set
        let litellm = ai.litellm.as_ref().expect("litellm block should exist");
        assert_eq!(litellm.endpoint, "https://llm.example");
        assert_eq!(litellm.api_key.as_deref(), Some("k"));
        assert_eq!(litellm.model, "m");
        assert_eq!(ai.provider, Some(super::AiProviderKind::LiteLlm));
        // the legacy flat keys were dropped so they are never written back out
        assert!(ai.endpoint.is_none());
        assert!(ai.api_key.is_none());
        assert!(ai.model.is_none());
        // a second migration is a no-op
        ai.migrate_legacy();
        assert_eq!(ai.litellm.as_ref().map(|l| l.model.as_str()), Some("m"));
    }

    /// a config with only nested provider blocks round-trips through YAML
    /// without resurrecting legacy flat keys or emitting empty blocks
    #[test]
    fn nested_ai_block_round_trips_cleanly() {
        // a config file carrying a single nested provider block
        let file_body = "keys:\n  api: https://thorium.example\n  username: analyst\n  token: t\n\
                         ai:\n  provider: litellm\n  litellm:\n    \
                         endpoint: https://llm.example\n    model: lm\n";
        let conf = CtlConf::assemble_config(file_body, env_overlay(&[]))
            .expect("config with a nested provider block should assemble");
        // serialize the config back out exactly as the config writers do
        let yaml = serde_norway::to_string(&conf).expect("config should serialize");
        // no legacy flat keys and no absent provider blocks appear in the output
        assert!(!yaml.contains("endpoint: null"));
        assert!(!yaml.contains("api_key: null"));
        assert!(!yaml.contains("openai"));
        assert!(!yaml.contains("anthropic"));
        assert!(!yaml.contains("bedrock"));
        // the nested block and selector both survive the round-trip
        assert!(yaml.contains("provider: litellm"));
        assert!(yaml.contains("litellm:"));
    }
}
