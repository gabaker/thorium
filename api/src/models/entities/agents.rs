//! Contains models for AI agents
//!
//! An [`AgentEntity`] is a configured AI agent. Its [`AgentConfig`] holds the prompts, accepted
//! models, and limits that workflow instances snapshot, and is hashed into the agent's
//! `config_hash`. Its tools, toolsets, sub-agents, and harness are graph edges that are filled in
//! when the agent is read.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use crate::models::{Entity, ModelAuthSource, ModelMatcher, ModelRequirements};

#[cfg(test)]
mod tests;

/// A configured AI agent that can be placed into workflows
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AgentEntity {
    /// The configuration for this agent
    pub config: AgentConfig,
    /// The content hash of this agent's configuration (`config`)
    pub config_hash: String,
    /// Which levels may supply model auth for this agent (read when a run is created; not part of
    /// `config_hash` and never snapshotted)
    #[serde(default)]
    pub model_auth: ModelAuthSource,
    /// The harness this agent runs in (Tool = latest, `ToolInstance` = pinned; populated from
    /// associations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Box<Entity>>,
    /// The tools this agent follows at their latest instance (populated from associations)
    #[serde(default)]
    pub latest_tools: Vec<Entity>,
    /// The tool instances this agent is pinned to (populated from associations)
    #[serde(default)]
    pub pinned_tools: Vec<Entity>,
    /// The toolsets this agent uses (`ToolSet` = latest, `ToolSetInstance` = pinned; populated from
    /// associations)
    #[serde(default)]
    pub tool_sets: Vec<Entity>,
    /// The agents this agent can delegate to (populated from associations)
    #[serde(default)]
    pub sub_agents: Vec<Entity>,
    /// When this agent was last modified (any edit, including its edges; not part of
    /// `config_hash`)
    pub modified: DateTime<Utc>,
}

impl AgentEntity {
    /// Clear the fields that are filled in from associations when the agent is read
    pub fn clear_edges(&mut self) {
        // drop every populated edge target
        self.harness = None;
        self.latest_tools.clear();
        self.pinned_tools.clear();
        self.tool_sets.clear();
        self.sub_agents.clear();
    }
}

/// The configuration of an agent that is snapshotted into workflow instances
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub struct AgentConfig {
    /// The system prompt given to the model when this agent is spawned
    pub system_prompt: Option<String>,
    /// The statically defined task prompt; a run's inputs (files and repos) are added to it by
    /// the harness
    pub prompt: Option<String>,
    /// The models this agent accepts, highest priority first (empty = the workflow's
    /// `default_models`, or any model the auth source reaches when that is empty too)
    #[serde(default)]
    pub models: Vec<ModelMatcher>,
    /// What every model this agent runs on must provide
    #[serde(default)]
    pub model_requirements: ModelRequirements,
    /// The max number of tools mapped into a single agent session
    pub max_tools: Option<u32>,
    /// The max number of model turns per invocation (for the entry agent, the whole run; for a
    /// sub-agent, each delegation or swarm member)
    pub max_turns: Option<u32>,
    /// The max copies of this agent a single swarm (a concurrent fan-out of tasks) may include;
    /// 0 opts this agent out of swarms (unset = harness default)
    pub max_swarm_size: Option<u32>,
    /// Tags a harness may use to narrow this agent's tools per run to limit model context
    #[serde(default)]
    pub tool_tags: BTreeMap<String, BTreeSet<String>>,
    /// The function name another agent's model calls to delegate to this agent
    /// (must match `^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`; required when this agent is a sub-agent)
    pub function_name: Option<String>,
    /// The description another agent's model sees when it can delegate to this agent
    /// (required when this agent is a sub-agent)
    pub model_description: Option<String>,
    /// Whether this agent's model must call a tool on the first call of each invocation, and
    /// which
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Whether this agent's model may call several tools in one turn (unset = the model API's
    /// default)
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
}

impl AgentConfig {
    /// Check the parts of this config a request can get wrong
    ///
    /// Returns a message naming the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        // a delegation function name follows the agentic function name rule
        if let Some(name) = &self.function_name
            && !super::tools::is_valid_function_name(name)
        {
            return Err(format!(
                "agent_config.function_name `{name}` must match ^[a-zA-Z_][a-zA-Z0-9_-]{{0,63}}$"
            ));
        }
        // a forced function names a tool or delegation tool the same way
        if let ToolChoice::Function(name) = &self.tool_choice
            && !super::tools::is_valid_function_name(name)
        {
            return Err(format!(
                "agent_config.tool_choice function `{name}` must match ^[a-zA-Z_][a-zA-Z0-9_-]{{0,63}}$"
            ));
        }
        // every accepted model names an exact model id
        if let Some(index) = self
            .models
            .iter()
            .position(|matcher| matcher.model_id.trim().is_empty())
        {
            return Err(format!("agent_config.models[{index}] must name a model_id"));
        }
        Ok(())
    }

    /// Compute this config's content hash, the agent's `config_hash`
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn config_hash(&self) -> Result<String, serde_json::Error> {
        // hash the typed config under the agent config tag
        super::digests::ThorDigest::compute(super::digests::DigestTag::AgentConfig, self)
    }
}

/// Whether an agent's model must call a tool, and which
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub enum ToolChoice {
    /// The model decides whether to call a tool
    #[default]
    Auto,
    /// The model must call some tool
    Required,
    /// The model must not call any tool
    None,
    /// The model must call the tool or delegation tool with this function name
    Function(String),
}

/// A request to create an agent entity and its graph edges
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AgentEntityRequest {
    /// The configuration for this agent
    pub config: AgentConfig,
    /// Which levels may supply model auth for this agent (unset = every level)
    pub model_auth: Option<ModelAuthSource>,
    /// The harness this agent runs in (Tool ID = latest, `ToolInstance` ID = pinned)
    pub harness: Option<Uuid>,
    /// The tools this agent uses (Tool IDs = latest, `ToolInstance` IDs = pinned)
    #[serde(default)]
    pub tools: Vec<Uuid>,
    /// The toolsets this agent uses (`ToolSet` IDs = latest, `ToolSetInstance` IDs = pinned)
    #[serde(default)]
    pub tool_sets: Vec<Uuid>,
    /// The agents this agent can delegate to
    #[serde(default)]
    pub sub_agents: Vec<Uuid>,
    /// Whether to add the agent's groups that an edge target is missing to that target instead
    /// of rejecting the edge
    #[serde(default)]
    pub extend_target_groups: bool,
}

impl AgentEntityRequest {
    /// Build a request to create an agent with a config
    ///
    /// # Arguments
    ///
    /// * `config` - The agent's configuration
    #[must_use]
    pub fn new(config: AgentConfig) -> Self {
        AgentEntityRequest {
            config,
            ..AgentEntityRequest::default()
        }
    }

    /// Add a tool (latest) or tool instance (pinned) for this agent to call
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool or tool instance to call
    #[must_use]
    pub fn tool(mut self, tool: Uuid) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add a toolset (latest) or toolset instance (pinned) for this agent to call
    ///
    /// # Arguments
    ///
    /// * `tool_set` - The toolset or toolset instance to call
    #[must_use]
    pub fn tool_set(mut self, tool_set: Uuid) -> Self {
        self.tool_sets.push(tool_set);
        self
    }

    /// Add an agent this agent can delegate to
    ///
    /// # Arguments
    ///
    /// * `sub_agent` - The agent to delegate to
    #[must_use]
    pub fn sub_agent(mut self, sub_agent: Uuid) -> Self {
        self.sub_agents.push(sub_agent);
        self
    }

    /// Set the harness this agent runs in
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness tool (latest) or tool instance (pinned)
    #[must_use]
    pub fn harness(mut self, harness: Uuid) -> Self {
        self.harness = Some(harness);
        self
    }

    /// Set which levels may supply model auth for this agent
    ///
    /// # Arguments
    ///
    /// * `model_auth` - The levels that may supply model auth
    #[must_use]
    pub fn model_auth(mut self, model_auth: ModelAuthSource) -> Self {
        self.model_auth = Some(model_auth);
        self
    }

    /// Add the agent's missing groups to edge targets instead of rejecting the edges
    #[must_use]
    pub fn extend_target_groups(mut self) -> Self {
        self.extend_target_groups = true;
        self
    }

    /// Get the form fields this request sends, by name
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the kind and the whole config as one JSON field
        let mut fields = vec![
            ("kind", super::EntityKinds::Agent.as_str().to_owned()),
            (
                "metadata[agent_config]",
                serde_json::to_string(&self.config)?,
            ),
        ];
        // the model auth levels travel in their own field
        if let Some(model_auth) = &self.model_auth {
            fields.push((
                "metadata[agent_model_auth]",
                serde_json::to_string(model_auth)?,
            ));
        }
        // the harness edge
        if let Some(harness) = &self.harness {
            fields.push(("metadata[agent_harness]", harness.to_string()));
        }
        // each tool, toolset, and sub-agent edge
        fields.extend(
            self.tools
                .iter()
                .map(|id| ("metadata[agent_tools][]", id.to_string())),
        );
        fields.extend(
            self.tool_sets
                .iter()
                .map(|id| ("metadata[agent_tool_sets][]", id.to_string())),
        );
        fields.extend(
            self.sub_agents
                .iter()
                .map(|id| ("metadata[agent_sub_agents][]", id.to_string())),
        );
        // widen targets instead of rejecting edges only when asked
        if self.extend_target_groups {
            fields.push(("metadata[extend_target_groups]", "true".to_owned()));
        }
        Ok(fields)
    }

    /// Add this agent request's metadata to a form
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

/// The fields of an agent update, used to build its form fields
#[derive(Debug, Clone, Copy)]
pub struct AgentUpdateFields<'a> {
    /// The new configuration for the agent
    pub config: Option<&'a AgentConfig>,
    /// The levels that may supply model auth for the agent
    pub model_auth: Option<&'a ModelAuthSource>,
    /// The harness to set for the agent
    pub harness: Option<&'a Uuid>,
    /// Whether to clear the agent's harness
    pub clear_harness: Option<&'a bool>,
    /// The tools to add to the agent
    pub add_tools: &'a [Uuid],
    /// The tools to remove from the agent
    pub remove_tools: &'a [Uuid],
    /// The toolsets to add to the agent
    pub add_tool_sets: &'a [Uuid],
    /// The toolsets to remove from the agent
    pub remove_tool_sets: &'a [Uuid],
    /// The sub-agents to add to the agent
    pub add_sub_agents: &'a [Uuid],
    /// The sub-agents to remove from the agent
    pub remove_sub_agents: &'a [Uuid],
    /// Whether to widen the targets of added edges instead of rejecting them
    pub extend_target_groups: bool,
}

impl AgentUpdateFields<'_> {
    /// Get the form fields this update sends, by name
    pub fn form_fields(&self) -> Result<Vec<(&'static str, String)>, serde_json::Error> {
        // the single-valued fields that are set
        let mut fields = Vec::new();
        if let Some(config) = self.config {
            fields.push(("metadata[agent_config]", serde_json::to_string(config)?));
        }
        if let Some(model_auth) = self.model_auth {
            fields.push((
                "metadata[agent_model_auth]",
                serde_json::to_string(model_auth)?,
            ));
        }
        if let Some(harness) = self.harness {
            fields.push(("metadata[agent_harness]", harness.to_string()));
        }
        if let Some(clear) = self.clear_harness {
            fields.push(("metadata[clear_agent_harness]", clear.to_string()));
        }
        // each edge to add or remove
        let lists: [(&'static str, &[Uuid]); 6] = [
            ("metadata[add_agent_tools][]", self.add_tools),
            ("metadata[remove_agent_tools][]", self.remove_tools),
            ("metadata[add_agent_tool_sets][]", self.add_tool_sets),
            ("metadata[remove_agent_tool_sets][]", self.remove_tool_sets),
            ("metadata[add_agent_sub_agents][]", self.add_sub_agents),
            (
                "metadata[remove_agent_sub_agents][]",
                self.remove_sub_agents,
            ),
        ];
        for (name, ids) in lists {
            fields.extend(ids.iter().map(|id| (name, id.to_string())));
        }
        // widen targets instead of rejecting edges only when asked
        if self.extend_target_groups {
            fields.push(("metadata[extend_target_groups]", "true".to_owned()));
        }
        Ok(fields)
    }
}
