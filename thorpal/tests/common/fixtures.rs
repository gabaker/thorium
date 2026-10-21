//! The files Thorium writes for a workflow run, built for tests

use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use thorium::models::{
    AgenticToolBacking, CredentialKind, CredentialSecrets, HarnessAgentFile, HarnessCredential,
    HarnessModelEntry, HarnessModels, HarnessRunFile, HarnessToolEntry, ModelApi,
    ModelCapabilities, ModelConfigRef, ModelLimits, ModelParameters, ModelProvider, ModelSecret,
    ToolAnnotations, ToolCapabilities, ToolInvocation, ToolLoading, ToolPortabilityReport,
    ToolResultFormat, ToolStrictness,
};
use uuid::Uuid;

/// The API key of the fixture's credential
pub const KEY: &str = "fixture-key-europa-k3f9";

/// The model id of the fixture's model
pub const MODEL: &str = "claude-fixture-io-k3f9";

/// The run token in the fixture's keys file
pub const TOKEN: &str = "fixture-run-token-ganymede";

/// The function name of the fixture's tool
pub const TOOL: &str = "lookup_file";

/// Build a model entry for the Anthropic provider
///
/// # Arguments
///
/// * `base_url` - The endpoint's base url
/// * `api` - The protocol to call the model with
/// * `kind` - The credential's kind
pub fn model(base_url: &str, api: ModelApi, kind: CredentialKind) -> HarnessModelEntry {
    // an Anthropic endpoint with a credential holding the fixture key
    HarnessModelEntry {
        provider: ModelProvider::Anthropic {
            base_url: Some(base_url.to_owned()),
            version: "2023-06-01".to_owned(),
            betas: std::collections::BTreeSet::new(),
            workspace_id: None,
        },
        model_id: MODEL.to_owned(),
        api,
        parameters: ModelParameters::default(),
        limits: ModelLimits::default(),
        capabilities: ModelCapabilities::default(),
        credential: Some(HarnessCredential::new(
            ModelConfigRef::Group(Uuid::new_v4()),
            kind,
            CredentialSecrets {
                primary: Some(ModelSecret::from(KEY)),
                session_token: None,
                fingerprint: None,
            },
            None,
        )),
    }
}

/// Build the `tools.json` entry of a tool backed by Thorium's MCP server
pub fn mcp_tool() -> HarnessToolEntry {
    // a tool with one required string property
    let schema = json!({
        "type": "object",
        "properties": {"sha256": {"type": "string", "description": "The file's SHA256"}},
        "required": ["sha256"],
        "additionalProperties": false,
    });
    HarnessToolEntry {
        function_name: TOOL.to_owned(),
        title: Some("Look up a file".to_owned()),
        model_description: "Look up a file's details in Thorium".to_owned(),
        input_schema: schema.as_object().unwrap().clone(),
        output_schema: None,
        input_examples: Vec::new(),
        strict: ToolStrictness::Auto,
        loading: ToolLoading::Eager,
        icons: Vec::new(),
        capabilities: ToolCapabilities::default(),
        annotations: ToolAnnotations::default(),
        portability: ToolPortabilityReport::default(),
        result_format: ToolResultFormat::Json,
        requires_approval: false,
        max_result_bytes: 0,
        timeout: Some(60),
        invocation: ToolInvocation::ThoriumApi {
            tool_instance: Uuid::new_v4(),
            backing: AgenticToolBacking::ThoriumMcp {
                tool: "get_file".to_owned(),
            },
        },
    }
}

/// The files of one workflow run
pub struct RunFixture {
    /// The run request
    pub run: HarnessRunFile,
    /// The entry agent and its sub-agents
    pub agent: HarnessAgentFile,
    /// The bound tools by tool instance id
    pub tools: BTreeMap<Uuid, HarnessToolEntry>,
    /// Every agent's model
    pub models: HarnessModels,
    /// The Thorium API url in the keys file
    pub api: String,
}

impl RunFixture {
    /// Build a run of one agent with one tool, one sample, and the fixture model
    ///
    /// # Arguments
    ///
    /// * `base_url` - The model endpoint's base url
    /// * `api` - The Thorium API url in the keys file
    pub fn new(base_url: &str, api: &str) -> Self {
        // the entry agent is bound to the one tool through Thorium's API
        let agent = Uuid::new_v4();
        let tool_instance = Uuid::new_v4();
        let snapshot = json!({
            "agent": agent,
            "name": "io-k3f9",
            "groups": ["jupiter-k3f9"],
            "config": {
                "system_prompt": "You are a careful analyst.",
                "prompt": "Describe the input file.",
            },
            "config_hash": "hash",
            "harness": null,
            "tools": [tool_instance],
            "bindings": [{"tool_instance": tool_instance, "mechanism": "ThoriumApi"}],
            "sub_agents": [],
            "toolset_digest": "digest",
        });
        let run = json!({
            "reaction": Uuid::new_v4(),
            "samples": ["a".repeat(64)],
            "workflow_instance": Uuid::new_v4(),
            "digest": "digest",
            "agent": agent,
        });
        let mut tool = mcp_tool();
        tool.invocation = ToolInvocation::ThoriumApi {
            tool_instance,
            backing: AgenticToolBacking::ThoriumMcp {
                tool: "get_file".to_owned(),
            },
        };
        RunFixture {
            run: serde_json::from_value(run).unwrap(),
            agent: HarnessAgentFile {
                agent: serde_json::from_value(snapshot).unwrap(),
                sub_agents: Vec::new(),
            },
            tools: BTreeMap::from([(tool_instance, tool)]),
            models: HarnessModels {
                agents: BTreeMap::from([(
                    agent,
                    model(
                        base_url,
                        ModelApi::AnthropicMessages,
                        CredentialKind::ApiKey,
                    ),
                )]),
            },
            api: api.to_owned(),
        }
    }

    /// Get the entry agent's model
    pub fn entry_model(&mut self) -> &mut HarnessModelEntry {
        // the entry agent is the run's agent
        let agent = self.run.agent;
        self.models.agents.get_mut(&agent).unwrap()
    }

    /// Write the run's files into a directory and get the harness arguments that point at them
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to write into
    pub fn write(&self, dir: &Path) -> Vec<String> {
        // write each input file
        let ephemeral = dir.join("ephemeral");
        std::fs::create_dir_all(&ephemeral).unwrap();
        let write = |name: &str, value: String| {
            let path = ephemeral.join(name);
            std::fs::write(&path, value).unwrap();
            path.to_string_lossy().into_owned()
        };
        let run = write("run.json", serde_json::to_string(&self.run).unwrap());
        let agent = write("agent.json", serde_json::to_string(&self.agent).unwrap());
        let tools = write("tools.json", serde_json::to_string(&self.tools).unwrap());
        let models = write("models.json", serde_json::to_string(&self.models).unwrap());
        let keys = write("keys.yml", format!("api: {}\ntoken: {TOKEN}\n", self.api));
        // the outputs go under the directory too
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        [
            ("--run", run),
            ("--agent", agent),
            ("--tools", tools),
            ("--models", models),
            ("--keys", keys),
            ("--results", path("results")),
            ("--status", path("result-files/status.json")),
            ("--result-files", path("result-files")),
            ("--samples", path("samples")),
        ]
        .into_iter()
        .flat_map(|(flag, value)| [flag.to_owned(), value])
        .collect()
    }
}
