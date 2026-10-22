//! The files the API writes for a workflow run's harness
//!
//! A workflow run's reaction carries three buffers that the thorium-agent downloads next to the
//! run's inputs: `run.json` (what the run is and its limits), `agent.json` (the entry agent's
//! snapshot and its direct sub-agents'), and `tools.json` (every bound tool's model-facing
//! descriptor and how to invoke it).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use uuid::Uuid;

use crate::conf::{Conf, ToolAccessConf};
use crate::models::{
    AgenticTool, AgenticToolBacking, HarnessAgentFile, HarnessRunFile, HarnessSpec,
    HarnessToolEntry, InstancedAgent, ReactionModelBinding, RepoDependency, ToolAnnotations,
    ToolInvocation, ToolMechanism, WorkflowInstanceEntity,
};

/// The name of a workflow run's request file
pub const RUN_FILE: &str = "run.json";

/// The name of a workflow run's agent file
pub const AGENT_FILE: &str = "agent.json";

/// The name of a workflow run's tools file
pub const TOOLS_FILE: &str = "tools.json";

/// The MCP tool that runs a tool behind Thorium for a model
pub const RUN_TOOL: &str = "run_tool";

/// The MCP tool that reads a tool run's status
pub const GET_TOOL_RUN: &str = "get_tool_run";

/// The MCP tool that reads a finished tool run's results
pub const GET_TOOL_RUN_RESULTS: &str = "get_tool_run_results";

/// Where one harness reaches the tools behind Thorium
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAccess {
    /// The CLI wrapper's base command including its subcommand
    pub cli: Vec<String>,
    /// The url of Thorium's MCP server as seen from inside the harness
    pub mcp_url: String,
}

impl ToolAccess {
    /// Resolve a harness's tool access, falling back to the deployment's defaults per field
    ///
    /// An empty `cli` or an unset `mcp_url` on the harness uses the deployment's value, and an
    /// unset deployment `mcp_url` uses the API url given to workers plus `/api/mcp`.
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness the run's agents run in
    /// * `defaults` - The deployment's tool access settings
    /// * `worker_api_url` - The API url given to workers
    #[must_use]
    pub fn resolve(harness: &HarnessSpec, defaults: &ToolAccessConf, worker_api_url: &str) -> Self {
        // get the harness's own overrides, if it has any
        let overrides = harness.interface.tool_access.as_ref();
        // a harness's non-empty CLI command wins over the deployment's
        let cli = match overrides {
            Some(access) if !access.cli.is_empty() => access.cli.clone(),
            _ => defaults.cli.clone(),
        };
        // a harness's MCP url wins over the deployment's, which wins over the worker API url
        let mcp_url = overrides
            .and_then(|access| access.mcp_url.clone())
            .or_else(|| defaults.mcp_url.clone())
            .unwrap_or_else(|| format!("{}/api/mcp", worker_api_url.trim_end_matches('/')));
        ToolAccess { cli, mcp_url }
    }
}

/// Get the API url the K8s scaler gives the workers of the primary cluster
///
/// # Arguments
///
/// * `conf` - The Thorium config
#[must_use]
pub fn worker_api_url(conf: &Conf) -> String {
    // use the primary cluster's override or the in cluster service url
    let k8s = &conf.thorium.scaler.k8s;
    k8s.resolved_api_url(&k8s.primary_cluster, &conf.thorium.namespace)
}

/// Get the most result bytes returned to a model: the smaller non-zero of two limits
///
/// # Arguments
///
/// * `backing` - The tool backing's limit (0 = unlimited)
/// * `harness` - The harness's limit (0 = unlimited)
#[must_use]
pub fn effective_max_result_bytes(backing: u64, harness: u64) -> u64 {
    // the CLI wrapper resolves result limits with the same rule
    crate::models::effective_max_result_bytes(backing, harness)
}

/// Build the invocation recipe of one bound tool
///
/// # Arguments
///
/// * `tool_instance` - The id of the bound tool instance
/// * `tool` - The bound tool's spec
/// * `mechanism` - The mechanism the tool was bound to
/// * `access` - Where the harness reaches the tools behind Thorium
fn invocation(
    tool_instance: Uuid,
    tool: &AgenticTool,
    mechanism: ToolMechanism,
    access: &ToolAccess,
) -> Result<ToolInvocation, String> {
    // build the recipe for the binding and the backing
    match (mechanism, &tool.backing) {
        // a native tool is carried out by the harness's built in tool
        (ToolMechanism::Native, AgenticToolBacking::HarnessNative { tool, config }) => {
            Ok(ToolInvocation::Native {
                tool: tool.clone(),
                config: config.clone(),
            })
        }
        // a harness that calls Thorium itself gets the instance and its backing
        (
            ToolMechanism::ThoriumApi,
            backing @ (AgenticToolBacking::ThoriumMcp { .. } | AgenticToolBacking::Container(_)),
        ) => Ok(ToolInvocation::ThoriumApi {
            tool_instance,
            backing: backing.clone(),
        }),
        // the CLI wrapper runs the instance, bounded by the tool's own timeout
        (
            ToolMechanism::Cli,
            AgenticToolBacking::ThoriumMcp { .. } | AgenticToolBacking::Container(_),
        ) => {
            let mut command = access.cli.clone();
            command.push(tool_instance.to_string());
            // the wrapper waits at most the tool's timeout when it has one
            if let Some(timeout) = tool.timeout {
                command.push("--timeout".to_owned());
                command.push(timeout.to_string());
            }
            Ok(ToolInvocation::Cli { command })
        }
        // Thorium's MCP server serves a Thorium MCP tool directly
        (ToolMechanism::Mcp, AgenticToolBacking::ThoriumMcp { tool }) => Ok(ToolInvocation::Mcp {
            server_url: access.mcp_url.clone(),
            tool: tool.clone(),
        }),
        // a container tool runs through the MCP server's one tool run tool
        (ToolMechanism::Mcp, AgenticToolBacking::Container(_)) => Ok(ToolInvocation::Mcp {
            server_url: access.mcp_url.clone(),
            tool: RUN_TOOL.to_owned(),
        }),
        // any other pairing can't come out of instancing
        (mechanism, backing) => Err(format!(
            "tool instance {tool_instance} is bound to {mechanism}, which its {} backing doesn't support",
            backing.name()
        )),
    }
}

/// Build the `tools.json` entry of one bound tool
///
/// # Arguments
///
/// * `tool_instance` - The id of the bound tool instance
/// * `tool` - The bound tool's spec
/// * `mechanism` - The mechanism the tool was bound to
/// * `harness` - The harness the run's agents run in
/// * `access` - Where the harness reaches the tools behind Thorium
pub fn tool_entry(
    tool_instance: Uuid,
    tool: &AgenticTool,
    mechanism: ToolMechanism,
    harness: &HarnessSpec,
    access: &ToolAccess,
) -> Result<HarnessToolEntry, String> {
    // build how the harness invokes this tool
    let invocation = invocation(tool_instance, tool, mechanism, access)?;
    // a container tool shared through the MCP server's run tool names its instance and the
    // tools that read its run back
    let mut model_description = tool.model_description.clone();
    if mechanism == ToolMechanism::Mcp && matches!(tool.backing, AgenticToolBacking::Container(_)) {
        // writing into a String never fails
        let _ = write!(
            model_description,
            "\n\nRun this tool with the `{RUN_TOOL}` MCP tool and tool instance `{tool_instance}`, then read the run with `{GET_TOOL_RUN}` and its results with `{GET_TOOL_RUN_RESULTS}`."
        );
    }
    // a container backing may limit its result below the harness's limit
    let backing_limit = match &tool.backing {
        AgenticToolBacking::Container(container) => container.outputs.max_result_bytes,
        AgenticToolBacking::ThoriumMcp { .. } | AgenticToolBacking::HarnessNative { .. } => 0,
    };
    Ok(HarnessToolEntry {
        function_name: tool.function_name.clone(),
        title: tool.title.clone(),
        model_description,
        input_schema: tool.input_schema.clone(),
        output_schema: tool.output_schema.clone(),
        input_examples: tool.input_examples.clone(),
        strict: tool.strict,
        loading: tool.loading,
        icons: tool.icons.clone(),
        capabilities: tool.capabilities.clone(),
        annotations: ToolAnnotations::from(tool),
        portability: tool.portability(),
        result_format: tool.result_format,
        requires_approval: tool.requires_approval,
        max_result_bytes: effective_max_result_bytes(
            backing_limit,
            harness.tool_policy.max_result_bytes,
        ),
        timeout: tool.timeout,
        invocation,
    })
}

/// Build a workflow run's `tools.json`: one entry per tool bound to any of its agents, keyed by
/// tool instance id
///
/// Delegation tools have no binding, so they're never listed.
///
/// # Arguments
///
/// * `instance` - The workflow instance being run
/// * `harness` - The entry agent's harness
/// * `specs` - The spec of every bound tool instance, by id
/// * `access` - Where the harness reaches the tools behind Thorium
pub fn tools_file<S: std::hash::BuildHasher>(
    instance: &WorkflowInstanceEntity,
    harness: &HarnessSpec,
    specs: &HashMap<Uuid, AgenticTool, S>,
    access: &ToolAccess,
) -> Result<BTreeMap<Uuid, HarnessToolEntry>, String> {
    // describe each bound tool once
    let mut entries = BTreeMap::new();
    for binding in instance.agents.iter().flat_map(|agent| &agent.bindings) {
        // skip tools another agent's binding already described
        if entries.contains_key(&binding.tool_instance) {
            continue;
        }
        // every bound tool must have a spec
        let Some(spec) = specs.get(&binding.tool_instance) else {
            return Err(format!(
                "bound tool instance {} has no agentic spec",
                binding.tool_instance
            ));
        };
        let entry = tool_entry(
            binding.tool_instance,
            spec,
            binding.mechanism,
            harness,
            access,
        )?;
        entries.insert(binding.tool_instance, entry);
    }
    Ok(entries)
}

/// Build a workflow run's `agent.json`: the entry agent and its direct sub-agents
///
/// # Arguments
///
/// * `instance` - The workflow instance being run
/// * `entry` - The entry agent
pub fn agent_file(
    instance: &WorkflowInstanceEntity,
    entry: &InstancedAgent,
) -> Result<HarnessAgentFile, String> {
    // snapshot each agent the entry agent delegates to
    let mut sub_agents = Vec::with_capacity(entry.sub_agents.len());
    for id in &entry.sub_agents {
        let Some(sub) = instance.agent(*id) else {
            return Err(format!("sub-agent {id} isn't in the workflow instance"));
        };
        sub_agents.push(sub.clone());
    }
    // keep the sub-agents in id order
    sub_agents.sort_by_key(|sub| sub.agent);
    Ok(HarnessAgentFile {
        agent: entry.clone(),
        sub_agents,
    })
}

/// The parts of a workflow run that its `run.json` describes
pub struct RunFileParts<'a> {
    /// The id of the run's reaction
    pub reaction: Uuid,
    /// The SHA256s of the run's files
    pub samples: &'a [String],
    /// The run's repos, each with the commitish it's checked out at
    pub repos: &'a [RepoDependency],
    /// The id of the workflow instance being run
    pub workflow_instance: Uuid,
    /// The workflow instance being run
    pub instance: &'a WorkflowInstanceEntity,
    /// The entry agent
    pub entry: &'a InstancedAgent,
    /// The entry agent's harness
    pub harness: &'a HarnessSpec,
    /// The model each agent resolved to
    pub models: &'a [ReactionModelBinding],
}

/// Build a workflow run's `run.json`
///
/// # Arguments
///
/// * `parts` - The parts of the run to describe
#[must_use]
pub fn run_file(parts: &RunFileParts) -> HarnessRunFile {
    // copy the run's identity, inputs, and limits
    HarnessRunFile {
        reaction: parts.reaction,
        samples: parts.samples.to_vec(),
        repos: parts.repos.to_vec(),
        workflow_instance: parts.workflow_instance,
        digest: parts.instance.digest.clone(),
        agent: parts.entry.agent,
        limits: parts.instance.limits.clone(),
        max_result_bytes: parts.harness.tool_policy.max_result_bytes,
        models: parts.models.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use uuid::Uuid;

    use super::{
        RunFileParts, ToolAccess, agent_file, effective_max_result_bytes, run_file, tool_entry,
        tools_file,
    };
    use crate::conf::ToolAccessConf;
    use crate::models::{
        AgentConfig, AgenticTool, AgenticToolBacking, ContainerToolBacking, HarnessSpec,
        HarnessToolAccess, InstancedAgent, InstancedToolBinding, RepoDependency, ToolCapabilities,
        ToolEffect, ToolIcon, ToolInvocation, ToolLoading, ToolMechanism, ToolOutputDelivery,
        ToolStrictness, WorkflowInstanceEntity, WorkflowInstanceStatus, WorkflowInstanceTrigger,
        WorkflowLimits,
    };

    /// The API url the tests give workers
    const WORKERS: &str = "http://thorium-api.thorium.svc.cluster.local.";

    /// Build an object input schema with one string property
    fn schema() -> serde_json::Map<String, serde_json::Value> {
        // an object with a sha256 property
        serde_json::json!({"type": "object", "properties": {"sha256": {"type": "string"}}})
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    /// Build an agentic tool with every model-facing field set
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's function name
    /// * `backing` - How the tool is invoked
    fn tool(name: &str, backing: AgenticToolBacking) -> AgenticTool {
        // fill every descriptor field so each one can be checked
        AgenticTool {
            function_name: name.to_owned(),
            title: Some(format!("{name} title")),
            model_description: format!("Runs {name}"),
            input_schema: schema(),
            output_schema: Some(schema()),
            capabilities: ToolCapabilities {
                effects: std::iter::once(ToolEffect::WritesExternal).collect(),
                idempotent: true,
                ..ToolCapabilities::default()
            },
            backing,
            requires_approval: true,
            timeout: Some(600),
            input_examples: vec![serde_json::json!({"sha256": "abc"})],
            strict: ToolStrictness::On,
            loading: ToolLoading::Deferred,
            icons: vec![ToolIcon {
                src: "https://moons.example/io.png".to_owned(),
                mime_type: None,
                sizes: Vec::new(),
                theme: None,
            }],
            ..AgenticTool::default()
        }
    }

    /// Build a container backing with a result limit
    ///
    /// # Arguments
    ///
    /// * `max_result_bytes` - The backing's result limit
    fn container(max_result_bytes: u64) -> AgenticToolBacking {
        // a container tool pinning a random image tool instance
        AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: Uuid::new_v4(),
            outputs: ToolOutputDelivery {
                path: None,
                max_result_bytes,
            },
            ..ContainerToolBacking::default()
        })
    }

    /// Build a harness with a result limit and optional tool access overrides
    ///
    /// # Arguments
    ///
    /// * `max_result_bytes` - The harness's result limit
    /// * `access` - The harness's tool access overrides
    fn harness(max_result_bytes: u64, access: Option<HarnessToolAccess>) -> HarnessSpec {
        // a default harness with the given limit and overrides
        let mut harness = HarnessSpec::default();
        harness.tool_policy.max_result_bytes = max_result_bytes;
        harness.interface.tool_access = access;
        harness
    }

    /// Build the deployment's tool access settings used by the fallback tests
    fn deployment() -> ToolAccessConf {
        // a deployment that sets both values
        ToolAccessConf {
            cli: vec!["/d/agent".to_owned(), "tool".to_owned(), "run".to_owned()],
            mcp_url: Some("http://d/api/mcp".to_owned()),
        }
    }

    /// Build an agent snapshot with bindings and sub-agents
    ///
    /// # Arguments
    ///
    /// * `bindings` - The agent's tool bindings
    /// * `sub_agents` - The agents it delegates to
    fn agent(bindings: &[(Uuid, ToolMechanism)], sub_agents: &[Uuid]) -> InstancedAgent {
        // an agent bound to each given tool
        InstancedAgent {
            agent: Uuid::new_v4(),
            name: "europa".to_owned(),
            groups: vec!["jupiter".to_owned()],
            config: AgentConfig::default(),
            config_hash: "hash".to_owned(),
            harness: Some(Uuid::new_v4()),
            tools: bindings.iter().map(|(id, _)| *id).collect(),
            tool_sets: Vec::new(),
            bindings: bindings
                .iter()
                .map(|(tool_instance, mechanism)| InstancedToolBinding {
                    tool_instance: *tool_instance,
                    mechanism: *mechanism,
                })
                .collect(),
            sub_agents: sub_agents.to_vec(),
            toolset_digest: "digest".to_owned(),
        }
    }

    /// Build a workflow instance of some agents
    ///
    /// # Arguments
    ///
    /// * `agents` - The instance's agents, the first being the entry agent
    fn instance(agents: Vec<InstancedAgent>) -> WorkflowInstanceEntity {
        // a ready instance with a sub-agent limit
        WorkflowInstanceEntity {
            workflow: Uuid::new_v4(),
            workflow_groups: vec!["jupiter".to_owned()],
            revision: 1,
            previous: None,
            status: WorkflowInstanceStatus::Ready,
            warnings: Vec::new(),
            trigger: WorkflowInstanceTrigger::Manual,
            triggered_by: String::new(),
            entry_agent: agents.first().map(|agent| agent.agent),
            limits: WorkflowLimits {
                max_alive_sub_agents: Some(3),
            },
            default_models: Vec::new(),
            agents,
            tools: Vec::new(),
            digest: "v1:instance".to_owned(),
        }
    }

    /// Each tools file entry carries the tool's full descriptor, its effective result limit, its
    /// annotations, and its portability report
    #[test]
    fn tools_file_full_descriptor() {
        // a Thorium MCP tool, a container tool, and a native tool
        let mcp = tool(
            "get_sample",
            AgenticToolBacking::ThoriumMcp {
                tool: "get_sample".to_owned(),
            },
        );
        let boxed = tool("run_ghidra", container(1000));
        let native = tool(
            "decompile",
            AgenticToolBacking::HarnessNative {
                tool: "ghidra_decompile".to_owned(),
                config: None,
            },
        );
        let access = ToolAccess::resolve(&harness(500, None), &deployment(), WORKERS);
        for (spec, mechanism, limit) in [
            (&mcp, ToolMechanism::Mcp, 500),
            (&boxed, ToolMechanism::Cli, 500),
            (&native, ToolMechanism::Native, 500),
        ] {
            let id = Uuid::new_v4();
            let entry = tool_entry(id, spec, mechanism, &harness(500, None), &access).unwrap();
            // every model-facing field is copied from the spec
            assert_eq!(entry.function_name, spec.function_name);
            assert_eq!(entry.title, spec.title);
            assert_eq!(entry.output_schema, spec.output_schema);
            assert_eq!(entry.input_examples, spec.input_examples);
            assert_eq!(entry.strict, spec.strict);
            assert_eq!(entry.loading, spec.loading);
            assert_eq!(entry.icons, spec.icons);
            assert_eq!(entry.requires_approval, spec.requires_approval);
            // the annotations and portability are derived from the spec
            assert_eq!(
                entry.annotations,
                crate::models::ToolAnnotations::from(spec)
            );
            assert!(entry.annotations.open_world_hint);
            assert_eq!(entry.portability, spec.portability());
            // the result limit is the smaller non-zero of the backing's and the harness's
            assert_eq!(entry.max_result_bytes, limit);
        }
        // a container limit below the harness's wins, and two unlimited stay unlimited
        let small = tool("small", container(200));
        let entry = tool_entry(
            Uuid::new_v4(),
            &small,
            ToolMechanism::Cli,
            &harness(500, None),
            &access,
        )
        .unwrap();
        assert_eq!(entry.max_result_bytes, 200);
        let open = tool("open", container(0));
        let entry = tool_entry(
            Uuid::new_v4(),
            &open,
            ToolMechanism::Cli,
            &harness(0, None),
            &access,
        )
        .unwrap();
        assert_eq!(entry.max_result_bytes, 0);
        assert_eq!(effective_max_result_bytes(1000, 500), 500);
        assert_eq!(effective_max_result_bytes(0, 0), 0);
        assert_eq!(effective_max_result_bytes(0, 700), 700);
    }

    /// Each binding gets its own invocation recipe: native tools name the harness's built in
    /// tool, the Thorium API names the instance and backing, the CLI wrapper names the instance
    /// and the tool's timeout, and MCP names Thorium's server and its tool
    #[test]
    #[allow(clippy::too_many_lines)]
    fn tools_file_invocations() {
        // a native tool, an MCP tool, and a container tool bound four ways
        let native = tool(
            "decompile",
            AgenticToolBacking::HarnessNative {
                tool: "ghidra_decompile".to_owned(),
                config: Some("{\"depth\":2}".to_owned()),
            },
        );
        let mcp = tool(
            "get_sample",
            AgenticToolBacking::ThoriumMcp {
                tool: "get_sample".to_owned(),
            },
        );
        let boxed = tool("run_ghidra", container(0));
        let (native_id, mcp_id, api_id, cli_id, boxed_mcp_id) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let entry = agent(
            &[
                (native_id, ToolMechanism::Native),
                (mcp_id, ToolMechanism::Mcp),
                (api_id, ToolMechanism::ThoriumApi),
                (cli_id, ToolMechanism::Cli),
            ],
            &[],
        );
        let sub = agent(
            &[
                (boxed_mcp_id, ToolMechanism::Mcp),
                (cli_id, ToolMechanism::Cli),
            ],
            &[],
        );
        let run = instance(vec![entry, sub]);
        let specs = HashMap::from([
            (native_id, native),
            (mcp_id, mcp.clone()),
            (api_id, boxed.clone()),
            (cli_id, boxed.clone()),
            (boxed_mcp_id, boxed.clone()),
        ]);
        let spec = harness(0, None);
        let access = ToolAccess::resolve(&spec, &ToolAccessConf::default(), WORKERS);
        let file = tools_file(&run, &spec, &specs, &access).unwrap();
        // one entry per bound tool instance, keyed by its id
        assert_eq!(
            file.keys().copied().collect::<BTreeSet<Uuid>>(),
            [native_id, mcp_id, api_id, cli_id, boxed_mcp_id]
                .into_iter()
                .collect::<BTreeSet<Uuid>>()
        );
        assert_eq!(
            file[&native_id].invocation,
            ToolInvocation::Native {
                tool: "ghidra_decompile".to_owned(),
                config: Some("{\"depth\":2}".to_owned()),
            }
        );
        assert_eq!(
            file[&api_id].invocation,
            ToolInvocation::ThoriumApi {
                tool_instance: api_id,
                backing: boxed.backing.clone(),
            }
        );
        assert_eq!(
            file[&cli_id].invocation,
            ToolInvocation::Cli {
                command: vec![
                    "/opt/thorium/thorium-agent".to_owned(),
                    "tool".to_owned(),
                    "run".to_owned(),
                    cli_id.to_string(),
                    "--timeout".to_owned(),
                    "600".to_owned(),
                ],
            }
        );
        let url = format!("{WORKERS}/api/mcp");
        assert_eq!(
            file[&mcp_id].invocation,
            ToolInvocation::Mcp {
                server_url: url.clone(),
                tool: "get_sample".to_owned(),
            }
        );
        assert_eq!(
            file[&boxed_mcp_id].invocation,
            ToolInvocation::Mcp {
                server_url: url,
                tool: "run_tool".to_owned(),
            }
        );
        // a harness's tool access overrides both defaults
        let custom = harness(
            0,
            Some(HarnessToolAccess {
                cli: vec!["/h/wrap".to_owned(), "run".to_owned()],
                mcp_url: Some("http://h/mcp".to_owned()),
            }),
        );
        let access = ToolAccess::resolve(&custom, &ToolAccessConf::default(), WORKERS);
        let file = tools_file(&run, &custom, &specs, &access).unwrap();
        let ToolInvocation::Cli { command } = &file[&cli_id].invocation else {
            panic!("the CLI binding isn't a CLI invocation");
        };
        assert_eq!(&command[..3], ["/h/wrap", "run", &cli_id.to_string()]);
        assert_eq!(
            file[&mcp_id].invocation,
            ToolInvocation::Mcp {
                server_url: "http://h/mcp".to_owned(),
                tool: "get_sample".to_owned(),
            }
        );
    }

    /// Two container tools bound to MCP share Thorium's one tool run tool, and each entry's
    /// description names its tool instance and the tools that read the run back
    #[test]
    fn tools_file_mcp_container_tools() {
        // two container tools bound to MCP
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let run = instance(vec![agent(
            &[(first, ToolMechanism::Mcp), (second, ToolMechanism::Mcp)],
            &[],
        )]);
        let specs = HashMap::from([
            (first, tool("yara_scan", container(0))),
            (second, tool("run_ghidra", container(0))),
        ]);
        let spec = harness(0, None);
        let access = ToolAccess::resolve(&spec, &deployment(), WORKERS);
        let file = tools_file(&run, &spec, &specs, &access).unwrap();
        for id in [first, second] {
            // both name the same server and its run tool
            assert_eq!(
                file[&id].invocation,
                ToolInvocation::Mcp {
                    server_url: "http://d/api/mcp".to_owned(),
                    tool: "run_tool".to_owned(),
                }
            );
            // the description names the instance and the read back tools
            let description = &file[&id].model_description;
            assert!(description.contains(&id.to_string()), "{description}");
            assert!(description.contains("get_tool_run"), "{description}");
            assert!(
                description.contains("get_tool_run_results"),
                "{description}"
            );
        }
    }

    /// A harness's tool access falls back to the deployment's settings one field at a time,
    /// and an unset deployment MCP url falls back to the worker API url
    #[test]
    fn tool_access_falls_back_per_field() {
        // a harness that sets only its CLI command
        let only_cli = harness(
            0,
            Some(HarnessToolAccess {
                cli: vec!["/h/wrap".to_owned(), "run".to_owned()],
                mcp_url: None,
            }),
        );
        let access = ToolAccess::resolve(&only_cli, &deployment(), WORKERS);
        assert_eq!(access.cli, vec!["/h/wrap", "run"]);
        assert_eq!(access.mcp_url, "http://d/api/mcp");
        // a harness that sets only its MCP url and an empty CLI command
        let only_url = harness(
            0,
            Some(HarnessToolAccess {
                cli: Vec::new(),
                mcp_url: Some("http://h/mcp".to_owned()),
            }),
        );
        let access = ToolAccess::resolve(&only_url, &deployment(), WORKERS);
        assert_eq!(access.cli, vec!["/d/agent", "tool", "run"]);
        assert_eq!(access.mcp_url, "http://h/mcp");
        // a harness without tool access gets both deployment values
        let access = ToolAccess::resolve(&harness(0, None), &deployment(), WORKERS);
        assert_eq!(access.cli, vec!["/d/agent", "tool", "run"]);
        assert_eq!(access.mcp_url, "http://d/api/mcp");
        // without a deployment MCP url the worker API url is used
        let mut no_url = deployment();
        no_url.mcp_url = None;
        let access = ToolAccess::resolve(&harness(0, None), &no_url, WORKERS);
        assert_eq!(access.mcp_url, format!("{WORKERS}/api/mcp"));
    }

    /// The run file holds the run's inputs, identity, limits, and the harness's result limit,
    /// and the agent file holds the entry agent and one snapshot per direct sub-agent
    #[test]
    fn run_and_agent_files() {
        // an entry agent delegating to two sub-agents, one of which delegates further
        let deep = agent(&[], &[]);
        let first = agent(&[], &[deep.agent]);
        let second = agent(&[], &[]);
        let entry = agent(
            &[(Uuid::new_v4(), ToolMechanism::Cli)],
            &[first.agent, second.agent],
        );
        let run = instance(vec![entry.clone(), first.clone(), second.clone(), deep]);
        // build the run file
        let reaction = Uuid::new_v4();
        let samples = vec!["a".repeat(64)];
        let repos = vec![RepoDependency {
            url: "github.com/moons/io".to_owned(),
            commitish: Some("main".to_owned()),
            kind: None,
        }];
        let instance_id = Uuid::new_v4();
        let spec = harness(4096, None);
        let file = run_file(&RunFileParts {
            reaction,
            samples: &samples,
            repos: &repos,
            workflow_instance: instance_id,
            instance: &run,
            entry: &entry,
            harness: &spec,
            models: &[],
        });
        assert_eq!(file.reaction, reaction);
        assert_eq!(file.samples, samples);
        assert_eq!(file.repos[0].commitish.as_deref(), Some("main"));
        assert_eq!(file.workflow_instance, instance_id);
        assert_eq!(file.digest, "v1:instance");
        assert_eq!(file.agent, entry.agent);
        assert_eq!(file.limits.max_alive_sub_agents, Some(3));
        assert_eq!(file.max_result_bytes, 4096);
        // the agent file holds the entry agent and its two direct sub-agents only
        let agents = agent_file(&run, &entry).unwrap();
        assert_eq!(agents.agent, entry);
        let mut expected = vec![first.agent, second.agent];
        expected.sort();
        assert_eq!(
            agents
                .sub_agents
                .iter()
                .map(|sub| sub.agent)
                .collect::<Vec<Uuid>>(),
            expected
        );
        // the files serialize with no delegation entries in their bindings
        let raw = serde_json::to_value(&agents).unwrap();
        assert_eq!(raw["agent"]["bindings"].as_array().map(Vec::len), Some(1));
        let _: BTreeMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::to_value(&file).unwrap()).unwrap();
    }
}
