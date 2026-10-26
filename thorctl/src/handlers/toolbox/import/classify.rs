//! Comparing what a manifest says with what Thorium holds
//!
//! Tool versions are compared by the hash of their spec, which is the hash Thorium gives a tool
//! instance, so a version is unchanged exactly when some instance of the tool already has its spec.
//! Toolsets, agents, workflows, and pipelines are compared field by field after both sides are put
//! in the same normal form (the views `toolsets edit` and `agents edit` use for the first two).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::{
    Entity, EntityMetadata, HarnessEnv, HarnessEnvVar, ModelAuthLevel, Pipeline, PipelineRequest,
    ToolKind,
};

use super::resolve::Prepared;
use super::world::LiveTool;
use crate::args::agents::ModelAuthArg;
use crate::handlers::agents::edit::{AgentEditable, AgentView};
use crate::handlers::agents::refs::{ToolRef as ViewToolRef, ToolSetRef as ViewToolSetRef};
use crate::handlers::edit::Editable;
use crate::handlers::toolbox::v2::plan_types::{VersionClass, VersionPlan, label_with_hash};
use crate::handlers::toolbox::v2::types::{
    AgentManifest, ToolManifest, ToolRef, ToolSetManifest, WorkflowManifest,
};
use crate::handlers::toolsets::edit::{ToolSetEditable, ToolSetView};

/// Compare one version of a manifest tool with the versions of the tool in Thorium
///
/// # Arguments
///
/// * `label` - The version's label in the manifest
/// * `prepared` - The version's spec made ready, or what it is waiting for
/// * `existing` - The tool as it is in Thorium, if it exists
#[must_use]
pub fn classify_version(
    label: &str,
    prepared: &Prepared,
    existing: Option<&LiveTool>,
) -> VersionPlan {
    // the hash is known once every id the spec needs is
    let spec_hash = match prepared {
        Prepared::Ready(ready) => Some(ready.spec_hash.clone()),
        Prepared::Pending(_) => None,
    };
    // a tool that doesn't exist yet gets every version as part of its creation
    let Some(existing) = existing else {
        return VersionPlan {
            label: label.to_owned(),
            class: VersionClass::New,
            spec_hash,
            publish_label: None,
        };
    };
    // some instance with the spec's hash is this version, whatever it is labeled
    let same_hash = spec_hash
        .as_ref()
        .is_some_and(|hash| existing.instances.iter().any(|i| i.spec_hash == *hash));
    // otherwise the label may be taken by a different spec
    let label_taken = existing.instance(label).is_some();
    let class = if same_hash {
        VersionClass::Unchanged
    } else if label_taken {
        VersionClass::LabelConflict
    } else {
        VersionClass::NewVersion
    };
    // a conflicting version is published under its label and a prefix of its hash
    let publish_label = match (&class, &spec_hash) {
        (VersionClass::LabelConflict, Some(hash)) => Some(label_with_hash(label, hash)),
        _ => None,
    };
    VersionPlan {
        label: label.to_owned(),
        class,
        spec_hash,
        publish_label,
    }
}

/// What differs between a manifest tool and the tool in Thorium, apart from its versions
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootChange {
    /// The description to set
    pub description: Option<String>,
    /// The groups to add the tool to
    pub add_groups: Vec<String>,
    /// The tag values to add, keyed by tag key
    pub add_tags: BTreeMap<String, BTreeSet<String>>,
    /// The harness env to set (harness tools only)
    pub harness_env: Option<HarnessEnv>,
    /// The model ids to add to the disabled models (harness tools only)
    pub disable_models: BTreeSet<String>,
    /// The model ids to remove from the disabled models (harness tools only)
    pub enable_models: BTreeSet<String>,
}

impl RootChange {
    /// Whether nothing differs
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // every part is unset
        self.description.is_none()
            && self.add_groups.is_empty()
            && self.add_tags.is_empty()
            && self.harness_env.is_none()
            && self.disable_models.is_empty()
            && self.enable_models.is_empty()
    }

    /// Describe the differences in one line each
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        // each difference gets its own line
        if let Some(description) = &self.description {
            lines.push(format!("description -> {description:?}"));
        }
        if !self.add_groups.is_empty() {
            lines.push(format!("add to groups {}", self.add_groups.join(", ")));
        }
        for (key, values) in &self.add_tags {
            lines.push(format!(
                "add tag {key}={}",
                values.iter().cloned().collect::<Vec<String>>().join(",")
            ));
        }
        if self.harness_env.is_some() {
            lines.push("set the harness env".to_owned());
        }
        if !self.disable_models.is_empty() {
            lines.push(format!(
                "disable models {}",
                self.disable_models
                    .iter()
                    .cloned()
                    .collect::<Vec<String>>()
                    .join(", ")
            ));
        }
        if !self.enable_models.is_empty() {
            lines.push(format!(
                "enable models {}",
                self.enable_models
                    .iter()
                    .cloned()
                    .collect::<Vec<String>>()
                    .join(", ")
            ));
        }
        lines
    }
}

/// Read a harness env from the text `tools harness-env --set` takes
///
/// Each entry is a value name with an optional `=NAME` rename.
///
/// # Arguments
///
/// * `entries` - The entries to read
pub fn parse_harness_env(entries: &[String]) -> Result<HarnessEnv, String> {
    // export each value, renaming the ones given a name
    let mut env = HarnessEnv::default();
    for entry in entries {
        // split off the rename, if there is one
        let (value, name) = match entry.split_once('=') {
            Some((value, name)) => (value, Some(name)),
            None => (entry.as_str(), None),
        };
        // the value must be one a harness can be given
        let var: HarnessEnvVar = serde_json::from_value(Value::String(value.to_owned()))
            .map_err(|_| format!("`{value}` is not a harness env value"))?;
        env.export.insert(var);
        if let Some(name) = name {
            env.names.insert(var, name.to_owned());
        }
    }
    Ok(env)
}

/// Find what differs between a manifest tool and the tool in Thorium, apart from its versions
///
/// # Arguments
///
/// * `tool` - The tool in the manifest
/// * `live` - The tool in Thorium
pub fn root_change(tool: &ToolManifest, live: &LiveTool) -> Result<RootChange, String> {
    let mut change = RootChange::default();
    // a description the manifest sets must be the tool's
    if let Some(description) = &tool.description
        && live.description.as_ref() != Some(description)
    {
        change.description = Some(description.clone());
    }
    // every group of the manifest must have the tool, though other groups are left alone
    change.add_groups = tool
        .groups
        .iter()
        .filter(|group| !live.groups.contains(group))
        .cloned()
        .collect();
    // every tag value of the manifest must be on the tool, though other tags are left alone
    for (key, values) in &tool.tags {
        let have = live.tags.get(key);
        let missing: BTreeSet<String> = values
            .iter()
            .filter(|value| !have.is_some_and(|have| have.contains(*value)))
            .cloned()
            .collect();
        if !missing.is_empty() {
            change.add_tags.insert(key.clone(), missing);
        }
    }
    // a harness's env and disabled models are the manifest's when it sets them
    if live.kind == ToolKind::Harness {
        if !tool.harness_env.is_empty() {
            let env = parse_harness_env(&tool.harness_env)?;
            if live.harness_env.as_ref() != Some(&env) {
                change.harness_env = Some(env);
            }
        }
        let wanted: BTreeSet<String> = tool.disabled_models.iter().cloned().collect();
        if !tool.disabled_models.is_empty() || !live.disabled_models.is_empty() {
            change.disable_models = wanted.difference(&live.disabled_models).cloned().collect();
            change.enable_models = live.disabled_models.difference(&wanted).cloned().collect();
        }
    }
    Ok(change)
}

/// Build the view of a toolset a manifest describes, in the form `toolsets edit` shows
///
/// # Arguments
///
/// * `name` - The toolset's name
/// * `toolset` - The toolset in the manifest
#[must_use]
pub fn toolset_view(name: &str, toolset: &ToolSetManifest) -> ToolSetView {
    // sort the lists the way the edit view does
    let mut groups = toolset.groups.clone();
    groups.sort();
    let mut tools: Vec<ViewToolRef> = toolset.tools.iter().map(view_tool).collect();
    tools.sort();
    ToolSetView {
        id: None,
        name: name.to_owned(),
        description: toolset.description.clone(),
        groups,
        read_only: toolset.read_only.unwrap_or(false),
        selector: toolset.selector.clone(),
        auto_instance: toolset.auto_instance.unwrap_or(true),
        tools,
    }
}

/// Build the view of an agent a manifest describes, in the form `agents edit` shows
///
/// # Arguments
///
/// * `name` - The agent's name
/// * `agent` - The agent in the manifest
#[must_use]
pub fn agent_view(name: &str, agent: &AgentManifest) -> AgentView {
    // sort the lists the way the edit view does
    let mut groups = agent.groups.clone();
    groups.sort();
    let mut tools: Vec<ViewToolRef> = agent.tools.iter().map(view_tool).collect();
    tools.sort();
    let mut toolsets: Vec<ViewToolSetRef> = agent
        .toolsets
        .iter()
        .map(|toolset| ViewToolSetRef {
            toolset: toolset.toolset.clone(),
            instance: None,
        })
        .collect();
    toolsets.sort();
    let mut sub_agents = agent.sub_agents.clone();
    sub_agents.sort();
    // listing no levels allows every level
    let levels: BTreeSet<ModelAuthArg> = agent
        .model_auth
        .iter()
        .copied()
        .map(ModelAuthArg::from)
        .collect();
    AgentView {
        id: None,
        name: name.to_owned(),
        description: agent.description.clone(),
        groups,
        config: serde_json::to_value(&agent.config).unwrap_or(Value::Null),
        model_auth: (!levels.is_empty()).then(|| levels.into_iter().collect()),
        harness: agent.harness.as_ref().map(view_tool),
        tools,
        toolsets,
        sub_agents,
    }
}

/// Turn a manifest tool reference into the reference an edit view holds
///
/// # Arguments
///
/// * `tool` - The reference to convert
fn view_tool(tool: &ToolRef) -> ViewToolRef {
    // the two types hold the same two fields
    ViewToolRef {
        tool: tool.tool.clone(),
        version: tool.version.clone(),
    }
}

/// Get the view of a toolset in Thorium without its id
///
/// # Arguments
///
/// * `live` - The toolset entity
#[must_use]
pub fn live_toolset_view(live: &Entity) -> ToolSetView {
    // the edit view of the entity, without the id that can't be compared
    let mut view = ToolSetEditable::to_view(live);
    view.id = None;
    view
}

/// Get the view of an agent in Thorium without its id
///
/// # Arguments
///
/// * `live` - The agent entity
#[must_use]
pub fn live_agent_view(live: &Entity) -> AgentView {
    // the edit view of the entity, without the id that can't be compared
    let mut view = AgentEditable::to_view(live);
    view.id = None;
    view
}

/// The fields of a workflow that an import compares and sets
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowView {
    /// The workflow's name
    pub name: String,
    /// What the workflow is for
    pub description: Option<String>,
    /// The groups the workflow is in
    pub groups: Vec<String>,
    /// The names of the agents the workflow runs, sorted
    pub agents: Vec<String>,
    /// The name of the agent a run starts with
    pub entry_agent: Option<String>,
    /// The run-wide limits, as JSON
    pub limits: Value,
    /// The default models, as JSON
    pub default_models: Value,
    /// The levels that may supply model auth; none listed means every level
    pub model_auth: Vec<ModelAuthArg>,
    /// Whether new instances are built automatically
    pub auto_instance: bool,
}

/// Build the view of a workflow a manifest describes
///
/// # Arguments
///
/// * `name` - The workflow's name
/// * `workflow` - The workflow in the manifest
#[must_use]
pub fn workflow_view(name: &str, workflow: &WorkflowManifest) -> WorkflowView {
    // sort the lists so views compare equal whatever order they were written in
    let mut groups = workflow.groups.clone();
    groups.sort();
    let mut agents = workflow.agents.clone();
    agents.sort();
    let levels: BTreeSet<ModelAuthArg> = workflow
        .model_auth
        .iter()
        .copied()
        .map(ModelAuthArg::from)
        .collect();
    WorkflowView {
        name: name.to_owned(),
        description: workflow.description.clone(),
        groups,
        agents,
        entry_agent: workflow.entry_agent.clone(),
        limits: serde_json::to_value(&workflow.limits).unwrap_or(Value::Null),
        default_models: serde_json::to_value(&workflow.default_models).unwrap_or(Value::Null),
        model_auth: levels.into_iter().collect(),
        auto_instance: workflow.auto_instance.unwrap_or(true),
    }
}

/// Get the view of a workflow in Thorium
///
/// # Arguments
///
/// * `live` - The workflow entity, with its agents filled in
#[must_use]
pub fn live_workflow_view(live: &Entity) -> WorkflowView {
    // only a workflow catalog has the fields; anything else shows as an empty workflow
    let EntityMetadata::WorkflowCatalog(meta) = &live.metadata else {
        return WorkflowView {
            name: live.name.clone(),
            description: live.description.clone(),
            groups: Vec::new(),
            agents: Vec::new(),
            entry_agent: None,
            limits: Value::Null,
            default_models: Value::Null,
            model_auth: Vec::new(),
            auto_instance: true,
        };
    };
    let mut groups = live.groups.clone();
    groups.sort();
    let mut agents: Vec<String> = meta.agents.iter().map(|agent| agent.name.clone()).collect();
    agents.sort();
    // the entry agent is named by finding it among the workflow's agents
    let entry_agent = meta.entry_agent.and_then(|id| {
        meta.agents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| agent.name.clone())
    });
    let levels: BTreeSet<ModelAuthArg> = meta
        .model_auth
        .levels
        .iter()
        .copied()
        .map(ModelAuthArg::from)
        .collect();
    WorkflowView {
        name: live.name.clone(),
        description: live.description.clone(),
        groups,
        agents,
        entry_agent,
        limits: serde_json::to_value(&meta.limits).unwrap_or(Value::Null),
        default_models: serde_json::to_value(&meta.default_models).unwrap_or(Value::Null),
        model_auth: levels.into_iter().collect(),
        auto_instance: meta.auto_instance,
    }
}

/// Render two views as a unified diff of their YAML, or `None` when they are equal
///
/// # Arguments
///
/// * `old` - What Thorium has
/// * `new` - What the manifest says
pub fn view_diff<T: Serialize + PartialEq>(old: &T, new: &T) -> Option<String> {
    // equal views have nothing to show
    if old == new {
        return None;
    }
    // render both as YAML so a changed list shows as changed lines
    let old = serde_norway::to_string(old).unwrap_or_default();
    let new = serde_norway::to_string(new).unwrap_or_default();
    Some(
        TextDiff::from_lines(&old, &new)
            .unified_diff()
            .context_radius(2)
            .header("thorium", "toolbox")
            .to_string(),
    )
}

/// Convert model auth arguments back into the levels the API uses
///
/// # Arguments
///
/// * `levels` - The levels as the views name them
#[must_use]
pub fn auth_levels(levels: &[ModelAuthArg]) -> BTreeSet<ModelAuthLevel> {
    // each name stands for one level
    levels.iter().copied().map(ModelAuthLevel::from).collect()
}

/// Describe what differs between a pipeline in Thorium and the one a manifest asks for, one
/// line per difference
///
/// # Arguments
///
/// * `live` - The pipeline in Thorium
/// * `wanted` - The request the manifest makes, with its stage tools resolved
#[must_use]
pub fn pipeline_differences(live: &Pipeline, wanted: &PipelineRequest) -> Vec<String> {
    let mut lines = Vec::new();
    // the order, compared the way the API compares it
    if !wanted.compare_order(&live.order) {
        lines.push(format!("order -> {}", wanted.order));
    }
    // the SLA defaults to a week when the manifest doesn't set one
    if let Some(sla) = wanted.sla
        && sla != live.sla
    {
        lines.push(format!("sla {} -> {sla}", live.sla));
    }
    // triggers
    let wanted_triggers: BTreeMap<_, _> = wanted.triggers.iter().collect();
    let live_triggers: BTreeMap<_, _> = live.triggers.iter().collect();
    if wanted_triggers != live_triggers {
        lines.push("triggers differ".to_owned());
    }
    // the description
    if wanted.description != live.description {
        lines.push(format!("description -> {:?}", wanted.description));
    }
    // the tool behind each stage
    if wanted.stage_tools != live.stage_tools {
        for (stage, id) in &wanted.stage_tools {
            if live.stage_tools.get(stage) != Some(id) {
                lines.push(format!("stage {stage} tool -> {id}"));
            }
        }
        for stage in live.stage_tools.keys() {
            if !wanted.stage_tools.contains_key(stage) {
                lines.push(format!("stage {stage} tool removed"));
            }
        }
    }
    lines
}
