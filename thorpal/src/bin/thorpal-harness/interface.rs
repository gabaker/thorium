//! The files Thorium hands the harness, the checks a run must pass before its first model call,
//! and the conversion of the run's model into `ThorPal`'s provider config

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use thorium::client::Keys;
use thorium::models::{
    AgenticToolBacking, CredentialType, HarnessAgentFile, HarnessCapabilities, HarnessInputs,
    HarnessModelEntry, HarnessModels, HarnessOutputs, HarnessRunFile, HarnessToolEntry, ModelApi,
    ModelProvider, ModelProviderKind, ToolInvocation, ToolMechanism,
};
use thorpal::AiProviderConfig;
use uuid::Uuid;

/// The directory the harness writes its conversation and traces to unless told otherwise
pub const DEFAULT_RESULT_FILES: &str = "/tmp/thorium/result-files";

/// The directory the thorium-agent downloads a run's samples to unless told otherwise
pub const DEFAULT_SAMPLES: &str = "/tmp/thorium/samples";

/// The env var in the harness image that sets the run timeout in seconds
pub const TIMEOUT_ENV: &str = "THORPAL_HARNESS_TIMEOUT_SECS";

/// The run timeout in seconds when the image doesn't set one
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// The prompt the entry agent gets when its snapshot has none
const DEFAULT_PROMPT: &str = "Analyze the input file.";

/// Where the harness reads its inputs and writes its outputs
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessPaths {
    /// The run request, agent, tools, models, and keys files
    pub inputs: HarnessInputs,
    /// The answer and status files
    pub outputs: HarnessOutputs,
    /// The directory the conversation and traces are written to
    pub result_files: PathBuf,
    /// The directory the run's samples were downloaded to
    pub samples: PathBuf,
}

impl Default for HarnessPaths {
    /// The standard Thorium layout
    fn default() -> Self {
        HarnessPaths {
            inputs: HarnessInputs::default(),
            outputs: HarnessOutputs::default(),
            result_files: PathBuf::from(DEFAULT_RESULT_FILES),
            samples: PathBuf::from(DEFAULT_SAMPLES),
        }
    }
}

impl HarnessPaths {
    /// Read the paths from the harness's command line, keeping the standard layout for any path
    /// that isn't given
    ///
    /// # Arguments
    ///
    /// * `args` - The command line arguments after the program name
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        // start from the standard layout
        let mut paths = HarnessPaths::default();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            // every flag takes one value
            let Some(value) = args.next() else {
                return Err(format!("{flag} needs a value"));
            };
            // put the value where its flag says
            match flag.as_str() {
                "--run" => paths.inputs.run_request.clone_from(value),
                "--agent" => paths.inputs.agent.clone_from(value),
                "--tools" => paths.inputs.tools.clone_from(value),
                "--models" => paths.inputs.models.clone_from(value),
                "--keys" => paths.inputs.thorium_keys.clone_from(value),
                "--results" => paths.outputs.answer.clone_from(value),
                "--status" => paths.outputs.status.clone_from(value),
                "--result-files" => paths.result_files = PathBuf::from(value),
                "--samples" => paths.samples = PathBuf::from(value),
                _ => return Err(format!("unknown argument {flag}")),
            }
        }
        Ok(paths)
    }
}

/// The files Thorium wrote for a run
#[derive(Debug, Clone)]
pub struct RunFiles {
    /// The run request (`run.json`)
    pub run: HarnessRunFile,
    /// The entry agent and its sub-agents (`agent.json`)
    pub agent: HarnessAgentFile,
    /// Every bound tool by tool instance id (`tools.json`)
    pub tools: BTreeMap<Uuid, HarnessToolEntry>,
    /// Every agent's model and credential (the models file)
    pub models: HarnessModels,
    /// The Thorium API url and the run's token (the keys file)
    pub keys: Keys,
}

/// Read and parse one JSON file, naming the file in any error
///
/// A file that holds credentials reports a parse error by its place and kind only, since serde's
/// text can quote the value it refused, and errors reach the logs and `status.json`.
///
/// # Arguments
///
/// * `path` - The file to read
/// * `secret` - Whether the file holds credentials
fn read_json<T: serde::de::DeserializeOwned>(path: &str, secret: bool) -> Result<T, String> {
    // read the file, then parse it
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("unable to read {path}: {error}"))?;
    serde_json::from_str(&text).map_err(|error| {
        if secret {
            parse_failure(path, &error)
        } else {
            format!("unable to parse {path}: {error}")
        }
    })
}

/// Describe a parse error of a file that holds credentials without any of the file's text
///
/// # Arguments
///
/// * `path` - The file that didn't parse
/// * `error` - Why it didn't parse
fn parse_failure(path: &str, error: &serde_json::Error) -> String {
    // the error's kind and place, never its message
    let kind = match error.classify() {
        serde_json::error::Category::Io => "a read error",
        serde_json::error::Category::Syntax => "invalid JSON",
        serde_json::error::Category::Data => "a value of the wrong type or shape",
        serde_json::error::Category::Eof => "an early end of the file",
    };
    format!(
        "unable to parse {path}: {kind} at line {} column {} (the file holds credentials, so the \
         rest of the error is withheld)",
        error.line(),
        error.column()
    )
}

/// Read the keys file, keeping its contents out of any error
///
/// # Arguments
///
/// * `path` - The keys file
fn read_keys(path: &str) -> Result<Keys, String> {
    // a file that can't be read says why, which names no contents
    std::fs::metadata(path).map_err(|error| format!("unable to read {path}: {error}"))?;
    // the parser's text can quote a value, and the file holds the run's token
    Keys::from_path(path).map_err(|_| {
        format!(
            "unable to parse {path}: it isn't a keys file with the API url and a token (the file \
             holds credentials, so the parser's error is withheld)"
        )
    })
}

impl RunFiles {
    /// Read every file Thorium wrote for a run
    ///
    /// # Arguments
    ///
    /// * `inputs` - Where the files are
    pub fn load(inputs: &HarnessInputs) -> Result<Self, String> {
        // read the run, agent, tools, and models files
        let run = read_json(&inputs.run_request, false)?;
        let agent = read_json(&inputs.agent, false)?;
        let tools = read_json(&inputs.tools, false)?;
        let models = read_json(&inputs.models, true)?;
        // read the keys file
        let keys = read_keys(&inputs.thorium_keys)?;
        Ok(RunFiles {
            run,
            agent,
            tools,
            models,
            keys,
        })
    }
}

/// What the harness can do, as its Harness tool declares it
#[must_use]
pub fn capabilities() -> HarnessCapabilities {
    // one mechanism, one model API, and one credential kind
    HarnessCapabilities {
        mechanisms: BTreeSet::from([ToolMechanism::ThoriumApi]),
        file_io: true,
        model_apis: BTreeSet::from([ModelApi::AnthropicMessages]),
        credential_kinds: BTreeSet::from([CredentialType::ApiKey]),
        per_agent_models: false,
    }
}

/// Why a run's model can't be turned into a `ThorPal` provider config
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionError {
    /// The harness can't call models with this protocol
    Api(ModelApi),
    /// The harness can't reach this provider
    Provider(ModelProviderKind),
    /// The harness can't authenticate with this kind of credential
    Credential(CredentialType),
    /// The model has no credential
    NoCredential,
    /// The credential has no secret
    NoSecret,
}

impl std::fmt::Display for ConversionError {
    /// Write this error, naming what isn't supported
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // name the protocol, provider, or credential kind
        match self {
            ConversionError::Api(api) => write!(
                f,
                "thorpal-harness can't call models with the {} model API; it supports only AnthropicMessages",
                api.name()
            ),
            ConversionError::Provider(provider) => write!(
                f,
                "thorpal-harness can't call models of the {provider} provider; it supports only Anthropic"
            ),
            ConversionError::Credential(kind) => write!(
                f,
                "thorpal-harness can't authenticate with a {kind} credential; it supports only ApiKey"
            ),
            ConversionError::NoCredential => write!(
                f,
                "thorpal-harness needs an ApiKey credential, but the model has no credential"
            ),
            ConversionError::NoSecret => write!(
                f,
                "thorpal-harness needs an ApiKey credential, but the credential has no secret"
            ),
        }
    }
}

/// Turn a run's model into `ThorPal`'s provider config
///
/// # Arguments
///
/// * `entry` - The model from the models file
pub fn provider_config(entry: &HarnessModelEntry) -> Result<AiProviderConfig, ConversionError> {
    // the protocol must be one the harness declares
    let declared = capabilities();
    if !declared.model_apis.contains(&entry.api) {
        return Err(ConversionError::Api(entry.api));
    }
    // ThorPal's Anthropic client takes only the base url from the provider
    let ModelProvider::Anthropic { base_url, .. } = &entry.provider else {
        return Err(ConversionError::Provider(ModelProviderKind::from(
            &entry.provider,
        )));
    };
    // the credential must be a kind the harness declares and hold a secret
    let Some(credential) = &entry.credential else {
        return Err(ConversionError::NoCredential);
    };
    let kind = credential.kind.cred_type();
    if !declared.credential_kinds.contains(&kind) {
        return Err(ConversionError::Credential(kind));
    }
    let Some(secret) = credential
        .secrets
        .primary
        .as_ref()
        .filter(|secret| !secret.is_empty())
    else {
        return Err(ConversionError::NoSecret);
    };
    Ok(AiProviderConfig::Anthropic {
        endpoint: base_url.clone(),
        api_key: secret.expose().to_owned(),
        model: entry.model_id.clone(),
    })
}

/// What a run gives `ThorPal` once every start check passes
#[derive(Debug, Clone)]
pub struct RunPlan {
    /// The provider config every agent talks to
    pub provider: AiProviderConfig,
    /// The entry agent's model, for rendering its tools
    pub model: HarnessModelEntry,
    /// The entry agent's tools, by tool instance id, in tools.json order
    pub tools: Vec<(Uuid, HarnessToolEntry)>,
    /// The sample `ThorPal` analyzes
    pub binary: PathBuf,
    /// The samples `ThorPal` isn't given
    pub skipped: Vec<String>,
    /// The entry agent's system prompt, if it has one
    pub preamble: Option<String>,
    /// The entry agent's task prompt with the run's input added
    pub prompt: String,
}

/// Check that `ThorPal` can run this run and build what it needs, before any model call
///
/// # Arguments
///
/// * `files` - The run's files
/// * `samples` - The directory the run's samples were downloaded to
pub fn plan(files: &RunFiles, samples: &Path) -> Result<RunPlan, String> {
    // ThorPal analyzes one file, so a run without one can't start
    let Some((first, rest)) = files.run.samples.split_first() else {
        return Err("ThorPal needs a sample to analyze, but the run has no input files".to_owned());
    };
    // samples are named by SHA256 and joined onto the samples directory, so each must be one
    if let Some(bad) = files
        .run
        .samples
        .iter()
        .find(|sha| sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(format!(
            "the run's input {bad:?} isn't a SHA256 (64 hexadecimal characters)"
        ));
    }
    // sub-agents aren't run by this harness
    let entry = &files.agent.agent;
    if !files.agent.sub_agents.is_empty() {
        let names: Vec<&str> = files
            .agent
            .sub_agents
            .iter()
            .map(|sub| sub.name.as_str())
            .collect();
        return Err(format!(
            "thorpal-harness doesn't delegate to sub-agents, but agent {} delegates to {}",
            entry.name,
            names.join(", ")
        ));
    }
    // collect the entry agent's tools in tools.json order
    let bound: BTreeSet<Uuid> = entry
        .bindings
        .iter()
        .map(|binding| binding.tool_instance)
        .collect();
    let tools: Vec<(Uuid, HarnessToolEntry)> = files
        .tools
        .iter()
        .filter(|(id, _)| bound.contains(id))
        .map(|(id, tool)| (*id, tool.clone()))
        .collect();
    // tools that need approval can't run without a user
    let approval: Vec<&str> = tools
        .iter()
        .filter(|(_, tool)| tool.requires_approval)
        .map(|(_, tool)| tool.function_name.as_str())
        .collect();
    if !approval.is_empty() {
        return Err(format!(
            "thorpal-harness can't ask a user to approve tool calls, so it refuses tools that require approval: {}",
            approval.join(", ")
        ));
    }
    // only tools reached through Thorium's API are dispatched
    for (_, tool) in &tools {
        let kind = match &tool.invocation {
            ToolInvocation::ThoriumApi {
                backing: AgenticToolBacking::HarnessNative { .. },
                ..
            } => Some("a ThoriumApi binding of a native tool"),
            ToolInvocation::ThoriumApi { .. } => None,
            ToolInvocation::Native { .. } => Some("Native"),
            ToolInvocation::Cli { .. } => Some("Cli"),
            ToolInvocation::Mcp { .. } => Some("Mcp"),
        };
        if let Some(kind) = kind {
            return Err(format!(
                "thorpal-harness calls tools only through Thorium's API, but tool {} is bound as {kind}",
                tool.function_name
            ));
        }
    }
    // the entry agent's model becomes ThorPal's provider
    let Some(model) = files.models.agents.get(&entry.agent) else {
        return Err(format!(
            "the models file has no model for agent {}",
            entry.name
        ));
    };
    let provider = provider_config(model).map_err(|error| error.to_string())?;
    // the entry agent's prompt names the sample ThorPal analyzes
    let binary = samples.join(first);
    let task = entry.config.prompt.as_deref().unwrap_or(DEFAULT_PROMPT);
    let prompt = format!(
        "{task}\n\nThe input file has SHA256 {first} and is at {}.",
        binary.display()
    );
    Ok(RunPlan {
        provider,
        model: model.clone(),
        tools,
        binary,
        skipped: rest.to_vec(),
        preamble: entry.config.system_prompt.clone(),
        prompt,
    })
}

/// The run status the harness writes to `status.json`
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct HarnessStatus {
    /// Whether the run finished with an answer
    pub success: bool,
    /// Why the run failed, if it did
    pub reason: Option<String>,
    /// How many model calls were made
    pub model_calls: u64,
    /// How many tool calls were made
    pub tool_calls: u64,
    /// The input tokens of every model call
    pub input_tokens: u64,
    /// The output tokens of every model call
    pub output_tokens: u64,
}

impl HarnessStatus {
    /// Build the status of a run that failed before its first model call
    ///
    /// # Arguments
    ///
    /// * `reason` - Why it failed
    #[must_use]
    pub fn refused<T: Into<String>>(reason: T) -> Self {
        // only the reason is set
        HarnessStatus {
            reason: Some(reason.into()),
            ..HarnessStatus::default()
        }
    }
}

/// Write a file, creating its directory first
///
/// # Arguments
///
/// * `path` - The file to write
/// * `contents` - What to write
pub fn write_file<P: AsRef<Path>>(path: P, contents: &[u8]) -> Result<(), String> {
    // make sure the directory exists, then write the file
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("unable to create {}: {error}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .map_err(|error| format!("unable to write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests;
