//! Contains models for tools, their immutable instances, and their versions
//!
//! A [`ToolEntity`] is the editable root of a tool; every version of its definition is an
//! immutable [`ToolInstanceEntity`] holding a [`ToolSpec`]. Agentic tools describe a function a
//! model can call, Image tools embed a [`WorkloadSpec`] that runs as Thorium jobs, and Harness
//! tools package the program that runs an agent's model and tool loop.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use strum::{AsRefStr, EnumDiscriminants, EnumString};
use uuid::Uuid;

use crate::models::{
    ArgStrategy, CredentialType, GenericJobArgs, ModelApi, WorkloadEnvironment, WorkloadSpec,
};

mod descriptors;
mod harness_env;
#[cfg(any(feature = "api", feature = "client"))]
mod portability;
#[cfg(test)]
mod tests;

pub use descriptors::{
    ApprovalMode, HarnessAgentFile, HarnessRunFile, HarnessToolEntry, ToolAnnotations,
    ToolApproval, ToolExposure, ToolInvocation, ToolServerAuth, ToolServerDescriptor,
    ToolServerInput, ToolServerTimeouts, ToolServerToolOverride, ToolServerTools,
    ToolServerTransport, ToolServerTrust, ToolServerValue, ToolServersFile, is_valid_server_id,
};
pub use harness_env::{HarnessEnv, HarnessEnvVar};

#[cfg(any(feature = "api", feature = "client"))]
pub use portability::{
    MAX_ICON_SIZES, MAX_ICON_SRC_BYTES, MAX_INPUT_EXAMPLE_BYTES, MAX_INPUT_EXAMPLES,
    MAX_INPUT_EXAMPLES_BYTES, MAX_SCHEMA_DEPTH, MAX_SCHEMA_PATTERNS, MAX_TOOL_ICONS,
};

/// The default number of bytes a tool's serialized input or output schema may take up
pub const DEFAULT_MAX_TOOL_SCHEMA_BYTES: usize = 65_536;

/// The longest name a model can call an agentic tool by
const MAX_FUNCTION_NAME_LEN: usize = 64;

/// The most bytes a harness MCP config file's path may take up
const MAX_MCP_CONFIG_PATH: usize = 1024;

/// The directory the thorium-agent uploads result files from
const RESULT_FILES_DIR: &str = "/tmp/thorium/result-files";

/// A version of a tool, formatted according to various standards
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub enum ToolVersion {
    /// A semantic version
    #[cfg_attr(feature = "api", schema(value_type = String))]
    SemVer(semver::Version),
    /// A version label in any other format
    Custom(String),
}

impl From<&str> for ToolVersion {
    /// Parse a version label as semver, falling back to a custom label
    ///
    /// # Arguments
    ///
    /// * `s` - The version label to convert
    fn from(s: &str) -> Self {
        // check if the string can be parsed to semver
        if let Ok(semver) = semver::Version::parse(s) {
            ToolVersion::SemVer(semver)
        // otherwise convert to a custom version
        } else {
            ToolVersion::Custom(s.to_string())
        }
    }
}

impl From<&String> for ToolVersion {
    /// Parse a version label as semver, falling back to a custom label
    ///
    /// # Arguments
    ///
    /// * `s` - The version label to convert
    fn from(s: &String) -> Self {
        // convert the label the same way as a str
        ToolVersion::from(s.as_str())
    }
}

impl fmt::Display for ToolVersion {
    /// Write this version as the label it was parsed from
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the label in its own format
        match self {
            ToolVersion::SemVer(version) => write!(f, "{version}"),
            ToolVersion::Custom(label) => write!(f, "{label}"),
        }
    }
}

impl ToolVersion {
    /// Parse the version label of a new tool instance, refusing an empty label
    ///
    /// # Arguments
    ///
    /// * `raw` - The version label to parse
    pub fn parse_label(raw: &str) -> Result<Self, String> {
        // an empty label can't tell versions apart
        if raw.is_empty() {
            return Err("tool_version can't be empty".to_owned());
        }
        // parse the label as semver or keep it as a custom label
        Ok(ToolVersion::from(raw))
    }
}

/// An editable tool whose versions are immutable tool instances
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolEntity {
    /// The kind of tool this is (fixed for the life of the tool)
    pub kind: ToolKind,
    /// The ID of the newest non-deprecated instance of this tool
    pub latest: Option<Uuid>,
    /// The version label of the newest non-deprecated instance of this tool
    pub latest_version: Option<ToolVersion>,
    /// The spec hash of the newest non-deprecated instance of this tool
    pub latest_spec_hash: Option<String>,
    /// When this tool was last modified (any edit, including a new `latest`)
    pub modified: DateTime<Utc>,
    /// The instances of this tool newest first (populated on get, not stored)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<ToolInstanceSummary>,
    /// The values the thorium-agent exports to this harness as env vars (Harness tools only;
    /// editable, not versioned or hashed, and read when a run starts)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_env: Option<HarnessEnv>,
    /// Exact model ids no run of this harness may resolve to, at any level (Harness tools only;
    /// editable, not versioned or hashed, and read when a run starts)
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub disabled_models: BTreeSet<String>,
}

impl ToolEntity {
    /// Build the root of a new tool of a kind, which has no instances yet
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of tool
    /// * `modified` - When the tool was created
    #[must_use]
    pub fn new(kind: ToolKind, modified: DateTime<Utc>) -> Self {
        // a new tool has no latest instance and no harness settings
        ToolEntity {
            kind,
            latest: None,
            latest_version: None,
            latest_spec_hash: None,
            modified,
            instances: Vec::new(),
            harness_env: None,
            disabled_models: BTreeSet::new(),
        }
    }
}

/// A brief summary of a single tool instance
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolInstanceSummary {
    /// The ID of this tool instance
    pub id: Uuid,
    /// The version label of this tool instance
    pub version: ToolVersion,
    /// The content hash of this tool instance
    pub spec_hash: String,
    /// Whether this tool instance is deprecated
    pub deprecated: bool,
    /// When this tool instance was created
    pub created: DateTime<Utc>,
}

/// A single immutable version of a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolInstanceEntity {
    /// The ID of the tool this is an instance of
    pub tool: Uuid,
    /// The version label of this instance
    pub version: ToolVersion,
    /// The ID of the instance this instance supersedes
    pub previous: Option<Uuid>,
    /// The content hash of this instance's spec
    pub spec_hash: String,
    /// Whether this instance is deprecated and should no longer be resolved as latest
    #[serde(default)]
    pub deprecated: bool,
    /// The kind-specific definition of this tool version
    pub spec: ToolSpec,
    /// How portable an agentic spec is across model APIs (computed on get, never stored)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portability: Option<ToolPortabilityReport>,
    /// The native agentic tool instances a Harness instance provides, sorted (filled in from its
    /// `Provides` edges on get, never stored)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<Uuid>,
}

impl ToolInstanceEntity {
    /// Fill in the portability report of an agentic spec
    ///
    /// Called only on responses, so the report is never written back to storage.
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn fill_portability(&mut self) {
        // only agentic specs have a portability report
        self.portability = self.spec.portability();
    }
}

/// The specific kind a tool is, including any data unique to its kind
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, EnumDiscriminants)]
// a spec is stored and hashed as one value, so its larger variant isn't boxed
#[allow(clippy::large_enum_variant)]
// generate a type just containing the tool kind's name with no data
#[strum_discriminants(name(ToolKind))]
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
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolSpec {
    /// A tool exposed to AI agents and called dynamically by a model
    Agentic(AgenticTool),
    /// A static tool run as Thorium jobs in any workload environment
    Image(WorkloadSpec),
    /// A swappable agent harness that runs an agent's model and tool loop
    Harness(HarnessSpec),
}

impl Default for ToolSpec {
    /// The default spec only anchors digest pruning; validation still requires every field
    fn default() -> Self {
        ToolSpec::Agentic(AgenticTool::default())
    }
}

impl ToolSpec {
    /// Get the kind of tool this spec defines
    #[must_use]
    pub fn kind(&self) -> ToolKind {
        // the kind is this spec's discriminant
        ToolKind::from(self)
    }

    /// Parse the JSON encoded spec a tool request carries
    ///
    /// # Arguments
    ///
    /// * `raw` - The JSON encoded spec
    pub fn parse_request(raw: &str) -> Result<Self, String> {
        // parse the typed spec, refusing unknown fields and variants
        serde_json::from_str(raw)
            .map_err(|err| format!("tool_spec is not a valid tool spec: {err}"))
    }

    /// Get the image tool instance this spec pins, if it pins one
    #[must_use]
    pub fn pinned_instance(&self) -> Option<Uuid> {
        // only agentic tools with a container backing pin an instance
        match self {
            ToolSpec::Agentic(AgenticTool {
                backing: AgenticToolBacking::Container(container),
                ..
            }) => Some(container.tool_instance),
            _ => None,
        }
    }

    /// Get the agentic function name of this spec, if it's an agentic tool
    #[must_use]
    pub fn function_name(&self) -> Option<&str> {
        // only agentic tools have function names
        match self {
            ToolSpec::Agentic(agentic) => Some(&agentic.function_name),
            ToolSpec::Image(_) | ToolSpec::Harness(_) => None,
        }
    }

    /// Get the portability report of this spec, if it's an agentic spec
    #[cfg(any(feature = "api", feature = "client"))]
    #[must_use]
    pub fn portability(&self) -> Option<ToolPortabilityReport> {
        // only agentic specs have a portability report
        match self {
            ToolSpec::Agentic(agentic) => Some(agentic.portability()),
            ToolSpec::Image(_) | ToolSpec::Harness(_) => None,
        }
    }

    /// Validate the parts of this spec that need nothing outside of it
    ///
    /// Checks that need the database (pinned instances, MCP tool names, and admin-only workload
    /// settings) are made by the API when the spec is published.
    ///
    /// # Arguments
    ///
    /// * `max_schema_bytes` - The most bytes a serialized input or output schema may take up
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn validate(&self, max_schema_bytes: usize) -> Result<(), String> {
        // validate the parts specific to this kind
        match self {
            ToolSpec::Agentic(agentic) => agentic.validate(max_schema_bytes),
            ToolSpec::Image(workload) => validate_workload(workload),
            ToolSpec::Harness(harness) => harness.validate(),
        }
    }
}

/// Validate the parts of a workload spec that need nothing outside of it
///
/// # Arguments
///
/// * `workload` - The workload spec to validate
pub fn validate_workload(workload: &WorkloadSpec) -> Result<(), String> {
    // container environments must name an image to run
    match &workload.environment {
        WorkloadEnvironment::K8s(env) if env.image.is_empty() => {
            Err("environment.K8s.image can't be empty".to_owned())
        }
        WorkloadEnvironment::Windows(env) if env.image.is_empty() => {
            Err("environment.Windows.image can't be empty".to_owned())
        }
        _ => Ok(()),
    }
}

/// A packaged agent harness and the interface Thorium uses to run it
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct HarnessSpec {
    /// Where and how the harness runs (validated to `K8s`)
    pub workload: WorkloadSpec,
    /// Where the harness reads its inputs and writes its outputs, and how it is launched
    #[serde(default)]
    pub interface: HarnessInterface,
    /// The ways this harness can carry out tool calls and the models it can call
    #[serde(default)]
    pub capabilities: HarnessCapabilities,
    /// Limits on the tools agents running in this harness may use
    #[serde(default)]
    pub tool_policy: HarnessToolPolicy,
    /// What the thorium-agent does with tool runs this harness started but never collected
    #[serde(default)]
    pub orphaned_runs: OrphanedRunPolicy,
}

impl HarnessSpec {
    /// Validate the parts of this harness that need nothing outside of it
    ///
    /// A harness runs only in the `K8s` environment.
    pub fn validate(&self) -> Result<(), String> {
        // harnesses are only scheduled in K8s
        if !matches!(self.workload.environment, WorkloadEnvironment::K8s(_)) {
            return Err(format!(
                "a harness must run in the K8s environment, not {}",
                self.workload.environment.scaler()
            ));
        }
        // the workload must be valid on its own
        validate_workload(&self.workload)?;
        // a harness supports native tools exactly when its instance provides some, so it can't
        // declare the mechanism
        if self
            .capabilities
            .mechanisms
            .contains(&ToolMechanism::Native)
        {
            return Err(
                "capabilities.mechanisms can't list Native; a harness supports native tools by providing them"
                    .to_owned(),
            );
        }
        // the harness's own MCP config file must not overwrite one of its other files
        if let Some(target) = &self.interface.mcp_config {
            self.interface.validate_mcp_config(target)?;
        }
        // the models and keys files must not be uploaded as results
        self.interface.validate_secret_inputs()?;
        Ok(())
    }

    /// Get the mechanisms this harness supports for a tool, including `Native` for one its
    /// instance provides
    ///
    /// # Arguments
    ///
    /// * `provided` - Whether this harness's instance provides the tool
    #[must_use]
    pub fn mechanisms_for(&self, provided: bool) -> BTreeSet<ToolMechanism> {
        // start from the declared mechanisms
        let mut mechanisms = self.capabilities.mechanisms.clone();
        // a provided native tool is carried out by the harness itself
        if provided {
            mechanisms.insert(ToolMechanism::Native);
        }
        mechanisms
    }
}

/// The files Thorium and a harness exchange: inputs in, one answer out, then exit
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct HarnessInterface {
    /// Where each input lands
    #[serde(default)]
    pub inputs: HarnessInputs,
    /// Where each output is read from
    #[serde(default)]
    pub outputs: HarnessOutputs,
    /// How the thorium-agent passes each input and output location on the harness's command line
    #[serde(default)]
    pub args: HarnessArgs,
    /// Where this harness reaches the tools behind Thorium (unset = the deployment defaults)
    #[serde(default)]
    pub tool_access: Option<HarnessToolAccess>,
    /// The path and format of an MCP config file the harness reads (unset = none); Thorium
    /// doesn't write it, it only checks the path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_config: Option<McpConfigTarget>,
}

impl HarnessInterface {
    /// Check whether a path is, or lands under, a directory results are collected from
    ///
    /// A trailing slash on the answer directory doesn't change which files are inside it.
    ///
    /// # Arguments
    ///
    /// * `path` - The absolute path to check
    fn in_results_dir(&self, path: &str) -> bool {
        // the path is the directory or starts with it followed by a separator
        let inside = |dir: &str| {
            path.strip_prefix(dir.trim_end_matches('/'))
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        inside(RESULT_FILES_DIR) || inside(&self.outputs.answer)
    }

    /// Make sure the models file and the Thorium keys file, which hold credentials, are written
    /// to plain paths outside every directory results are collected from
    ///
    /// The thorium-agent uploads what it finds in those directories as results, so a credentials
    /// file there would be stored with the run's results.
    pub fn validate_secret_inputs(&self) -> Result<(), String> {
        // check each file that holds credentials
        for (field, path) in [
            ("inputs.models", &self.inputs.models),
            ("inputs.thorium_keys", &self.inputs.thorium_keys),
        ] {
            // an absolute path with an empty, `.`, or `..` segment could name a directory results
            // are collected from without starting with it
            if path.starts_with('/')
                && path[1..]
                    .split('/')
                    .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            {
                return Err(format!(
                    "interface.{field} `{path}` can't have an empty, `.`, or `..` segment"
                ));
            }
            // the file can't be, or land under, a directory results are collected from
            if self.in_results_dir(path) {
                return Err(format!(
                    "interface.{field} `{path}` is inside a directory results are collected from, where credentials would be uploaded as results"
                ));
            }
        }
        Ok(())
    }

    /// Make sure a harness MCP config file is written to a plain absolute path none of the
    /// harness's other files use
    ///
    /// Claude Code's `.claude/settings.json` beside the file is checked the same way.
    ///
    /// # Arguments
    ///
    /// * `target` - The harness's MCP config file
    pub fn validate_mcp_config(&self, target: &McpConfigTarget) -> Result<(), String> {
        // the file is written at an absolute path of bounded length
        if !target.path.starts_with('/') {
            return Err(format!(
                "interface.mcp_config.path `{}` must be an absolute path",
                target.path
            ));
        }
        if target.path.len() > MAX_MCP_CONFIG_PATH {
            return Err(format!(
                "interface.mcp_config.path is longer than {MAX_MCP_CONFIG_PATH} bytes"
            ));
        }
        // every segment names a file or directory, so equal paths are equal strings
        if target.path[1..]
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return Err(format!(
                "interface.mcp_config.path `{}` can't have an empty, `.`, or `..` segment",
                target.path
            ));
        }
        // check the file and, for Claude Code, the settings file written beside it
        self.check_mcp_file(&target.path)?;
        if target.format == McpConfigFormat::ClaudeCode {
            let dir = target.path.rsplit_once('/').map_or("", |(dir, _)| dir);
            self.check_mcp_file(&format!("{dir}/.claude/settings.json"))?;
        }
        Ok(())
    }

    /// Make sure the path of a harness's MCP config file doesn't replace an input or output the
    /// harness exchanges with Thorium
    ///
    /// # Arguments
    ///
    /// * `path` - The absolute path of the MCP config file
    fn check_mcp_file(&self, path: &str) -> Result<(), String> {
        // the file can't be, or land under, a directory results are collected from
        if self.in_results_dir(path) {
            return Err(format!(
                "interface.mcp_config writes `{path}`, which is inside a directory results are collected from"
            ));
        }
        // the file can't replace an input or output the harness exchanges with Thorium
        let taken = [
            ("inputs.run_request", &self.inputs.run_request),
            ("inputs.agent", &self.inputs.agent),
            ("inputs.tools", &self.inputs.tools),
            ("inputs.models", &self.inputs.models),
            ("inputs.thorium_keys", &self.inputs.thorium_keys),
            ("outputs.answer", &self.outputs.answer),
            ("outputs.status", &self.outputs.status),
        ];
        if let Some((field, _)) = taken.iter().find(|(_, taken)| taken.as_str() == path) {
            return Err(format!(
                "interface.mcp_config writes `{path}`, which is already interface.{field}"
            ));
        }
        Ok(())
    }
}

/// Where a harness reads its inputs (defaults are the standard Thorium layout)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessInputs {
    /// The run request: input sample SHA256s and repos, reaction id, workflow instance id and
    /// digest, agent id, workflow limits, and the harness's result size limit
    /// (default `/tmp/thorium/ephemeral/run.json`); the inputs themselves are downloaded to the
    /// workload's sample and repo paths
    pub run_request: String,
    /// The entry agent's and its sub-agents' snapshots from the workflow instance
    /// (default `/tmp/thorium/ephemeral/agent.json`)
    pub agent: String,
    /// The tools the agents were given; delegation tools are derived by the harness from the
    /// sub-agent snapshots (default `/tmp/thorium/ephemeral/tools.json`)
    pub tools: String,
    /// The models file: every agent's resolved model and credential, written by the
    /// thorium-agent (default `/tmp/thorium/ephemeral/models.json`)
    pub models: String,
    /// The Thorium API url and the run-scoped token as a `keys.yml`, written by the thorium-agent
    /// (default `/tmp/thorium/keys.yml`)
    pub thorium_keys: String,
}

impl Default for HarnessInputs {
    /// The standard Thorium input layout
    fn default() -> Self {
        HarnessInputs {
            run_request: "/tmp/thorium/ephemeral/run.json".to_owned(),
            agent: "/tmp/thorium/ephemeral/agent.json".to_owned(),
            tools: "/tmp/thorium/ephemeral/tools.json".to_owned(),
            models: "/tmp/thorium/ephemeral/models.json".to_owned(),
            thorium_keys: "/tmp/thorium/keys.yml".to_owned(),
        }
    }
}

/// Where a harness writes its run outputs, which are uploaded as results on each input
/// (defaults are the thorium-agent's standard result paths)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessOutputs {
    /// The final answer, uploaded as the renderable result (default `/tmp/thorium/results`)
    pub answer: String,
    /// The run status (success or failure reason, turn and token counts), uploaded as a result
    /// file (default `/tmp/thorium/result-files/status.json`)
    pub status: String,
}

impl Default for HarnessOutputs {
    /// The thorium-agent's standard result paths
    fn default() -> Self {
        HarnessOutputs {
            answer: "/tmp/thorium/results".to_owned(),
            status: "/tmp/thorium/result-files/status.json".to_owned(),
        }
    }
}

/// How the thorium-agent passes each value it can give a harness on the harness's command line
/// (every value defaults to not being passed)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessArgs {
    /// How to pass the run request path (`HarnessInputs.run_request`)
    pub run: ArgStrategy,
    /// How to pass the agent snapshots path (`HarnessInputs.agent`)
    pub agent: ArgStrategy,
    /// How to pass the tools path (`HarnessInputs.tools`)
    pub tools: ArgStrategy,
    /// How to pass the models file path (`HarnessInputs.models`)
    pub models: ArgStrategy,
    /// How to pass the Thorium keys path (`HarnessInputs.thorium_keys`)
    pub thorium_keys: ArgStrategy,
    /// How to pass the answer path (`HarnessOutputs.answer`)
    pub results: ArgStrategy,
    /// How to pass the result files directory (`/tmp/thorium/result-files`)
    pub result_files: ArgStrategy,
    /// How to pass the run's reaction id
    pub reaction: ArgStrategy,
}

/// Where a harness reaches the tools behind Thorium, overriding the deployment defaults
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct HarnessToolAccess {
    /// The CLI wrapper's base command including its subcommand (empty = the deployment default)
    #[serde(default)]
    pub cli: Vec<String>,
    /// The url of Thorium's MCP server as seen from inside the harness (unset = the deployment
    /// default)
    #[serde(default)]
    pub mcp_url: Option<String>,
}

/// A harness's own MCP config file, rendered by the thorium-agent from the run's tool server
/// descriptors
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct McpConfigTarget {
    /// The config file format the harness reads
    pub format: McpConfigFormat,
    /// The absolute path the file is written to
    pub path: String,
}

/// The format of an MCP config file a harness reads
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum McpConfigFormat {
    /// Thorium's own neutral `ToolServersFile` JSON
    #[default]
    Neutral,
    /// The Agent Plugins `mcp.json` (`mcpServers`, transport only)
    AgentPlugins,
    /// Claude Code's `.mcp.json`, plus the `permissions` in `.claude/settings.json` beside it
    ClaudeCode,
    /// Codex's `config.toml` (`[mcp_servers.<id>]`)
    Codex,
    /// Gemini CLI's `settings.json` (`mcpServers`)
    GeminiCli,
    /// VS Code's `.vscode/mcp.json` (`servers` and `inputs`)
    VsCode,
    /// Goose's `config.yaml` (`extensions`)
    Goose,
}

/// The ways a harness can carry out tool calls and the models it can call
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct HarnessCapabilities {
    /// The mechanisms this harness supports for tools that are not built into it (`Native` is
    /// implied by the native tools a harness instance provides and can't be listed)
    #[serde(default)]
    pub mechanisms: BTreeSet<ToolMechanism>,
    /// Whether this harness can hand local files to tools and receive the files tools write back
    #[serde(default)]
    pub file_io: bool,
    /// The protocols this harness can call models with
    #[serde(default)]
    pub model_apis: BTreeSet<ModelApi>,
    /// The credential kinds this harness can authenticate with
    #[serde(default)]
    pub credential_kinds: BTreeSet<CredentialType>,
    /// Whether the agents of one run may use different models (false = every agent must resolve
    /// to the same model, or the run fails)
    #[serde(default)]
    pub per_agent_models: bool,
}

/// Limits on the tools agents running in a harness may use
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct HarnessToolPolicy {
    /// Tools with any of these effects can't run in this harness
    #[serde(default)]
    pub denied_effects: BTreeSet<ToolEffect>,
    /// The max bytes of a tool's result returned to the model (0 = unlimited)
    #[serde(default)]
    pub max_result_bytes: u64,
}

/// What the thorium-agent does with tool runs a harness started but never collected when it
/// exits
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum OrphanedRunPolicy {
    /// Fail each unfinished orphaned tool run, then delete each orphaned run's reaction and log
    /// each deletion to the stage logs
    #[default]
    Delete,
    /// Only log each orphaned tool run to the stage logs
    Warn,
}

/// How a harness carries out a tool call, in priority order (the derived `Ord` is the priority)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolMechanism {
    /// A tool built into the harness itself
    Native,
    /// A Thorium-aware harness calls Thorium's REST API itself
    ThoriumApi,
    /// A command-line tool (for tools behind Thorium, the thorium-agent CLI wrapper)
    Cli,
    /// An MCP server (for tools behind Thorium, Thorium's MCP server)
    Mcp,
}

impl ToolMechanism {
    /// Get the name of this mechanism
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // name each mechanism by its variant
        match self {
            ToolMechanism::Native => "Native",
            ToolMechanism::ThoriumApi => "ThoriumApi",
            ToolMechanism::Cli => "Cli",
            ToolMechanism::Mcp => "Mcp",
        }
    }
}

impl fmt::Display for ToolMechanism {
    /// Write this mechanism's name
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the variant's name
        write!(f, "{}", self.as_str())
    }
}

/// Join some mechanisms into an English list such as `ThoriumApi, Cli, or Mcp`
///
/// # Arguments
///
/// * `mechanisms` - The mechanisms to list
fn list_mechanisms(mechanisms: &BTreeSet<ToolMechanism>) -> String {
    // name each mechanism in priority order
    let names = mechanisms
        .iter()
        .map(ToolMechanism::as_str)
        .collect::<Vec<&str>>();
    match names.as_slice() {
        [] => "no mechanism".to_owned(),
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

/// A tool and the harness it is bound against, as the binding rules see them
#[derive(Debug, Clone, Copy)]
pub struct BindingCheck<'a> {
    /// The name of the tool instance being bound
    pub tool_name: &'a str,
    /// The spec of the tool instance being bound
    pub tool: &'a ToolSpec,
    /// The name of the harness instance the tool is bound against
    pub harness_name: &'a str,
    /// The spec of the harness instance the tool is bound against
    pub harness: &'a HarnessSpec,
    /// Whether the harness instance provides the tool
    pub provided: bool,
}

impl BindingCheck<'_> {
    /// Get the mechanisms both the tool and the harness support, after the file and effect
    /// checks
    fn supported(&self) -> Result<BTreeSet<ToolMechanism>, String> {
        // only agentic tools are called by a model
        let ToolSpec::Agentic(agentic) = self.tool else {
            return Err(format!(
                "tool `{}` is a {} tool, which no harness can call",
                self.tool_name,
                self.tool.kind()
            ));
        };
        // a tool that moves files needs a harness that can hand them over
        let capabilities = &agentic.capabilities;
        if (capabilities.file_inputs || capabilities.file_outputs)
            && !self.harness.capabilities.file_io
        {
            return Err(format!(
                "tool `{}` needs file_io, which harness `{}` lacks",
                self.tool_name, self.harness_name
            ));
        }
        // a tool with an effect the harness denies can't run in it
        let denied = capabilities
            .effects
            .intersection(&self.harness.tool_policy.denied_effects)
            .map(|effect| format!("{effect:?}"))
            .collect::<Vec<String>>();
        if !denied.is_empty() {
            return Err(format!(
                "harness `{}` denies the {} effect of tool `{}`",
                self.harness_name,
                denied.join(" and "),
                self.tool_name
            ));
        }
        // a native tool runs only in a harness whose instance provides it
        let needed = agentic.backing.mechanisms();
        if needed.contains(&ToolMechanism::Native) && !self.provided {
            return Err(format!(
                "tool `{}` isn't provided by `{}`",
                self.tool_name, self.harness_name
            ));
        }
        // the tool and the harness must share a mechanism
        let common = needed
            .intersection(&self.harness.mechanisms_for(self.provided))
            .copied()
            .collect::<BTreeSet<ToolMechanism>>();
        if common.is_empty() {
            return Err(format!(
                "tool `{}` needs {}, and harness `{}` supports none of them",
                self.tool_name,
                list_mechanisms(&needed),
                self.harness_name
            ));
        }
        Ok(common)
    }

    /// Pick the highest-priority mechanism both the tool and the harness support
    pub fn bind(&self) -> Result<ToolMechanism, String> {
        // the derived order is the priority, so the first common mechanism wins
        let common = self.supported()?;
        common
            .first()
            .copied()
            .ok_or_else(|| format!("tool `{}` has no common mechanism", self.tool_name))
    }

    /// Check that a mechanism passes the binding rules for this tool and harness
    ///
    /// # Arguments
    ///
    /// * `mechanism` - The mechanism a binding names
    pub fn check(&self, mechanism: ToolMechanism) -> Result<(), String> {
        // the mechanism must be one both sides support
        let common = self.supported()?;
        if common.contains(&mechanism) {
            Ok(())
        } else {
            Err(format!(
                "tool `{}` can't be bound to {mechanism} in harness `{}`, which supports {} for it",
                self.tool_name,
                self.harness_name,
                list_mechanisms(&common)
            ))
        }
    }
}

/// The info needed to describe an agentic tool to a model and to invoke it
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct AgenticTool {
    /// The name a model uses to call this tool (must match `^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`)
    pub function_name: String,
    /// A short human readable title for this tool
    pub title: Option<String>,
    /// The description given to the model explaining what this tool does and when to use it
    /// (must not be empty)
    pub model_description: String,
    /// The JSON schema describing this tool's input arguments (top level type must be `object`)
    #[cfg_attr(feature = "api", schema(value_type = Object))]
    pub input_schema: serde_json::Map<String, serde_json::Value>,
    /// The JSON schema describing this tool's structured output if it has one
    #[cfg_attr(feature = "api", schema(value_type = Option<Object>))]
    pub output_schema: Option<serde_json::Map<String, serde_json::Value>>,
    /// What this tool does when it runs and which files it takes or produces (set by the author)
    #[serde(default)]
    pub capabilities: ToolCapabilities,
    /// How a harness invokes this tool
    pub backing: AgenticToolBacking,
    /// The format of the result this tool returns to the model
    #[serde(default)]
    pub result_format: ToolResultFormat,
    /// Whether each call to this tool must be approved by the user before it runs
    #[serde(default)]
    pub requires_approval: bool,
    /// The max number of seconds a single call to this tool may run for (required for `ThoriumMcp`
    /// and `Container` backings)
    pub timeout: Option<u64>,
    /// Example inputs that show a model how to call this tool (at most 5, each valid against
    /// `input_schema`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "api", schema(value_type = Vec<Object>))]
    pub input_examples: Vec<serde_json::Value>,
    /// Whether model APIs that support strict tool schemas should enforce `input_schema` exactly
    #[serde(default, skip_serializing_if = "ToolStrictness::is_auto")]
    pub strict: ToolStrictness,
    /// Whether a model sees this tool up front or finds it through tool search
    #[serde(default, skip_serializing_if = "ToolLoading::is_eager")]
    pub loading: ToolLoading,
    /// Icons a client may show for this tool (MCP `icons`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<ToolIcon>,
}

/// Check whether a name is a valid agentic function name (`^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`)
///
/// # Arguments
///
/// * `name` - The name to check
#[must_use]
pub fn is_valid_function_name(name: &str) -> bool {
    // the name starts with a letter or underscore and has at most 64 letters, digits,
    // underscores, or hyphens
    name.len() <= MAX_FUNCTION_NAME_LEN
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Get the number of bytes a schema takes up when serialized
///
/// # Arguments
///
/// * `schema` - The schema to measure
fn schema_bytes(schema: &serde_json::Map<String, serde_json::Value>) -> usize {
    // measure the compact serialized schema; a JSON map always serializes
    serde_json::to_vec(schema).map_or(0, |bytes| bytes.len())
}

impl AgenticTool {
    /// Validate the parts of this agentic tool that need nothing outside of it
    ///
    /// # Arguments
    ///
    /// * `max_schema_bytes` - The most bytes a serialized input or output schema may take up
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn validate(&self, max_schema_bytes: usize) -> Result<(), String> {
        // the function name must be one every model API can call
        if !is_valid_function_name(&self.function_name) {
            return Err(format!(
                "function_name `{}` must start with a letter or underscore and have at most {MAX_FUNCTION_NAME_LEN} letters, digits, underscores, or hyphens",
                self.function_name
            ));
        }
        // some model APIs require a description
        if self.model_description.trim().is_empty() {
            return Err("model_description can't be empty".to_owned());
        }
        // the input schema must describe an object
        if self.input_schema.get("type") != Some(&serde_json::Value::String("object".to_owned())) {
            return Err(
                "input_schema must be a JSON object whose top-level type is `object`".to_owned(),
            );
        }
        // the input schema must fit within the size cap
        let input_bytes = schema_bytes(&self.input_schema);
        if input_bytes > max_schema_bytes {
            return Err(format!(
                "input_schema is {input_bytes} bytes, over the {max_schema_bytes} byte limit"
            ));
        }
        // the output schema must fit within the size cap too
        if let Some(output_schema) = &self.output_schema {
            // measure the output schema
            let output_bytes = schema_bytes(output_schema);
            if output_bytes > max_schema_bytes {
                return Err(format!(
                    "output_schema is {output_bytes} bytes, over the {max_schema_bytes} byte limit"
                ));
            }
        }
        // check the schemas, examples, icons, and strictness every model API needs
        self.validate_portability()?;
        // a native backing names its tool and carries JSON config
        self.backing.validate_native()?;
        // tools behind Thorium need a timeout
        if self.timeout.is_none()
            && !matches!(self.backing, AgenticToolBacking::HarnessNative { .. })
        {
            return Err(format!(
                "timeout is required for a {} backing",
                self.backing.name()
            ));
        }
        // a container backing's input mapping must name schema properties
        if let AgenticToolBacking::Container(container) = &self.backing {
            container.inputs.validate(&self.input_schema)?;
        }
        Ok(())
    }

    /// Check the portability rules: schema safety, property names, the output schema's result
    /// format, input examples, icons, and strictness
    #[cfg(any(feature = "api", feature = "client"))]
    fn validate_portability(&self) -> Result<(), String> {
        // the input schema can't fetch anything, nest too deeply, or hold too many patterns
        let walked = portability::walk(&self.input_schema);
        portability::check_safety("input_schema", &walked)?;
        portability::check_patterns("input_schema", &walked)?;
        // every property name is portable and every object lists its properties
        portability::check_properties(&walked)?;
        // the output schema can't fetch anything, nest too deeply, or hold too many patterns either
        if let Some(output_schema) = &self.output_schema {
            let output_walked = portability::walk(output_schema);
            portability::check_safety("output_schema", &output_walked)?;
            portability::check_patterns("output_schema", &output_walked)?;
            // structured output is only returned as JSON
            if self.result_format != ToolResultFormat::Json {
                return Err(format!(
                    "output_schema needs result_format Json, not {:?}",
                    self.result_format
                ));
            }
            // the validator must accept the output schema, since results are checked against it
            portability::compile("output_schema", output_schema)?;
        }
        // the validator must accept the input schema, including every pattern
        let validator = portability::compile("input_schema", &self.input_schema)?;
        // the examples must be few, small, and valid
        portability::check_examples(&self.input_examples, &validator)?;
        // the icons must be few, small, and portable
        portability::check_icons(&self.icons)?;
        // a tool that must be strict needs a strict-compatible schema
        if self.strict == ToolStrictness::On {
            let issues = portability::strict_issues(&walked);
            if !issues.is_empty() {
                return Err(format!(
                    "strict is On but input_schema isn't strict-compatible: {}",
                    issues.join("; ")
                ));
            }
        }
        Ok(())
    }

    /// Get how portable this tool's input schema is across model APIs
    #[cfg(any(feature = "api", feature = "client"))]
    #[must_use]
    pub fn portability(&self) -> ToolPortabilityReport {
        // report on the input schema
        portability::report(&self.input_schema)
    }

    /// Build the MCP annotations of this tool from its capabilities
    #[cfg(feature = "api")]
    #[must_use]
    pub fn mcp_annotations(&self) -> rmcp::model::ToolAnnotations {
        // a tool with no effects is read-only
        let read_only = self.capabilities.effects.is_empty();
        // a tool is destructive only when its author declared it
        let destructive = self.capabilities.effects.contains(&ToolEffect::Destructive);
        // a tool reaches an open world when it's declared to or when it writes outside Thorium
        let open_world = self.capabilities.open_world
            || self
                .capabilities
                .effects
                .contains(&ToolEffect::WritesExternal);
        // build the annotations, titled when the tool has a title
        let annotations = match &self.title {
            Some(title) => rmcp::model::ToolAnnotations::with_title(title.clone()),
            None => rmcp::model::ToolAnnotations::new(),
        };
        annotations
            .read_only(read_only)
            .destructive(destructive)
            .idempotent(self.capabilities.idempotent)
            .open_world(open_world)
    }

    /// Convert this tool to an MCP tool definition
    #[cfg(feature = "api")]
    #[must_use]
    pub fn to_mcp_tool(&self) -> rmcp::model::Tool {
        // copy the fields a model reads and derive the annotations
        let mut tool = rmcp::model::Tool::new(
            self.function_name.clone(),
            self.model_description.clone(),
            std::sync::Arc::new(self.input_schema.clone()),
        )
        .with_annotations(self.mcp_annotations());
        // carry the title, output schema, and icons when the tool has them
        if let Some(title) = &self.title {
            tool = tool.with_title(title.clone());
        }
        if let Some(output_schema) = &self.output_schema {
            tool = tool.with_raw_output_schema(std::sync::Arc::new(output_schema.clone()));
        }
        if let Some(icons) = self.mcp_icons() {
            tool = tool.with_icons(icons);
        }
        tool
    }

    /// Check an MCP tool result against this tool's output schema
    ///
    /// A successful result of a tool with an output schema must carry structured content that
    /// matches the schema; one that doesn't becomes an error result naming the mismatch. Error
    /// results and the results of tools without an output schema are returned unchanged.
    ///
    /// # Arguments
    ///
    /// * `result` - The result to check
    #[cfg(feature = "api")]
    #[must_use]
    pub fn check_mcp_result(
        &self,
        result: rmcp::model::CallToolResult,
    ) -> rmcp::model::CallToolResult {
        // only successful results of tools with an output schema are checked
        let Some(output_schema) = &self.output_schema else {
            return result;
        };
        if result.is_error == Some(true) {
            return result;
        }
        // find what keeps the result from matching the schema, if anything does
        let mismatch = match (
            portability::compile("output_schema", output_schema),
            &result.structured_content,
        ) {
            (Err(error), _) => Some(error),
            (Ok(_), None) => Some(format!(
                "{} returned no structured content to match its output_schema",
                self.function_name
            )),
            (Ok(validator), Some(content)) => validator.validate(content).err().map(|error| {
                format!(
                    "{} returned a result that doesn't match its output_schema at #{}: {error}",
                    self.function_name,
                    error.instance_path()
                )
            }),
        };
        // a result that doesn't match becomes an error naming the mismatch
        match mismatch {
            Some(message) => {
                rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(message)])
            }
            None => result,
        }
    }

    /// Convert this tool's icons to MCP icons, if it has any
    #[cfg(feature = "api")]
    fn mcp_icons(&self) -> Option<Vec<rmcp::model::Icon>> {
        // a tool without icons sends none
        if self.icons.is_empty() {
            return None;
        }
        // copy each icon's url, media type, sizes, and theme
        let icons = self
            .icons
            .iter()
            .map(|icon| {
                // start from the icon's url
                let mut mcp_icon = rmcp::model::Icon::new(icon.src.clone());
                // add its media type, sizes, and theme when it has them
                if let Some(mime_type) = &icon.mime_type {
                    mcp_icon = mcp_icon.with_mime_type(mime_type.clone());
                }
                if !icon.sizes.is_empty() {
                    mcp_icon = mcp_icon.with_sizes(icon.sizes.clone());
                }
                if let Some(theme) = icon.theme {
                    mcp_icon = mcp_icon.with_theme(theme.into());
                }
                mcp_icon
            })
            .collect();
        Some(icons)
    }
}

/// Get the portability report of a JSON encoded agentic tool spec
///
/// The spec may be a full `ToolSpec` (`{"Agentic": {...}}`) or a bare agentic tool. The report is
/// returned JSON encoded, so bindings in other languages can use it without these types.
///
/// # Arguments
///
/// * `raw` - The JSON encoded spec
#[cfg(any(feature = "api", feature = "client"))]
pub fn tool_portability_json(raw: &str) -> Result<String, String> {
    // read a full spec first, then a bare agentic tool
    let tool = match serde_json::from_str::<ToolSpec>(raw) {
        Ok(ToolSpec::Agentic(tool)) => tool,
        Ok(ToolSpec::Image(_) | ToolSpec::Harness(_)) => {
            return Err("only agentic tool specs have a portability report".to_owned());
        }
        Err(spec_err) => serde_json::from_str::<AgenticTool>(raw).map_err(|tool_err| {
            format!("not a tool spec ({spec_err}) or an agentic tool ({tool_err})")
        })?,
    };
    // encode the report
    serde_json::to_string(&tool.portability()).map_err(|err| err.to_string())
}

/// What an agentic tool does when it runs (a tool with no effects is read-only)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
// each flag is a separate stored and hashed field of the tool spec
#[allow(clippy::struct_excessive_bools)]
pub struct ToolCapabilities {
    /// The side effects this tool can have (empty = read-only)
    #[serde(default)]
    pub effects: BTreeSet<ToolEffect>,
    /// Calling this tool repeatedly with the same input has no additional effect
    #[serde(default)]
    pub idempotent: bool,
    /// This tool takes files from the harness environment as input
    #[serde(default)]
    pub file_inputs: bool,
    /// This tool writes files into the harness environment as output
    #[serde(default)]
    pub file_outputs: bool,
    /// This tool reaches systems outside Thorium, even if it only reads from them
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_world: bool,
}

/// A side effect an agentic tool can have
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolEffect {
    /// Creates or changes data in Thorium (samples, results, tags, entities)
    WritesThorium,
    /// Creates or changes data outside of Thorium
    WritesExternal,
    /// Deletes or overwrites data so it can't be recovered
    Destructive,
}

/// The format of the result an agentic tool returns to the model
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolResultFormat {
    /// A JSON value
    #[default]
    Json,
    /// Plain text
    Text,
    /// Result files, returned to the model as their paths
    Files,
}

/// Whether model APIs that support strict tool schemas enforce a tool's input schema exactly
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolStrictness {
    /// Strict wherever the model and its API support it and the schema is strict-compatible
    #[default]
    Auto,
    /// Strict wherever supported; a schema that isn't strict-compatible is refused at publish
    On,
    /// Never strict
    Off,
}

impl ToolStrictness {
    /// Check whether this is the default strictness, which is left out of stored specs
    #[must_use]
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn is_auto(&self) -> bool {
        // only Auto is the default
        *self == ToolStrictness::Auto
    }
}

/// Whether a model sees a tool's definition up front or finds it through tool search
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolLoading {
    /// The definition is in every request
    #[default]
    Eager,
    /// The definition is loaded on demand through tool search where the model API supports it
    /// (a hint; APIs without tool search load it eagerly)
    Deferred,
}

impl ToolLoading {
    /// Check whether this is the default loading, which is left out of stored specs
    #[must_use]
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn is_eager(&self) -> bool {
        // only Eager is the default
        *self == ToolLoading::Eager
    }
}

/// An icon for a tool (the MCP `Icon` shape)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct ToolIcon {
    /// The icon's url: an `https:` url or a `data:` URI
    pub src: String,
    /// The icon's media type (for example `image/png`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The sizes the icon fits (for example `48x48`, or `any` for a vector icon)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<String>,
    /// The UI theme the icon is drawn for (unset = any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ToolIconTheme>,
}

/// The UI theme an icon is drawn for
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolIconTheme {
    /// Drawn for a light background
    Light,
    /// Drawn for a dark background
    Dark,
}

#[cfg(feature = "api")]
impl From<ToolIconTheme> for rmcp::model::IconTheme {
    /// Convert an icon theme to its MCP icon theme
    ///
    /// # Arguments
    ///
    /// * `theme` - The icon theme to convert
    fn from(theme: ToolIconTheme) -> Self {
        // each theme has the same meaning in MCP
        match theme {
            ToolIconTheme::Light => rmcp::model::IconTheme::Light,
            ToolIconTheme::Dark => rmcp::model::IconTheme::Dark,
        }
    }
}

/// How portable an agentic tool's schemas are across model APIs (computed, never stored)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolPortabilityReport {
    /// Whether `input_schema` can be sent in strict mode where a model API supports it
    pub strict_compatible: bool,
    /// Why `input_schema` can't be sent in strict mode (empty when it can)
    #[serde(default)]
    pub strict_issues: Vec<String>,
    /// Whether `input_schema` is a valid Gemini `parameters` schema as written
    pub gemini_compatible: bool,
    /// The lossy rewrites a Gemini `parameters` schema needs (empty when none)
    #[serde(default)]
    pub gemini_rewrites: Vec<String>,
}

/// How an agent harness invokes an agentic tool
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub enum AgenticToolBacking {
    /// A tool served by Thorium's own MCP server
    ThoriumMcp {
        /// The name of the MCP tool to call
        tool: String,
    },
    /// A tool implemented by a container image tool
    Container(ContainerToolBacking),
    /// A tool built into the harness the agent runs in (valid only when that harness instance
    /// provides this tool instance)
    HarnessNative {
        /// The name of the tool within the harness
        tool: String,
        /// Harness-specific JSON config passed to the harness unchanged
        #[serde(default)]
        config: Option<String>,
    },
}

impl Default for AgenticToolBacking {
    /// The default backing only anchors digest pruning; validation still requires a real tool
    fn default() -> Self {
        AgenticToolBacking::ThoriumMcp {
            tool: String::new(),
        }
    }
}

impl AgenticToolBacking {
    /// Get the name of this kind of backing
    #[must_use]
    pub fn name(&self) -> &'static str {
        // name each backing by its variant
        match self {
            AgenticToolBacking::ThoriumMcp { .. } => "ThoriumMcp",
            AgenticToolBacking::Container(_) => "Container",
            AgenticToolBacking::HarnessNative { .. } => "HarnessNative",
        }
    }

    /// Get the mechanisms a harness can carry out calls to a tool with this backing through
    #[must_use]
    pub fn mechanisms(&self) -> BTreeSet<ToolMechanism> {
        // tools behind Thorium are reached through its API, the CLI wrapper, or its MCP server,
        // and a native tool only by the harness that has it built in
        match self {
            AgenticToolBacking::ThoriumMcp { .. } | AgenticToolBacking::Container(_) => [
                ToolMechanism::ThoriumApi,
                ToolMechanism::Cli,
                ToolMechanism::Mcp,
            ]
            .into_iter()
            .collect(),
            AgenticToolBacking::HarnessNative { .. } => {
                std::iter::once(ToolMechanism::Native).collect()
            }
        }
    }

    /// Validate a native backing: its tool is named and its config, when set, is JSON
    fn validate_native(&self) -> Result<(), String> {
        // only native backings are checked here
        let AgenticToolBacking::HarnessNative { tool, config } = self else {
            return Ok(());
        };
        // the harness needs a tool name to find its built-in tool
        if tool.trim().is_empty() {
            return Err("backing.HarnessNative.tool can't be empty".to_owned());
        }
        // the config is passed through unchanged, but it must be JSON
        if let Some(config) = config
            && let Err(err) = serde_json::from_str::<serde_json::Value>(config)
        {
            return Err(format!(
                "backing.HarnessNative.config must be valid JSON: {err}"
            ));
        }
        Ok(())
    }
}

/// An agentic tool implemented by a container image tool
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct ContainerToolBacking {
    /// The Image tool instance that implements this tool (always pinned)
    pub tool_instance: Uuid,
    /// How the model's tool call input maps onto the tool's job args and input files
    #[serde(default)]
    pub inputs: ToolInputMapping,
    /// Where the tool's outputs are delivered inside the harness environment
    #[serde(default)]
    pub outputs: ToolOutputDelivery,
}

/// How a model's tool call input becomes a container tool's job args and input files
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct ToolInputMapping {
    /// How each top level `input_schema` property maps onto the tool's job args
    #[serde(default)]
    pub args: BTreeMap<String, ArgTarget>,
    /// The `input_schema` property holding sample SHA256s to run on (defaults to the run's input files)
    pub samples: Option<String>,
}

impl ToolInputMapping {
    /// Make sure this mapping names only top-level properties of an input schema
    ///
    /// Every `args` key must be a top-level property, and `samples` must name a top-level
    /// property whose type is an array of strings.
    ///
    /// # Arguments
    ///
    /// * `input_schema` - The input schema of the tool this mapping belongs to
    pub fn validate(
        &self,
        input_schema: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        // get the top-level properties of the schema
        let properties = input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        // every arg must map a top-level property
        for key in self.args.keys() {
            if !properties.is_some_and(|props| props.contains_key(key)) {
                return Err(format!(
                    "input mapping arg `{key}` is not a top-level input_schema property"
                ));
            }
        }
        // the samples property must be a top-level array of strings
        if let Some(samples) = &self.samples {
            // get the schema of the named property
            let property = properties.and_then(|props| props.get(samples));
            // check the property is an array whose items are strings
            let is_string_array = property.is_some_and(|prop| {
                prop.get("type").and_then(serde_json::Value::as_str) == Some("array")
                    && prop
                        .get("items")
                        .and_then(|items| items.get("type"))
                        .and_then(serde_json::Value::as_str)
                        == Some("string")
            });
            if !is_string_array {
                return Err(format!(
                    "input mapping samples `{samples}` must name a top-level input_schema property whose type is an array of strings"
                ));
            }
        }
        Ok(())
    }

    /// Map a model's tool call input onto a container tool's job args and input samples
    ///
    /// Mapped properties are placed in the order of their names, a missing or `null` property
    /// maps to nothing, and properties the mapping doesn't name are left out.
    ///
    /// # Arguments
    ///
    /// * `input` - The tool call's input object
    pub fn map_input(
        &self,
        input: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<MappedToolInput, String> {
        // start with no args and no samples
        let mut mapped = MappedToolInput::default();
        // place each mapped property that the input sets
        for (property, target) in &self.args {
            // skip properties the input leaves out or sets to null
            let value = match input.get(property) {
                None | Some(serde_json::Value::Null) => continue,
                Some(value) => value,
            };
            match target {
                // append each value as a positional
                ArgTarget::Positional => mapped.args.positionals.extend(arg_values(value)),
                // pass every value under the kwarg's key
                ArgTarget::Kwarg(key) => mapped
                    .args
                    .kwargs
                    .entry(key.clone())
                    .or_default()
                    .extend(arg_values(value)),
                // pass the switch only when the property is true
                ArgTarget::Switch(flag) => match value {
                    serde_json::Value::Bool(true) => mapped.args.switches.push(flag.clone()),
                    serde_json::Value::Bool(false) => (),
                    _ => {
                        return Err(format!(
                            "input property `{property}` maps to the switch `{flag}`, so it must be true or false"
                        ));
                    }
                },
            }
        }
        // read the files to run on from the samples property
        if let Some(property) = &self.samples {
            match input.get(property) {
                None | Some(serde_json::Value::Null) => (),
                Some(serde_json::Value::Array(values)) => {
                    // every value must be a SHA256 string
                    let samples = values
                        .iter()
                        .map(|value| value.as_str().map(str::to_owned))
                        .collect::<Option<Vec<String>>>()
                        .ok_or_else(|| {
                            format!("input property `{property}` must be a list of SHA256 strings")
                        })?;
                    mapped.samples = Some(samples);
                }
                Some(_) => {
                    return Err(format!(
                        "input property `{property}` must be a list of SHA256 strings"
                    ));
                }
            }
        }
        Ok(mapped)
    }
}

/// Get the job arg values one tool call input value maps to
///
/// An array gives one value per item, a string its text, and any other value its JSON text.
///
/// # Arguments
///
/// * `value` - The input value to convert
fn arg_values(value: &serde_json::Value) -> Vec<String> {
    // convert one value to its arg text
    let text = |value: &serde_json::Value| match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    match value {
        serde_json::Value::Array(values) => values.iter().map(text).collect(),
        other => vec![text(other)],
    }
}

/// A model's tool call input mapped onto a container tool's job
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MappedToolInput {
    /// The job args the input maps to
    pub args: GenericJobArgs,
    /// The sample SHA256s the input names (unset when the mapping has no samples property or
    /// the input leaves it out)
    pub samples: Option<Vec<String>>,
}

/// Where a single tool call input property goes on the tool's job
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ArgTarget {
    /// Appended as a positional argument (arrays append each value)
    #[default]
    Positional,
    /// Passed as a keyword argument with this key (e.g. `--depth`)
    Kwarg(String),
    /// Passed as this switch when the property is `true` (e.g. `--recursive`)
    Switch(String),
}

/// Where a container tool's outputs are delivered inside the harness
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputDelivery {
    /// The directory the tool's results and result files are written to
    /// (default `/tmp/thorium/tool-results/{function_name}`)
    pub path: Option<String>,
    /// The max bytes of the tool's renderable result returned to the model, with the rest left on
    /// disk (0 = unlimited)
    #[serde(default)]
    pub max_result_bytes: u64,
}

impl ToolOutputDelivery {
    /// Get the directory a tool's outputs are delivered to
    ///
    /// An unset path delivers them under the default tool results directory, in a directory named
    /// after the tool's function name.
    ///
    /// # Arguments
    ///
    /// * `function_name` - The name a model calls the tool by
    #[must_use]
    pub fn resolved_path(&self, function_name: &str) -> String {
        // use the configured directory or the function's default directory
        match &self.path {
            Some(path) => path.clone(),
            None => format!("{DEFAULT_TOOL_RESULTS_DIR}/{function_name}"),
        }
    }

    /// Get the most result bytes returned to a model for this tool in a harness
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness's own limit (0 = unlimited)
    #[must_use]
    pub fn effective_max_result_bytes(&self, harness: u64) -> u64 {
        effective_max_result_bytes(self.max_result_bytes, harness)
    }
}

/// The directory container tool outputs are delivered under when a backing names no path
pub const DEFAULT_TOOL_RESULTS_DIR: &str = "/tmp/thorium/tool-results";

/// Get the most result bytes returned to a model: the smaller non-zero of two limits
///
/// # Arguments
///
/// * `backing` - The tool backing's limit (0 = unlimited)
/// * `harness` - The harness's limit (0 = unlimited)
#[must_use]
pub fn effective_max_result_bytes(backing: u64, harness: u64) -> u64 {
    // zero means unlimited, so only non-zero limits compete
    match (backing, harness) {
        (0, limit) | (limit, 0) => limit,
        (backing, harness) => backing.min(harness),
    }
}

/// A request to create a tool and its first tool instance
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolEntityRequest {
    /// The kind of tool to create
    pub kind: ToolKind,
    /// The version label for the first tool instance (parsed into a `ToolVersion`)
    pub version: String,
    /// The JSON encoded `ToolSpec` for the first tool instance
    pub spec: String,
    /// The native agentic tool instances the first instance provides (Harness tools only)
    #[serde(default)]
    pub provides: Vec<Uuid>,
    /// The JSON encoded `HarnessEnv` for this tool (Harness tools only)
    #[serde(default)]
    pub harness_env: Option<String>,
    /// Exact model ids no run of this harness may resolve to (Harness tools only)
    #[serde(default)]
    pub disabled_models: BTreeSet<String>,
}

impl ToolEntityRequest {
    /// Build a request to create a tool from a typed spec
    ///
    /// # Arguments
    ///
    /// * `version` - The version label for the first tool instance
    /// * `spec` - The spec of the first tool instance
    pub fn new(version: impl Into<String>, spec: &ToolSpec) -> Result<Self, serde_json::Error> {
        // encode the spec the way the form carries it
        let encoded = serde_json::to_string(spec)?;
        Ok(ToolEntityRequest {
            kind: spec.kind(),
            version: version.into(),
            spec: encoded,
            provides: Vec::new(),
            harness_env: None,
            disabled_models: BTreeSet::new(),
        })
    }

    /// Set the native tool instances the first instance of a harness provides
    ///
    /// # Arguments
    ///
    /// * `provides` - The native agentic tool instances
    #[must_use]
    pub fn provides(mut self, provides: Vec<Uuid>) -> Self {
        self.provides = provides;
        self
    }

    /// Set the values a harness's pod env holds
    ///
    /// # Arguments
    ///
    /// * `env` - The harness env
    pub fn harness_env(mut self, env: &HarnessEnv) -> Result<Self, serde_json::Error> {
        // encode the env the way the form carries it
        self.harness_env = Some(serde_json::to_string(env)?);
        Ok(self)
    }

    /// Set the model ids no run of a harness may resolve to
    ///
    /// # Arguments
    ///
    /// * `models` - The disabled model ids
    #[must_use]
    pub fn disabled_models<T: Into<String>>(mut self, models: impl IntoIterator<Item = T>) -> Self {
        self.disabled_models = models.into_iter().map(Into::into).collect();
        self
    }

    /// Get the form fields this request sends, by name
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind, then this tool's metadata
        let mut fields = vec![
            ("kind", super::EntityKinds::Tool.as_str().to_owned()),
            ("metadata[tool_kind]", self.kind.as_ref().to_owned()),
            ("metadata[tool_version]", self.version.clone()),
            ("metadata[tool_spec]", self.spec.clone()),
        ];
        // the harness's native tools and root settings, when they're set
        fields.extend(
            self.provides
                .iter()
                .map(|id| ("metadata[tool_provides][]", id.to_string())),
        );
        if let Some(env) = &self.harness_env {
            fields.push(("metadata[tool_harness_env]", env.clone()));
        }
        fields.extend(
            self.disabled_models
                .iter()
                .map(|model| ("metadata[tool_disabled_models][]", model.clone())),
        );
        fields
    }

    /// Add this tool request's metadata to a form
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
            .form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }
}

/// A request to create a tool instance directly for an existing tool
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolInstanceEntityRequest {
    /// The ID of the tool this is a new instance of
    pub tool: Uuid,
    /// The version label for this tool instance (parsed into a `ToolVersion`)
    pub version: String,
    /// The JSON encoded `ToolSpec` for this tool instance
    pub spec: String,
    /// The native agentic tool instances this instance provides (Harness tools only)
    #[serde(default)]
    pub provides: Vec<Uuid>,
}

impl ToolInstanceEntityRequest {
    /// Build a request to publish a tool instance from a typed spec
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool to publish a new instance of
    /// * `version` - The version label for the new instance
    /// * `spec` - The spec of the new instance
    pub fn new(
        tool: Uuid,
        version: impl Into<String>,
        spec: &ToolSpec,
    ) -> Result<Self, serde_json::Error> {
        // encode the spec the way the form carries it
        let encoded = serde_json::to_string(spec)?;
        Ok(ToolInstanceEntityRequest {
            tool,
            version: version.into(),
            spec: encoded,
            provides: Vec::new(),
        })
    }

    /// Set the native tool instances a harness instance provides
    ///
    /// # Arguments
    ///
    /// * `provides` - The native agentic tool instances
    #[must_use]
    pub fn provides(mut self, provides: Vec<Uuid>) -> Self {
        self.provides = provides;
        self
    }

    /// Get the form fields this request sends, by name
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind, then this instance's metadata
        let mut fields = vec![
            ("kind", super::EntityKinds::ToolInstance.as_str().to_owned()),
            ("metadata[tool_instance_tool]", self.tool.to_string()),
            ("metadata[tool_version]", self.version.clone()),
            ("metadata[tool_spec]", self.spec.clone()),
        ];
        // the native tools a harness instance provides
        fields.extend(
            self.provides
                .iter()
                .map(|id| ("metadata[tool_provides][]", id.to_string())),
        );
        fields
    }

    /// Add this tool instance request's metadata to a form
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
            .form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }
}

/// The fields of a tool update
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolUpdateFields<'a> {
    /// The version label for a new tool instance
    pub version: Option<&'a String>,
    /// The JSON encoded spec for a new tool instance
    pub spec: Option<&'a String>,
    /// The native tool instances a new harness instance provides
    pub provides: &'a [Uuid],
    /// The new JSON encoded harness env
    pub harness_env: Option<&'a String>,
    /// Whether to clear the harness env
    pub clear_harness_env: Option<bool>,
    /// The model ids to add to the harness's disabled models
    pub disable_models: Option<&'a BTreeSet<String>>,
    /// The model ids to remove from the harness's disabled models
    pub enable_models: Option<&'a BTreeSet<String>>,
}

/// Get the form fields a tool update sends, by name
///
/// # Arguments
///
/// * `update` - The fields of the tool update
#[must_use]
pub fn tool_update_form_fields(update: ToolUpdateFields<'_>) -> Vec<(&'static str, String)> {
    // add only the fields that are set
    let mut fields = Vec::with_capacity(2);
    if let Some(version) = update.version {
        fields.push(("metadata[tool_version]", version.clone()));
    }
    if let Some(spec) = update.spec {
        fields.push(("metadata[tool_spec]", spec.clone()));
    }
    // the native tools a new harness instance provides
    fields.extend(
        update
            .provides
            .iter()
            .map(|id| ("metadata[tool_provides][]", id.to_string())),
    );
    // the harness root's settings
    if let Some(env) = update.harness_env {
        fields.push(("metadata[tool_harness_env]", env.clone()));
    }
    if let Some(clear) = update.clear_harness_env {
        fields.push(("metadata[clear_tool_harness_env]", clear.to_string()));
    }
    fields.extend(
        update
            .disable_models
            .into_iter()
            .flatten()
            .map(|model| ("metadata[disable_tool_models][]", model.clone())),
    );
    fields.extend(
        update
            .enable_models
            .into_iter()
            .flatten()
            .map(|model| ("metadata[enable_tool_models][]", model.clone())),
    );
    fields
}

/// Get the form fields a tool instance update sends, by name
///
/// # Arguments
///
/// * `deprecated` - Whether the tool instance should be deprecated
#[must_use]
pub fn tool_instance_update_form_fields(deprecated: Option<bool>) -> Vec<(&'static str, String)> {
    // add the deprecated flag if it's set
    deprecated
        .map(|deprecated| ("metadata[tool_instance_deprecated]", deprecated.to_string()))
        .into_iter()
        .collect()
}

cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use crate::models::EntityMetadataForm;
        use crate::utils::ApiError;
        use crate::bad;

        impl ToolEntityRequest {
            /// Take the tool fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // a tool instance's root is never set on a tool
                if form.tool_instance_tool.is_some() {
                    return bad!("tool_instance_tool is only valid for a tool instance".to_owned());
                }
                // the kind of tool is required
                let Some(kind) = form.tool_kind.take() else {
                    return bad!("A tool needs a tool_kind".to_owned());
                };
                // the first instance's version label is required
                let Some(version) = form.tool_version.take() else {
                    return bad!("A tool needs a tool_version for its first instance".to_owned());
                };
                // the first instance's spec is required
                let Some(spec) = form.tool_spec.take() else {
                    return bad!("A tool needs a tool_spec for its first instance".to_owned());
                };
                // the harness fields are checked against the tool's kind when it's created
                Ok(ToolEntityRequest {
                    kind,
                    version,
                    spec,
                    provides: std::mem::take(&mut form.tool_provides),
                    harness_env: form.tool_harness_env.take(),
                    disabled_models: std::mem::take(&mut form.tool_disabled_models),
                })
            }
        }

        impl ToolInstanceEntityRequest {
            /// Take the tool instance fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // a tool instance takes its kind from its tool
                if form.tool_kind.is_some() {
                    return bad!("tool_kind is only valid for a tool".to_owned());
                }
                // a harness's settings live on its tool, not on an instance
                if form.tool_harness_env.is_some() || !form.tool_disabled_models.is_empty() {
                    return bad!(
                        "tool_harness_env and tool_disabled_models are only valid for a tool"
                            .to_owned()
                    );
                }
                // the tool to publish an instance of is required
                let Some(tool) = form.tool_instance_tool.take() else {
                    return bad!("A tool instance needs a tool_instance_tool".to_owned());
                };
                // the version label is required
                let Some(version) = form.tool_version.take() else {
                    return bad!("A tool instance needs a tool_version".to_owned());
                };
                // the spec is required
                let Some(spec) = form.tool_spec.take() else {
                    return bad!("A tool instance needs a tool_spec".to_owned());
                };
                Ok(ToolInstanceEntityRequest {
                    tool,
                    version,
                    spec,
                    provides: std::mem::take(&mut form.tool_provides),
                })
            }
        }
    }
}
