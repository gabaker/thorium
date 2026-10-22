//! The run-time descriptors a harness reads: its `tools.json` entries and its MCP server
//! descriptors
//!
//! None of these are stored or hashed. The API writes a [`HarnessToolEntry`] per bound tool when
//! a run starts, carrying the tool's whole model-facing descriptor and how to invoke it. A
//! [`ToolServersFile`] lists the MCP servers a harness may reach in Thorium's neutral format.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

use super::{
    AgenticTool, AgenticToolBacking, McpConfigFormat, ToolCapabilities, ToolEffect, ToolIcon,
    ToolLoading, ToolPortabilityReport, ToolResultFormat, ToolStrictness,
};

/// The longest an MCP server id may be
const MAX_SERVER_ID_LEN: usize = 31;

/// One tool as written into a harness's `tools.json` (written by the API, read by the harness)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HarnessToolEntry {
    /// The name a model uses to call this tool
    pub function_name: String,
    /// A short human readable title for this tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The description given to the model explaining what this tool does and when to use it
    pub model_description: String,
    /// The JSON schema describing this tool's input arguments
    #[cfg_attr(feature = "api", schema(value_type = Object))]
    pub input_schema: serde_json::Map<String, serde_json::Value>,
    /// The JSON schema describing this tool's structured output if it has one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "api", schema(value_type = Option<Object>))]
    pub output_schema: Option<serde_json::Map<String, serde_json::Value>>,
    /// Example inputs that show a model how to call this tool
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "api", schema(value_type = Vec<Object>))]
    pub input_examples: Vec<serde_json::Value>,
    /// Whether model APIs that support strict tool schemas should enforce `input_schema` exactly
    #[serde(default)]
    pub strict: ToolStrictness,
    /// Whether a model sees this tool up front or finds it through tool search
    #[serde(default)]
    pub loading: ToolLoading,
    /// Icons a client may show for this tool
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<ToolIcon>,
    /// What this tool does when it runs and which files it takes or produces
    pub capabilities: ToolCapabilities,
    /// The MCP annotations derived from this tool's title and capabilities
    pub annotations: ToolAnnotations,
    /// How portable this tool's schemas are across model APIs
    pub portability: ToolPortabilityReport,
    /// The format of the result this tool returns
    pub result_format: ToolResultFormat,
    /// Whether each call to this tool must be approved by the user before it runs
    #[serde(default)]
    pub requires_approval: bool,
    /// The max bytes of this tool's result returned to the model: the smaller non-zero of the
    /// backing's and the harness's limits (0 = unlimited)
    #[serde(default)]
    pub max_result_bytes: u64,
    /// The max number of seconds a single call to this tool may run for
    pub timeout: Option<u64>,
    /// How the harness carries out a call to this tool
    pub invocation: ToolInvocation,
}

/// A tool's MCP annotations, derived by Thorium from its spec (serialized with MCP's field names)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
// each hint is a separate MCP annotation
#[allow(clippy::struct_excessive_bools)]
pub struct ToolAnnotations {
    /// The tool's title (`AgenticTool.title`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The tool changes nothing (no effects)
    pub read_only_hint: bool,
    /// The tool can delete or overwrite data (the `Destructive` effect)
    pub destructive_hint: bool,
    /// Repeating a call with the same input has no further effect (`idempotent`)
    pub idempotent_hint: bool,
    /// The tool reaches outside Thorium (`open_world`, or the `WritesExternal` effect)
    pub open_world_hint: bool,
}

impl From<&AgenticTool> for ToolAnnotations {
    /// Derive a tool's MCP annotations from its title and capabilities
    ///
    /// # Arguments
    ///
    /// * `tool` - The agentic tool to describe
    fn from(tool: &AgenticTool) -> Self {
        // a tool with no effects is read-only and a declared destructive effect is destructive
        let effects = &tool.capabilities.effects;
        ToolAnnotations {
            title: tool.title.clone(),
            read_only_hint: effects.is_empty(),
            destructive_hint: effects.contains(&ToolEffect::Destructive),
            idempotent_hint: tool.capabilities.idempotent,
            open_world_hint: tool.capabilities.open_world
                || effects.contains(&ToolEffect::WritesExternal),
        }
    }
}

/// How a harness carries out calls to one tool, built from the tool's binding
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolInvocation {
    /// Call a tool built into the harness
    Native {
        /// The name of the tool within the harness
        tool: String,
        /// Harness-specific JSON config passed through unchanged
        config: Option<String>,
    },
    /// Call Thorium's REST API directly
    ThoriumApi {
        /// The tool instance to call
        tool_instance: Uuid,
        /// The tool's backing, which says which API calls to make
        backing: AgenticToolBacking,
    },
    /// Run a command; the harness or model appends `--input '<json>'`
    Cli {
        /// The command and its leading arguments
        command: Vec<String>,
    },
    /// Call a tool on an MCP server
    Mcp {
        /// The url of the MCP server
        server_url: String,
        /// The name of the MCP tool to call
        tool: String,
    },
}

/// Thorium's neutral MCP config format: the MCP servers a harness may reach
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolServersFile {
    /// Every MCP server the harness may reach, ordered by id
    pub servers: Vec<ToolServerDescriptor>,
}

/// How a harness reaches one MCP server and what it may do with its tools
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolServerDescriptor {
    /// The server's id (`^[a-z][a-z0-9-]{0,30}$`), used as its key in every rendered file
    pub id: String,
    /// A short human readable title for the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The server's version
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The MCP protocol revisions the server negotiates, newest first
    #[serde(default)]
    pub protocol_versions: Vec<String>,
    /// How to reach or start the server
    pub transport: ToolServerTransport,
    /// The secret inputs the transport and auth reference, by input name
    #[serde(default)]
    pub inputs: BTreeMap<String, ToolServerInput>,
    /// How the harness authenticates to the server
    #[serde(default)]
    pub auth: ToolServerAuth,
    /// How long the harness waits for the server
    #[serde(default)]
    pub timeouts: ToolServerTimeouts,
    /// Whether the harness connects to this server
    pub enabled: bool,
    /// Whether the harness must fail to start when it can't connect to this server
    pub required: bool,
    /// Which tools the model sees and which calls need approval
    #[serde(default)]
    pub tools: ToolServerTools,
    /// Whether the harness may trust this server's tool annotations
    #[serde(default)]
    pub trust: ToolServerTrust,
    /// Harness-specific settings merged last into that format's rendered entry, by format
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "api", schema(value_type = Object))]
    pub extensions: BTreeMap<McpConfigFormat, serde_json::Value>,
}

/// Check whether a string is a valid MCP server id (`^[a-z][a-z0-9-]{0,30}$`)
///
/// Server ids have no `_`, so a harness that prefixes tool names with the server id can split
/// the name back, and they're short, so prefixed tool names stay within 64 characters.
///
/// # Arguments
///
/// * `id` - The id to check
#[must_use]
pub fn is_valid_server_id(id: &str) -> bool {
    // the id starts with a lowercase letter and has at most 31 lowercase letters, digits, or
    // hyphens
    id.len() <= MAX_SERVER_ID_LEN
        && id
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// How a harness reaches or starts an MCP server
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolServerTransport {
    /// A remote server over MCP Streamable HTTP
    Http {
        /// The server's url
        url: String,
        /// Headers sent with every request, by header name
        #[serde(default)]
        headers: BTreeMap<String, ToolServerValue>,
    },
    /// A local server the harness starts and talks to over stdin and stdout
    Stdio {
        /// The program to run
        command: String,
        /// The program's arguments
        #[serde(default)]
        args: Vec<String>,
        /// Env vars set for the program, by name
        #[serde(default)]
        env: BTreeMap<String, ToolServerValue>,
        /// The directory the program runs in (unset = the harness's own)
        #[serde(default)]
        cwd: Option<String>,
    },
}

/// A header or env value in a descriptor: a literal, or a reference to a secret input
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum ToolServerValue {
    /// A literal value that isn't secret
    Literal(String),
    /// The value of a secret input, with an optional prefix such as `Bearer `
    Input {
        /// The name of the input in `ToolServerDescriptor.inputs`
        input: String,
        /// Text put before the input's value
        #[serde(default)]
        prefix: Option<String>,
    },
}

/// A secret a descriptor references by name; its value is never written into a config file
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolServerInput {
    /// What the secret is for
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The env var of the harness process that holds the value
    pub env: String,
}

/// How a harness authenticates to an MCP server
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolServerAuth {
    /// No authentication beyond the transport's headers
    #[default]
    None,
    /// An `Authorization: Bearer <value>` header with the value of an input
    Bearer {
        /// The name of the input in `ToolServerDescriptor.inputs`
        input: String,
    },
}

/// How long a harness waits for an MCP server
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolServerTimeouts {
    /// The max milliseconds to wait for the server to start or answer its first request
    #[serde(default)]
    pub startup_ms: Option<u64>,
    /// The max milliseconds a tool call may take (unset = the harness's default)
    #[serde(default)]
    pub tool_call_ms: Option<u64>,
}

/// Which of a server's tools the model sees and which calls need approval
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolServerTools {
    /// Which tools the model sees
    #[serde(default)]
    pub expose: ToolExposure,
    /// Which calls the harness runs without asking, asks about, or refuses
    #[serde(default)]
    pub approval: ToolApproval,
    /// Per-tool settings, by MCP tool name
    #[serde(default)]
    pub overrides: BTreeMap<String, ToolServerToolOverride>,
}

/// Which of a server's tools the model sees (a tool in both lists is hidden)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolExposure {
    /// The only tools shown, by MCP tool name (empty = every tool the server lists)
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools never shown, by MCP tool name
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Which calls a harness runs without asking, asks about, or refuses (`deny` wins over `ask`,
/// and `ask` over `auto`)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolApproval {
    /// What happens to a call of a tool in none of the lists
    #[serde(default)]
    pub default: ApprovalMode,
    /// Tools whose calls run without asking
    #[serde(default)]
    pub auto: Vec<String>,
    /// Tools whose calls need a user's approval
    #[serde(default)]
    pub ask: Vec<String>,
    /// Tools whose calls are refused
    #[serde(default)]
    pub deny: Vec<String>,
}

/// What a harness does with one tool call
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ApprovalMode {
    /// Run the call without asking
    Auto,
    /// Ask a user to approve the call first
    #[default]
    Ask,
    /// Refuse the call
    Deny,
}

/// Settings for one tool of an MCP server
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolServerToolOverride {
    /// The max milliseconds a call of this tool may take
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// The max tokens of this tool's output a harness passes to the model
    #[serde(default)]
    pub output_token_limit: Option<u64>,
}

/// Whether a harness may trust an MCP server's tool annotations
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolServerTrust {
    /// The annotations are hints only
    #[default]
    Untrusted,
    /// The annotations come from a source the deployment trusts
    Trusted,
}
