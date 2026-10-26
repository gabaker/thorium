//! Contains models for workflow catalogs and workflow instances
//!
//! A [`WorkflowCatalogEntity`] is the editable root that wires agents into a workflow through
//! its `Orchestrates` edges. A [`WorkflowInstanceEntity`] is a constant, fully resolved snapshot
//! of a catalog's graph: every agent reached, the exact tool instances each was given, the
//! workflow's limits and default models, and a content digest over all of it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::tools::{ToolKind, ToolMechanism, ToolSpec};
use crate::models::{AgentConfig, Entity, ModelAuthSource, ModelMatcher};

#[cfg(feature = "client")]
pub(crate) mod resolve;
#[cfg(test)]
mod tests;

#[cfg(feature = "client")]
pub use resolve::{
    DedupeKey, GraphEdge, GraphNode, GraphReader, ResolvedWorkflow, WatchedReader, WorkflowBuild,
    build_workflow_instance, newest_instance, resolve_workflow,
};
#[cfg(all(feature = "client", any(test, feature = "test-utilities")))]
pub use resolve::{GraphRead, MemoryGraph};

/// The tag naming the workflow an instance belongs to
pub const WORKFLOW_ID_TAG: &str = "WorkflowId";

/// The tag naming each agent in an instance
pub const INSTANCE_AGENT_TAG: &str = "InstanceAgent";

/// The tag naming each tool an instance's agents follow at latest through their own edges
pub const FOLLOWS_TOOL_TAG: &str = "InstanceFollowsTool";

/// The tag naming each toolset an instance's agents follow at latest
pub const FOLLOWS_TOOL_SET_TAG: &str = "InstanceFollowsToolSet";

/// The delimiter between a workflow instance's name and its id in a workflow run's result key
///
/// Workflow names can hold any character, so the delimiter alone can't split a key. A key always
/// ends with the delimiter and a hyphenated id, which is what [`WorkflowResultKey::parse`] reads.
pub const RESULT_KEY_DELIMITER: char = '~';

/// The length of a hyphenated id as it ends a workflow run's result key
const RESULT_KEY_ID_LEN: usize = 36;

/// The key a workflow run's results are stored under
///
/// It is the workflow instance's name, the [`RESULT_KEY_DELIMITER`], and the instance's id, so
/// two instances with the same name and revision never share a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowResultKey<'a> {
    /// The name the workflow instance had when the run started
    pub name: &'a str,
    /// The id of the workflow instance the run ran
    pub instance: Uuid,
}

impl<'a> WorkflowResultKey<'a> {
    /// Build the result key for a run of a workflow instance
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the workflow instance
    /// * `instance` - The id of the workflow instance
    #[must_use]
    pub fn build(name: &str, instance: Uuid) -> String {
        format!("{name}{RESULT_KEY_DELIMITER}{instance}")
    }

    /// Split a result key into the instance's name and id
    ///
    /// A key that doesn't end with the delimiter and a hyphenated id, such as the key of a tool
    /// run, isn't a workflow run's key and gives `None`.
    ///
    /// # Arguments
    ///
    /// * `key` - The result key to split
    #[must_use]
    pub fn parse(key: &'a str) -> Option<Self> {
        // the id is always the last characters of the key
        let split = key.len().checked_sub(RESULT_KEY_ID_LEN)?;
        let (head, id) = (key.get(..split)?, key.get(split..)?);
        // the delimiter and a non-empty name come before the id
        let name = head.strip_suffix(RESULT_KEY_DELIMITER)?;
        if name.is_empty() {
            return None;
        }
        // the id must be a valid id
        let instance = Uuid::parse_str(id).ok()?;
        Some(WorkflowResultKey { name, instance })
    }

    /// Get the name to show for a result key
    ///
    /// A workflow run's key shows as the instance's name and any other key shows as it is.
    ///
    /// # Arguments
    ///
    /// * `key` - The result key to show
    #[must_use]
    pub fn display_name(key: &'a str) -> &'a str {
        // a workflow run's key hides the id it carries
        Self::parse(key).map_or(key, |parsed| parsed.name)
    }
}

/// The default for the most agents one workflow may reach
pub const DEFAULT_MAX_AGENTS_PER_WORKFLOW: usize = 32;

/// The default for the most effective tools one agent of a workflow may be given
pub const DEFAULT_MAX_TOOLS_PER_AGENT: usize = 128;

/// The default for the most `DelegatesTo` hops between a workflow's agents and any agent it reaches
pub const DEFAULT_MAX_CRAWL_DEPTH: u8 = 8;

/// The limits on how much of the graph one workflow may reach
///
/// The resolver refuses (as an `Invalid` instance) a workflow past any of them and stops reading
/// the graph there, and the API refuses a submitted instance larger than a resolved one can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlLimits {
    /// The most agents one workflow may reach
    pub max_agents_per_workflow: usize,
    /// The most effective tools one agent may be given
    pub max_tools_per_agent: usize,
    /// The most `DelegatesTo` hops between a workflow's agents and any agent it reaches
    pub max_crawl_depth: u8,
}

impl Default for CrawlLimits {
    /// Build the default crawl limits
    fn default() -> Self {
        CrawlLimits {
            max_agents_per_workflow: DEFAULT_MAX_AGENTS_PER_WORKFLOW,
            max_tools_per_agent: DEFAULT_MAX_TOOLS_PER_AGENT,
            max_crawl_depth: DEFAULT_MAX_CRAWL_DEPTH,
        }
    }
}

impl From<&crate::conf::Events> for CrawlLimits {
    /// Get the crawl limits an events config sets
    ///
    /// # Arguments
    ///
    /// * `events` - The events config
    fn from(events: &crate::conf::Events) -> Self {
        CrawlLimits {
            max_agents_per_workflow: events.max_agents_per_workflow,
            max_tools_per_agent: events.max_tools_per_agent,
            max_crawl_depth: events.max_crawl_depth,
        }
    }
}

impl CrawlLimits {
    /// Get the most tools, and the most toolset instances, one agent of an instance resolved
    /// under these limits can list
    ///
    /// An agent's tools are only read while its tool and toolset edges fit twice the tool
    /// limit, since a latest edge and a pin can name the same instance, and an agent over the
    /// tool limit still lists what it was given in its invalid instance.
    #[must_use]
    pub fn max_listed_tools_per_agent(&self) -> usize {
        // twice the effective tool limit
        self.max_tools_per_agent.saturating_mul(2)
    }

    /// Get the most tools one instance resolved under these limits can list
    ///
    /// Every agent lists at most its listed tools and its harness, and each of those may pin
    /// one dependency.
    #[must_use]
    pub fn max_instance_tools(&self) -> usize {
        // each agent's tools and harness, each with at most one dependency
        self.max_agents_per_workflow
            .saturating_mul(self.max_listed_tools_per_agent().saturating_add(1))
            .saturating_mul(2)
    }
}

/// The default for settings that are on unless turned off
const fn default_true() -> bool {
    true
}

/// The editable root definition of an agentic workflow
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkflowCatalogEntity {
    /// The agent that receives the run's inputs first (must be one of this workflow's agents)
    pub entry_agent: Option<Uuid>,
    /// Whether the event handler should create new instances automatically on changes
    #[serde(default = "default_true")]
    pub auto_instance: bool,
    /// Run-wide limits a harness applies to every run of this workflow (part of the digest)
    #[serde(default)]
    pub limits: WorkflowLimits,
    /// The models, highest priority first, for agents whose own `models` list is empty (part of
    /// the digest)
    #[serde(default)]
    pub default_models: Vec<ModelMatcher>,
    /// Which levels may supply model auth for this workflow's runs (read when a run is created;
    /// not part of the digest and never snapshotted)
    #[serde(default)]
    pub model_auth: ModelAuthSource,
    /// The agents directly in this workflow (populated from associations)
    #[serde(default)]
    #[cfg_attr(feature = "api", schema(no_recursion))]
    pub agents: Vec<Entity>,
    /// When this workflow was last modified (any edit, including its agent edges; not part of
    /// the digest)
    pub modified: DateTime<Utc>,
    /// The newest ready instance of this workflow (kept on the workflow as its instances are
    /// created and deleted)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_instance: Option<Uuid>,
    /// The newest instance of this workflow, whatever its status, which the next instance
    /// supersedes (kept on the workflow as its instances are created and deleted)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_instance: Option<Uuid>,
    /// The highest revision any instance of this workflow was created at, kept so a deleted
    /// instance's revision is never given out again (0 before the first instance)
    #[serde(default)]
    pub highest_revision: u64,
}

impl WorkflowCatalogEntity {
    /// Clear the fields that are filled in when the workflow is read
    pub fn clear_populated(&mut self) {
        // drop the agents, which come from the workflow's edges
        self.agents.clear();
    }
}

/// Run-wide limits a harness applies to every run of a workflow
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub struct WorkflowLimits {
    /// The max number of sub-agent invocations running at once across the whole run, counting
    /// single delegations and swarm members together; extra invocations queue (unset = harness
    /// default)
    pub max_alive_sub_agents: Option<u32>,
}

/// A constant, fully resolved snapshot of a workflow catalog's graph
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkflowInstanceEntity {
    /// The ID of the workflow catalog this is an instance of
    pub workflow: Uuid,
    /// The groups the workflow catalog was in at instancing time
    pub workflow_groups: Vec<String>,
    /// The monotonically increasing revision of this instance within its workflow
    pub revision: u64,
    /// The ID of the instance this instance supersedes
    pub previous: Option<Uuid>,
    /// Whether this instance is valid and can be run
    pub status: WorkflowInstanceStatus,
    /// Non fatal issues found while instancing (e.g. pins to deprecated tool instances)
    #[serde(default)]
    pub warnings: Vec<String>,
    /// What caused this instance to be created
    pub trigger: WorkflowInstanceTrigger,
    /// The user whose change or request caused this instance (not in the digest)
    #[serde(default)]
    pub triggered_by: String,
    /// The agent that receives the run's inputs first
    pub entry_agent: Option<Uuid>,
    /// The workflow's run-wide limits at instancing time
    #[serde(default)]
    pub limits: WorkflowLimits,
    /// The workflow's default models at instancing time
    #[serde(default)]
    pub default_models: Vec<ModelMatcher>,
    /// Every agent reachable in this workflow and the exact tools it was given
    pub agents: Vec<InstancedAgent>,
    /// Every unique tool instance referenced anywhere in this workflow
    pub tools: Vec<InstancedTool>,
    /// The content digest of this workflow instance
    pub digest: String,
}

impl WorkflowInstanceEntity {
    /// Sort the agents by id, the tools by tool and tool instance, and each agent's lists by id
    ///
    /// Equal instances then serialize identically whatever order their graph was read in.
    pub fn sort_lists(&mut self) {
        // agents order by their id and tools by their derived order
        self.agents.sort_by_key(|agent| agent.agent);
        self.tools.sort();
        // each agent's own lists order by id
        for agent in &mut self.agents {
            agent.tools.sort();
            agent.tool_sets.sort();
            agent.bindings.sort();
            agent.sub_agents.sort();
        }
    }

    /// Whether this instance resolved cleanly and can be run
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == WorkflowInstanceStatus::Ready
    }

    /// Build the name a workflow instance gets: `{workflow name}#r{revision}`
    ///
    /// # Arguments
    ///
    /// * `workflow` - The name of the workflow
    /// * `revision` - The instance's revision
    #[must_use]
    pub fn name_for(workflow: &str, revision: u64) -> String {
        format!("{workflow}#r{revision}")
    }

    /// Get one of this instance's agents by its id
    ///
    /// # Arguments
    ///
    /// * `agent` - The id of the agent
    #[must_use]
    pub fn agent(&self, agent: Uuid) -> Option<&InstancedAgent> {
        self.agents
            .iter()
            .find(|instanced| instanced.agent == agent)
    }

    /// Get one of this instance's tools by its tool instance id
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The id of the tool instance
    #[must_use]
    pub fn tool(&self, tool_instance: Uuid) -> Option<&InstancedTool> {
        self.tools
            .iter()
            .find(|tool| tool.tool_instance == tool_instance)
    }

    /// Compute this instance's content digest
    ///
    /// The digest covers each agent's config hash, harness, tools, toolset instances, and tool
    /// bindings (as a multiset), the delegations between agents, the entry agent's config hash, the
    /// dependencies, the limits, and the default models; ids, names, groups, and the
    /// revision, trigger, and status are left out.
    ///
    /// # Arguments
    ///
    /// * `tool_set_digests` - The digest of each toolset instance the agents were given
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn compute_digest(
        &self,
        tool_set_digests: &std::collections::BTreeMap<Uuid, String>,
    ) -> Result<String, serde_json::Error> {
        // digest under the current scheme
        self.compute_digest_with_scheme(super::digests::ThorDigest::SCHEME, tool_set_digests)
    }

    /// Compute this instance's content digest under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to use
    /// * `tool_set_digests` - The digest of each toolset instance the agents were given
    #[cfg(any(feature = "api", feature = "client"))]
    pub(crate) fn compute_digest_with_scheme(
        &self,
        scheme: &str,
        tool_set_digests: &std::collections::BTreeMap<Uuid, String>,
    ) -> Result<String, serde_json::Error> {
        use super::digests::{ThorDigest, WorkflowAgentNode};
        use serde::ser::Error as _;
        use std::collections::HashMap;
        // index the spec hash of every tool and the config hash of every agent by id
        let hashes = self
            .tools
            .iter()
            .map(|tool| (tool.tool_instance, tool.spec_hash.as_str()))
            .collect::<HashMap<Uuid, &str>>();
        let configs = self
            .agents
            .iter()
            .map(|agent| (agent.agent, agent.config_hash.as_str()))
            .collect::<HashMap<Uuid, &str>>();
        // look up the spec hash of each tool instance this instance names
        let spec_hash = |id: &Uuid| {
            hashes.get(id).copied().ok_or_else(|| {
                serde_json::Error::custom(format!("tool instance {id} isn't in tools"))
            })
        };
        // describe every agent by its content
        let mut nodes = Vec::with_capacity(self.agents.len());
        for agent in &self.agents {
            let harness = agent.harness.as_ref().map(spec_hash).transpose()?;
            let tools = agent
                .tools
                .iter()
                .map(spec_hash)
                .collect::<Result<Vec<&str>, serde_json::Error>>()?;
            let tool_sets = agent
                .tool_sets
                .iter()
                .map(|id| {
                    tool_set_digests.get(id).map(String::as_str).ok_or_else(|| {
                        serde_json::Error::custom(format!("toolset instance {id} has no digest"))
                    })
                })
                .collect::<Result<Vec<&str>, serde_json::Error>>()?;
            let bindings = agent
                .bindings
                .iter()
                .map(|binding| Ok((spec_hash(&binding.tool_instance)?, binding.mechanism)))
                .collect::<Result<Vec<_>, serde_json::Error>>()?;
            nodes.push(WorkflowAgentNode {
                config_hash: &agent.config_hash,
                harness,
                tools,
                tool_sets,
                bindings,
            });
        }
        // describe each delegation by the config hashes of its two agents
        let mut delegations = Vec::new();
        for agent in &self.agents {
            for sub in &agent.sub_agents {
                let Some(target) = configs.get(sub) else {
                    return Err(serde_json::Error::custom(format!(
                        "sub-agent {sub} isn't in agents"
                    )));
                };
                delegations.push((agent.config_hash.as_str(), *target));
            }
        }
        // the entry agent is described by its config hash, when it's in the instance
        let entry = self.entry_agent.and_then(|id| configs.get(&id).copied());
        // the dependencies are described by their spec hashes
        let dependencies = self
            .tools
            .iter()
            .filter(|tool| tool.dependency)
            .map(|tool| tool.spec_hash.as_str())
            .collect::<Vec<&str>>();
        let input = super::digests::WorkflowDigestInput {
            agents: &nodes,
            delegations: &delegations,
            entry_agent: entry,
            dependencies: &dependencies,
            limits: &self.limits,
            default_models: &self.default_models,
        };
        ThorDigest::workflow_instance_with_scheme(scheme, &input)
    }
}

/// An agent as it was resolved into a workflow instance
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct InstancedAgent {
    /// The ID of the agent entity
    pub agent: Uuid,
    /// The name of the agent at instancing time
    pub name: String,
    /// The groups the agent was in at instancing time
    pub groups: Vec<String>,
    /// A snapshot of the agent's configuration at instancing time
    pub config: AgentConfig,
    /// The hash of the agent's configuration at instancing time
    pub config_hash: String,
    /// The ID of the harness tool instance this agent runs in
    pub harness: Option<Uuid>,
    /// The IDs of the tool instances this agent was given (its own tools plus its toolsets'
    /// members)
    pub tools: Vec<Uuid>,
    /// The IDs of the toolset instances whose members this agent was given
    #[serde(default)]
    pub tool_sets: Vec<Uuid>,
    /// How the harness carries out calls to each of this agent's tools (part of the digest)
    #[serde(default)]
    pub bindings: Vec<InstancedToolBinding>,
    /// The IDs of the agents this agent may delegate to
    pub sub_agents: Vec<Uuid>,
    /// The digest of this agent's effective toolset
    pub toolset_digest: String,
}

/// The mechanism a harness uses to carry out calls to one of an agent's tools
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct InstancedToolBinding {
    /// The ID of the tool instance this binding is for
    pub tool_instance: Uuid,
    /// The mechanism picked for this tool
    pub mechanism: ToolMechanism,
}

impl InstancedAgent {
    /// Compute the digest of this agent's effective toolset from its tools' spec hashes
    ///
    /// # Arguments
    ///
    /// * `tools` - The tools of the instance this agent is in
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn compute_toolset_digest(
        &self,
        tools: &[InstancedTool],
    ) -> Result<String, serde_json::Error> {
        use serde::ser::Error as _;
        use std::collections::HashMap;
        // index the spec hash of every tool by id
        let index = tools
            .iter()
            .map(|tool| (tool.tool_instance, tool.spec_hash.as_str()))
            .collect::<HashMap<Uuid, &str>>();
        // look up the spec hash of each of this agent's tools
        let hashes = self
            .tools
            .iter()
            .map(|id| {
                index.get(id).copied().ok_or_else(|| {
                    serde_json::Error::custom(format!("tool instance {id} isn't in tools"))
                })
            })
            .collect::<Result<Vec<&str>, serde_json::Error>>()?;
        super::digests::ThorDigest::agent_toolset(&hashes)
    }
}

/// A tool instance as it was resolved into a workflow instance
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct InstancedTool {
    /// The ID of the tool (root) entity
    pub tool: Uuid,
    /// The ID of the exact tool instance used
    pub tool_instance: Uuid,
    /// The name of the tool
    pub name: String,
    /// The groups the tool instance was in at instancing time
    pub groups: Vec<String>,
    /// The kind of tool this is
    pub kind: ToolKind,
    /// The version label of the tool instance
    pub version: String,
    /// The content hash of the tool instance
    pub spec_hash: String,
    /// The function name a model calls this tool by (agentic tools only)
    pub function_name: Option<String>,
    /// Whether this tool was pinned (true) or resolved from latest (false)
    pub pinned: bool,
    /// Whether this tool is only a dependency of another tool (e.g. the Image tool behind a
    /// `Container` backing)
    #[serde(default)]
    pub dependency: bool,
}

/// Whether a workflow instance can be run
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum WorkflowInstanceStatus {
    /// This instance resolved cleanly and can be run
    Ready,
    /// This instance failed validation and cannot be run
    Invalid {
        /// The reasons this instance is invalid
        errors: Vec<String>,
    },
}

impl WorkflowInstanceStatus {
    /// Get the name of this status, as its intrinsic tag records it
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkflowInstanceStatus::Ready => "Ready",
            WorkflowInstanceStatus::Invalid { .. } => "Invalid",
        }
    }
}

/// What caused a workflow instance to be created
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum WorkflowInstanceTrigger {
    /// The workflow catalog or one of its Orchestrates edges changed
    WorkflowUpdated,
    /// A tool followed at latest by an agent in this workflow got a new instance
    ToolUpdated {
        /// The tool that was updated
        tool: Uuid,
        /// The new instance of that tool
        tool_instance: Uuid,
    },
    /// An agent in this workflow had its configuration or edges changed
    AgentUpdated {
        /// The agent that was updated
        agent: Uuid,
    },
    /// A toolset followed at latest by an agent in this workflow got a new ready instance
    ToolSetUpdated {
        /// The toolset that was updated
        tool_set: Uuid,
        /// The new ready instance of that toolset
        tool_set_instance: Uuid,
    },
    /// A user explicitly requested a new instance
    Manual,
}

/// One agent of a workflow instance and the stored spec of each tool it was given, as a
/// harness reads them
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct InstancedAgentToolset {
    /// The agent as it was resolved into the instance
    pub agent: InstancedAgent,
    /// Each of the agent's tools with its stored spec, in the order the agent lists them
    pub tools: Vec<InstancedToolSpec>,
}

/// A tool of a workflow instance with the spec its tool instance stores
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct InstancedToolSpec {
    /// The tool as it was resolved into the instance
    pub tool: InstancedTool,
    /// The spec the tool instance stores
    pub spec: ToolSpec,
}

/// A request to create a workflow catalog entity and its agent edges
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkflowCatalogEntityRequest {
    /// The agents directly in this workflow
    #[serde(default)]
    pub agents: Vec<Uuid>,
    /// The agent that receives the run's inputs first
    pub entry_agent: Option<Uuid>,
    /// Whether the event handler should create new instances automatically on changes
    pub auto_instance: Option<bool>,
    /// Run-wide limits for every run of this workflow
    pub limits: Option<WorkflowLimits>,
    /// The models for agents whose own `models` list is empty, highest priority first
    #[serde(default)]
    pub default_models: Vec<ModelMatcher>,
    /// Which levels may supply model auth for this workflow's runs (unset = every level)
    pub model_auth: Option<ModelAuthSource>,
    /// Whether to add the workflow's groups that an agent is missing to that agent instead of
    /// rejecting the edge
    #[serde(default)]
    pub extend_target_groups: bool,
}

impl WorkflowCatalogEntityRequest {
    /// Add an agent to the workflow
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent to add
    #[must_use]
    pub fn agent(mut self, agent: Uuid) -> Self {
        self.agents.push(agent);
        self
    }

    /// Set the agent that receives the run's inputs first
    ///
    /// # Arguments
    ///
    /// * `agent` - The entry agent, which must be one of the workflow's agents
    #[must_use]
    pub fn entry_agent(mut self, agent: Uuid) -> Self {
        self.entry_agent = Some(agent);
        self
    }

    /// Set whether new instances are built automatically
    ///
    /// # Arguments
    ///
    /// * `auto_instance` - Whether new instances are built automatically
    #[must_use]
    pub fn auto_instance(mut self, auto_instance: bool) -> Self {
        self.auto_instance = Some(auto_instance);
        self
    }

    /// Set the run-wide limits
    ///
    /// # Arguments
    ///
    /// * `limits` - The run-wide limits
    #[must_use]
    pub fn limits(mut self, limits: WorkflowLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Add a default model, after the ones already added
    ///
    /// # Arguments
    ///
    /// * `model` - The model to accept
    #[must_use]
    pub fn default_model(mut self, model: ModelMatcher) -> Self {
        self.default_models.push(model);
        self
    }

    /// Set which levels may supply model auth
    ///
    /// # Arguments
    ///
    /// * `model_auth` - The levels that may supply model auth
    #[must_use]
    pub fn model_auth(mut self, model_auth: ModelAuthSource) -> Self {
        self.model_auth = Some(model_auth);
        self
    }

    /// Add the workflow's missing groups to its agents instead of rejecting the edges
    #[must_use]
    pub fn extend_target_groups(mut self) -> Self {
        self.extend_target_groups = true;
        self
    }

    /// Get the form fields this request sends, by name
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the kind, then each agent
        let mut fields = vec![(
            "kind",
            super::EntityKinds::WorkflowCatalog.as_str().to_owned(),
        )];
        fields.extend(
            self.agents
                .iter()
                .map(|id| ("metadata[workflow_agents][]", id.to_string())),
        );
        // the settings that are set
        if let Some(entry) = &self.entry_agent {
            fields.push(("metadata[workflow_entry_agent]", entry.to_string()));
        }
        if let Some(auto_instance) = self.auto_instance {
            fields.push((
                "metadata[workflow_auto_instance]",
                auto_instance.to_string(),
            ));
        }
        // the structured settings travel as JSON
        if let Some(limits) = &self.limits {
            fields.push(("metadata[workflow_limits]", serde_json::to_string(limits)?));
        }
        if !self.default_models.is_empty() {
            fields.push((
                "metadata[workflow_default_models]",
                serde_json::to_string(&self.default_models)?,
            ));
        }
        if let Some(model_auth) = &self.model_auth {
            fields.push((
                "metadata[workflow_model_auth]",
                serde_json::to_string(model_auth)?,
            ));
        }
        // widen agents instead of rejecting edges only when asked
        if self.extend_target_groups {
            fields.push(("metadata[extend_target_groups]", "true".to_owned()));
        }
        Ok(fields)
    }

    /// Add this workflow request's metadata to a form
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

/// A request to create a workflow instance (from the event handler or, rarely, a user)
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkflowInstanceEntityRequest {
    /// The JSON encoded `WorkflowInstanceEntity` to validate and store
    pub instance: String,
}

impl WorkflowInstanceEntityRequest {
    /// Build a request to submit a workflow instance
    ///
    /// # Arguments
    ///
    /// * `instance` - The workflow instance to submit
    pub fn new(instance: &WorkflowInstanceEntity) -> Result<Self, serde_json::Error> {
        // encode the instance the way the form carries it
        Ok(WorkflowInstanceEntityRequest {
            instance: serde_json::to_string(instance)?,
        })
    }

    /// Get the form fields this request sends, by name
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind, then the instance as one JSON field
        vec![
            (
                "kind",
                super::EntityKinds::WorkflowInstance.as_str().to_owned(),
            ),
            ("metadata[workflow_instance]", self.instance.clone()),
        ]
    }

    /// Add this workflow instance request's metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    #[must_use]
    pub fn add_to_form(self, form: reqwest::multipart::Form) -> reqwest::multipart::Form {
        // add each of our fields to the form
        self.form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value))
    }
}

/// The fields of a workflow catalog update, used to build its form fields
#[derive(Debug, Clone, Copy)]
pub struct WorkflowCatalogUpdateFields<'a> {
    /// The new entry agent
    pub entry_agent: Option<&'a Uuid>,
    /// Whether to clear the entry agent
    pub clear_entry_agent: Option<&'a bool>,
    /// Whether new instances are built automatically
    pub auto_instance: Option<&'a bool>,
    /// The new run-wide limits
    pub limits: Option<&'a WorkflowLimits>,
    /// The new default models, replacing the whole list
    pub default_models: Option<&'a [ModelMatcher]>,
    /// The levels that may supply model auth
    pub model_auth: Option<&'a ModelAuthSource>,
    /// The agents to add
    pub add_agents: &'a [Uuid],
    /// The agents to remove
    pub remove_agents: &'a [Uuid],
    /// Whether to widen the agents of added edges instead of rejecting them
    pub extend_target_groups: bool,
}

impl WorkflowCatalogUpdateFields<'_> {
    /// Get the form fields this update sends, by name
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the single-valued fields that are set
        let mut fields = Vec::new();
        if let Some(entry) = self.entry_agent {
            fields.push(("metadata[workflow_entry_agent]", entry.to_string()));
        }
        if let Some(clear) = self.clear_entry_agent {
            fields.push(("metadata[clear_workflow_entry_agent]", clear.to_string()));
        }
        if let Some(auto_instance) = self.auto_instance {
            fields.push((
                "metadata[workflow_auto_instance]",
                auto_instance.to_string(),
            ));
        }
        // the structured fields travel as JSON, and an empty model list clears the list
        if let Some(limits) = self.limits {
            fields.push(("metadata[workflow_limits]", serde_json::to_string(limits)?));
        }
        if let Some(models) = self.default_models {
            fields.push((
                "metadata[workflow_default_models]",
                serde_json::to_string(models)?,
            ));
        }
        if let Some(model_auth) = self.model_auth {
            fields.push((
                "metadata[workflow_model_auth]",
                serde_json::to_string(model_auth)?,
            ));
        }
        // each agent to add or remove
        fields.extend(
            self.add_agents
                .iter()
                .map(|id| ("metadata[add_workflow_agents][]", id.to_string())),
        );
        fields.extend(
            self.remove_agents
                .iter()
                .map(|id| ("metadata[remove_workflow_agents][]", id.to_string())),
        );
        // widen agents instead of rejecting edges only when asked
        if self.extend_target_groups {
            fields.push(("metadata[extend_target_groups]", "true".to_owned()));
        }
        Ok(fields)
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use crate::models::EntityMetadataForm;
        use crate::models::entities::models::parse_json_field;
        use crate::utils::ApiError;
        use crate::bad;

        impl WorkflowCatalogEntityRequest {
            /// Take the workflow catalog fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // parse the structured fields that are set
                let limits = match form.workflow_limits.take() {
                    Some(raw) => Some(parse_json_field("workflow_limits", &raw)?),
                    None => None,
                };
                let default_models = match form.workflow_default_models.take() {
                    Some(raw) => parse_json_field("workflow_default_models", &raw)?,
                    None => Vec::new(),
                };
                let model_auth = match form.workflow_model_auth.take() {
                    Some(raw) => Some(parse_json_field("workflow_model_auth", &raw)?),
                    None => None,
                };
                Ok(WorkflowCatalogEntityRequest {
                    agents: std::mem::take(&mut form.workflow_agents),
                    entry_agent: form.workflow_entry_agent.take(),
                    auto_instance: form.workflow_auto_instance.take(),
                    limits,
                    default_models,
                    model_auth,
                    extend_target_groups: form.extend_target_groups == Some(true),
                })
            }
        }

        impl WorkflowInstanceEntityRequest {
            /// Parse the submitted workflow instance of an entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn parse_form(form: &mut EntityMetadataForm) -> Result<WorkflowInstanceEntity, ApiError> {
                // the instance is required
                let Some(raw) = form.workflow_instance.take() else {
                    return bad!("a workflow instance needs a workflow_instance".to_owned());
                };
                // it must be a whole workflow instance
                match serde_json::from_str(&raw) {
                    Ok(instance) => Ok(instance),
                    Err(err) => bad!(format!("workflow_instance is not a valid workflow instance: {err}")),
                }
            }
        }
    }
}
