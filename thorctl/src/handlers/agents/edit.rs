//! Editing AI agents: the view people edit and the update it becomes
//!
//! The view names everything (tools, toolsets, sub-agents, the harness) instead of carrying IDs.
//! An edit is turned into an update by diffing the edited view against the view of the agent as
//! it was read; only the references that are new need to be looked up, and the edges that are
//! gone are removed by the IDs the agent already has.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::{
    AgentConfig, AgentEntity, Entity, EntityMetadataUpdate, EntityUpdate, ModelAuthLevel,
    ModelAuthSource,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::refs::{ToolRef, ToolSetRef, diff, duplicate, resolve_agent, set_key as set};
use super::{agent_meta, read_config};
use crate::args::agents::{ModelAuthArg, UpdateAgent};
use crate::handlers::edit::{Editable, default_header};
use crate::utils::patch::EditOp;

#[cfg(test)]
mod tests;

/// The document people edit for an AI agent
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentView {
    /// The agent's ID, shown for context and ignored on save
    #[serde(rename = "*id*", default)]
    pub id: Option<String>,
    /// The agent's name
    pub name: String,
    /// What the agent is for
    #[serde(default)]
    pub description: Option<String>,
    /// The groups the agent is in
    pub groups: Vec<String>,
    /// The agent's config: prompts, accepted models, limits (read strictly, unknown keys refused)
    pub config: Value,
    /// The levels that may supply model auth; unset allows every level
    #[serde(default)]
    pub model_auth: Option<Vec<ModelAuthArg>>,
    /// The harness the agent runs in
    #[serde(default)]
    pub harness: Option<ToolRef>,
    /// The tools the agent calls
    pub tools: Vec<ToolRef>,
    /// The toolsets the agent calls
    pub toolsets: Vec<ToolSetRef>,
    /// The names of the agents this agent delegates to
    pub sub_agents: Vec<String>,
}

/// Which AI agent to edit and how to widen the groups of its edge targets
#[derive(Debug, Clone, Copy)]
pub struct AgentTarget {
    /// The agent's ID
    pub id: Uuid,
    /// Whether to add the agent's groups to edge targets that miss some of them
    pub extend_target_groups: bool,
}

/// The references an edit adds, which need to be looked up
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    /// The tools that are new to the agent
    pub tools: Vec<ToolRef>,
    /// The toolsets that are new to the agent
    pub toolsets: Vec<ToolSetRef>,
    /// The names of the sub-agents that are new to the agent
    pub sub_agents: Vec<String>,
    /// The harness, when the edit sets a different one
    pub harness: Option<ToolRef>,
}

/// The IDs the references an edit adds were found to name
#[derive(Debug, Default)]
pub struct Resolved {
    /// The ID each added tool or tool instance
    pub tools: BTreeMap<ToolRef, Uuid>,
    /// The ID of each added toolset or toolset instance
    pub toolsets: BTreeMap<ToolSetRef, Uuid>,
    /// The ID of each added sub-agent
    pub sub_agents: BTreeMap<String, Uuid>,
    /// The ID of the harness or harness instance to set
    pub harness: Option<Uuid>,
}

/// The editing of AI agents by the shared edit flow
pub struct AgentEditable;

/// Read an agent config document strictly
///
/// Unknown keys are refused, so a mistyped setting is an error instead of being dropped.
///
/// # Arguments
///
/// * `value` - The config as edited
pub fn strict_config(value: &Value) -> Result<AgentConfig, String> {
    // only a mapping of settings can be a config
    let Value::Object(map) = value else {
        return Err("config must be a mapping of settings".to_owned());
    };
    // refuse any key a config doesn't have
    let known = serde_json::to_value(AgentConfig::default())
        .map_err(|err| format!("Failed to read the config keys: {err}"))?;
    let known: BTreeSet<String> = known
        .as_object()
        .map_or_else(BTreeSet::new, |known| known.keys().cloned().collect());
    let unknown: Vec<&str> = map
        .keys()
        .filter(|key| !known.contains(*key))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "config has unknown key(s): {} (valid keys: {})",
            unknown.join(", "),
            known.iter().cloned().collect::<Vec<String>>().join(", ")
        ));
    }
    // read the settings as typed values and run the checks the API runs
    let config: AgentConfig = serde_json::from_value(value.clone())
        .map_err(|err| format!("config is not valid: {err}"))?;
    config.validate()?;
    Ok(config)
}

/// Check a view for mistakes that need no server to find
///
/// # Arguments
///
/// * `view` - The edited view
fn check(view: &AgentView) -> Result<(), String> {
    // an agent needs a name and a group
    if view.name.trim().is_empty() {
        return Err("name must not be blank".to_owned());
    }
    if view.groups.is_empty() {
        return Err("an AI agent needs at least one group".to_owned());
    }
    if let Some(group) = duplicate(&view.groups) {
        return Err(format!("group {group} is listed twice"));
    }
    // the config is read strictly
    strict_config(&view.config)?;
    // an edge is listed once
    if let Some(tool) = duplicate(&view.tools) {
        return Err(format!("tool {} is listed twice", tool.tool));
    }
    if let Some(toolset) = duplicate(&view.toolsets) {
        return Err(format!("toolset {} is listed twice", toolset.toolset));
    }
    if let Some(sub_agent) = duplicate(&view.sub_agents) {
        return Err(format!("sub-agent {sub_agent} is listed twice"));
    }
    if let Some(levels) = &view.model_auth
        && let Some(level) = duplicate(levels)
    {
        return Err(format!("model auth level {level:?} is listed twice"));
    }
    // an agent cannot delegate to itself
    if view.sub_agents.contains(&view.name) {
        return Err(format!("{} cannot delegate to itself", view.name));
    }
    // a pinned version names a version
    let blank_version = view
        .tools
        .iter()
        .chain(view.harness.iter())
        .any(|tool| tool.version.as_deref().is_some_and(|v| v.trim().is_empty()));
    if blank_version {
        return Err(
            "a tool version must not be blank; leave it out to follow the latest".to_owned(),
        );
    }
    Ok(())
}

/// The model auth levels a view allows, with none listed meaning every level
///
/// # Arguments
///
/// * `view` - The view to read
fn auth_levels(view: &AgentView) -> BTreeSet<ModelAuthArg> {
    // an unset or empty list allows every level
    view.model_auth.iter().flatten().copied().collect()
}

/// Find the references an edit adds
///
/// # Arguments
///
/// * `before` - The view of the agent as it was read
/// * `after` - The edited view
#[must_use]
pub fn wanted(before: &AgentView, after: &AgentView) -> Wanted {
    // keep what the edited view lists that the agent didn't have
    let new = |items: &[ToolRef], old: &[ToolRef]| -> Vec<ToolRef> {
        let old: BTreeSet<&ToolRef> = old.iter().collect();
        let mut new: Vec<ToolRef> = items
            .iter()
            .filter(|item| !old.contains(item))
            .cloned()
            .collect();
        new.sort();
        new
    };
    let old_toolsets: BTreeSet<&ToolSetRef> = before.toolsets.iter().collect();
    let mut toolsets: Vec<ToolSetRef> = after
        .toolsets
        .iter()
        .filter(|item| !old_toolsets.contains(item))
        .cloned()
        .collect();
    toolsets.sort();
    let old_agents: BTreeSet<&String> = before.sub_agents.iter().collect();
    let mut sub_agents: Vec<String> = after
        .sub_agents
        .iter()
        .filter(|item| !old_agents.contains(item))
        .cloned()
        .collect();
    sub_agents.sort();
    Wanted {
        tools: new(&after.tools, &before.tools),
        toolsets,
        sub_agents,
        // a different harness replaces the old one
        harness: after
            .harness
            .clone()
            .filter(|harness| before.harness.as_ref() != Some(harness)),
    }
}

/// Look up the references an edit adds
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups the agent will be in, which the references are looked up in
/// * `wants` - The references to look up
pub async fn resolve(
    thorium: &Thorium,
    groups: &[String],
    wants: &Wanted,
) -> Result<Resolved, Error> {
    // find each tool, toolset and sub-agent the edit adds
    let mut resolved = Resolved::default();
    for tool in &wants.tools {
        let id = tool.resolve(thorium, groups, "a tool").await?;
        resolved.tools.insert(tool.clone(), id);
    }
    for toolset in &wants.toolsets {
        let id = toolset.resolve(thorium, groups).await?;
        resolved.toolsets.insert(toolset.clone(), id);
    }
    for name in &wants.sub_agents {
        let id = resolve_agent(thorium, groups, name).await?;
        resolved.sub_agents.insert(name.clone(), id);
    }
    // find the harness the edit sets
    if let Some(harness) = &wants.harness {
        resolved.harness = Some(harness.resolve(thorium, groups, "a harness").await?);
    }
    Ok(resolved)
}

/// Get the ID an edge removal names, from the edges the agent has now
///
/// # Arguments
///
/// * `existing` - The edges the agent has, by reference
/// * `key` - The reference to find
/// * `what` - What the reference is, for the error
fn existing_id<K: Ord>(existing: &BTreeMap<K, Uuid>, key: &K, what: &str) -> Result<Uuid, Error> {
    // the edge was in the view the agent was read as
    existing
        .get(key)
        .copied()
        .ok_or_else(|| Error::new(format!("The AI agent has no {what} edge to remove")))
}

/// Get the ID a new reference was resolved to
///
/// # Arguments
///
/// * `resolved` - The IDs the new references were resolved to
/// * `key` - The reference to find
/// * `what` - What the reference is, for the error
fn resolved_id<K: Ord>(resolved: &BTreeMap<K, Uuid>, key: &K, what: &str) -> Result<Uuid, Error> {
    // every added reference is looked up before the update is built
    resolved
        .get(key)
        .copied()
        .ok_or_else(|| Error::new(format!("The new {what} was not looked up")))
}

/// The edges an edit adds and removes, by ID
#[derive(Debug, Default)]
struct EdgeIds {
    /// The tools and tool instances to start calling
    add_tools: Vec<Uuid>,
    /// The tools and tool instances to stop calling
    remove_tools: Vec<Uuid>,
    /// The toolsets and toolset instances to start calling
    add_tool_sets: Vec<Uuid>,
    /// The toolsets and toolset instances to stop calling
    remove_tool_sets: Vec<Uuid>,
    /// The sub-agents to start delegating to
    add_sub_agents: Vec<Uuid>,
    /// The sub-agents to stop delegating to
    remove_sub_agents: Vec<Uuid>,
}

impl EdgeIds {
    /// Whether the edit adds and removes no edges
    fn is_empty(&self) -> bool {
        // every list is empty
        self.add_tools.is_empty()
            && self.remove_tools.is_empty()
            && self.add_tool_sets.is_empty()
            && self.remove_tool_sets.is_empty()
            && self.add_sub_agents.is_empty()
            && self.remove_sub_agents.is_empty()
    }
}

/// Find the IDs of the edges an edit adds and removes
///
/// # Arguments
///
/// * `data` - The agent as it was read, whose edges' IDs removals use
/// * `before` - The view of the agent as it was read
/// * `after` - The edited view
/// * `resolved` - The IDs of the references the edit adds
fn edge_ids(
    data: &Entity,
    before: &AgentView,
    after: &AgentView,
    resolved: &Resolved,
) -> Result<EdgeIds, Error> {
    // the edges the agent has now, by the references the view names them with
    let meta = agent_meta(data).cloned().unwrap_or_default();
    let mut tools = BTreeMap::new();
    for target in meta.latest_tools.iter().chain(&meta.pinned_tools) {
        tools.insert(ToolRef::from_entity(target), target.id);
    }
    let mut toolsets = BTreeMap::new();
    for target in &meta.tool_sets {
        toolsets.insert(ToolSetRef::from_entity(target), target.id);
    }
    let mut sub_agents = BTreeMap::new();
    for target in &meta.sub_agents {
        sub_agents.insert(target.name.clone(), target.id);
    }
    // diff each kind of edge
    let tool_diff = diff(&before.tools, &after.tools);
    let toolset_diff = diff(&before.toolsets, &after.toolsets);
    let sub_agent_diff = diff(&before.sub_agents, &after.sub_agents);
    let mut add_tools = Vec::new();
    for tool in &tool_diff.0 {
        add_tools.push(resolved_id(&resolved.tools, tool, "tool")?);
    }
    let mut remove_tools = Vec::new();
    for tool in &tool_diff.1 {
        remove_tools.push(existing_id(&tools, tool, "tool")?);
    }
    let mut add_tool_sets = Vec::new();
    for toolset in &toolset_diff.0 {
        add_tool_sets.push(resolved_id(&resolved.toolsets, toolset, "toolset")?);
    }
    let mut remove_tool_sets = Vec::new();
    for toolset in &toolset_diff.1 {
        remove_tool_sets.push(existing_id(&toolsets, toolset, "toolset")?);
    }
    let mut add_sub_agents = Vec::new();
    for name in &sub_agent_diff.0 {
        add_sub_agents.push(resolved_id(&resolved.sub_agents, name, "sub-agent")?);
    }
    let mut remove_sub_agents = Vec::new();
    for name in &sub_agent_diff.1 {
        remove_sub_agents.push(existing_id(&sub_agents, name, "sub-agent")?);
    }
    Ok(EdgeIds {
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
    })
}

/// Turn an edited view into the update that applies it
///
/// Returns `None` when the view holds no change.
///
/// # Arguments
///
/// * `data` - The agent as it was read, whose edges' IDs removals use
/// * `before` - The view of the agent as it was read
/// * `after` - The edited view
/// * `resolved` - The IDs of the references the edit adds
pub fn build_update(
    data: &Entity,
    before: &AgentView,
    after: &AgentView,
    resolved: &Resolved,
) -> Result<Option<EntityUpdate>, Error> {
    // the edges the edit adds and removes
    let edges = edge_ids(data, before, after, resolved)?;
    // a different harness is cleared first when the agent has one
    let harness_changed = before.harness != after.harness;
    let clear_harness = (harness_changed && before.harness.is_some()).then_some(true);
    let harness = if harness_changed && after.harness.is_some() {
        resolved.harness
    } else {
        None
    };
    // a changed config replaces the whole config
    let before_config = strict_config(&before.config).map_err(Error::new)?;
    let after_config = strict_config(&after.config).map_err(Error::new)?;
    let config = (before_config != after_config).then_some(after_config);
    // changed model auth replaces the levels; an empty set allows every level
    let levels = auth_levels(after);
    let model_auth = (auth_levels(before) != levels).then(|| ModelAuthSource {
        levels: levels.iter().copied().map(ModelAuthLevel::from).collect(),
    });
    // the agent's own fields go in the entity update
    let mut update = EntityUpdate::default();
    if before.name != after.name {
        update.name = Some(after.name.clone());
    }
    update.add_groups = after
        .groups
        .iter()
        .filter(|group| !before.groups.contains(group))
        .cloned()
        .collect();
    update.remove_groups = before
        .groups
        .iter()
        .filter(|group| !after.groups.contains(group))
        .cloned()
        .collect();
    if before.description != after.description {
        match &after.description {
            Some(description) => update.description = Some(description.clone()),
            None => update.clear_description = true,
        }
    }
    // the config, model auth, harness and edges go in the metadata update
    let metadata_changed = config.is_some()
        || model_auth.is_some()
        || harness.is_some()
        || clear_harness.is_some()
        || !edges.is_empty();
    if metadata_changed {
        update.metadata = Some(EntityMetadataUpdate::Agent {
            config,
            model_auth,
            harness,
            clear_harness,
            add_tools: edges.add_tools,
            remove_tools: edges.remove_tools,
            add_tool_sets: edges.add_tool_sets,
            remove_tool_sets: edges.remove_tool_sets,
            add_sub_agents: edges.add_sub_agents,
            remove_sub_agents: edges.remove_sub_agents,
            extend_target_groups: false,
        });
    }
    // nothing changed when no field was set
    let unchanged = update.name.is_none()
        && update.add_groups.is_empty()
        && update.remove_groups.is_empty()
        && update.description.is_none()
        && !update.clear_description
        && update.metadata.is_none();
    Ok((!unchanged).then_some(update))
}

/// Turn the typed flags of `agents update` into edits of the view
///
/// # Arguments
///
/// * `cmd` - The update command
/// * `view` - The view of the agent as it was read, which group changes are relative to
pub fn typed_ops(cmd: &UpdateAgent, view: &AgentView) -> Result<Vec<EditOp>, Error> {
    let mut ops = Vec::new();
    // the entity fields
    if let Some(name) = &cmd.rename {
        ops.push(set("name", json!(name)));
    }
    if let Some(description) = &cmd.description {
        ops.push(set("description", json!(description)));
    }
    if cmd.clear_description {
        ops.push(set("description", Value::Null));
    }
    // group changes start from the groups the agent is in
    if !cmd.add_group.is_empty() || !cmd.remove_group.is_empty() {
        let mut groups = view.groups.clone();
        groups.retain(|group| !cmd.remove_group.contains(group));
        for group in &cmd.add_group {
            if !groups.contains(group) {
                groups.push(group.clone());
            }
        }
        ops.push(set("groups", json!(groups)));
    }
    // a config file replaces the whole config
    if let Some(path) = &cmd.config {
        let config = read_config(path)?;
        let config = serde_json::to_value(config)
            .map_err(|err| Error::new(format!("Failed to convert the config: {err}")))?;
        ops.push(set("config", config));
    }
    // model auth levels are listed once, in precedence order
    if !cmd.model_auth.is_empty() {
        let levels: BTreeSet<ModelAuthArg> = cmd.model_auth.iter().copied().collect();
        ops.push(set("model_auth", json!(levels)));
    }
    if cmd.clear_model_auth {
        ops.push(set("model_auth", Value::Null));
    }
    Ok(ops)
}

impl Editable for AgentEditable {
    type Client = Thorium;
    type Target = AgentTarget;
    type Data = Entity;
    type View = AgentView;
    type Update = EntityUpdate;
    const KIND: &'static str = "Agent";
    const FIELD_ORDER: &'static [&'static str] = &[
        "*id*",
        "name",
        "description",
        "groups",
        "config",
        "model_auth",
        "harness",
        "tools",
        "toolsets",
        "sub_agents",
    ];

    /// Read the agent with its edges filled in
    async fn fetch(client: &Thorium, target: &AgentTarget) -> Result<Entity, Error> {
        client.entities.get(target.id).await
    }

    /// Build the view people edit from an agent
    fn to_view(data: &Entity) -> AgentView {
        // an entity that isn't an agent shows as an empty one
        let meta: AgentEntity = agent_meta(data).cloned().unwrap_or_default();
        // name each edge, keeping every list in a fixed order
        let mut tools: Vec<ToolRef> = meta
            .latest_tools
            .iter()
            .chain(&meta.pinned_tools)
            .map(ToolRef::from_entity)
            .collect();
        tools.sort();
        let mut toolsets: Vec<ToolSetRef> =
            meta.tool_sets.iter().map(ToolSetRef::from_entity).collect();
        toolsets.sort();
        let mut sub_agents: Vec<String> = meta
            .sub_agents
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        sub_agents.sort();
        let mut groups = data.groups.clone();
        groups.sort();
        // every level may supply model auth when none is listed
        let levels: BTreeSet<ModelAuthArg> = meta
            .model_auth
            .levels
            .iter()
            .copied()
            .map(ModelAuthArg::from)
            .collect();
        AgentView {
            id: Some(data.id.to_string()),
            name: data.name.clone(),
            description: data.description.clone(),
            groups,
            config: serde_json::to_value(&meta.config).unwrap_or(Value::Null),
            model_auth: (!levels.is_empty()).then(|| levels.into_iter().collect()),
            harness: meta.harness.as_deref().map(ToolRef::from_entity),
            tools,
            toolsets,
            sub_agents,
        }
    }

    /// The agent's ID
    fn id(data: &Entity) -> String {
        data.id.to_string()
    }

    /// The agent's name
    fn name(data: &Entity) -> String {
        data.name.clone()
    }

    /// Check an edited view before anything is sent
    fn validate(view: &AgentView, data: &Entity) -> Result<(), Error> {
        // the view's own checks need nothing from the agent
        let _ = data;
        check(view).map_err(Error::new)
    }

    /// Look up what the edit adds and build the update
    async fn calculate_update(
        client: &Thorium,
        data: &Entity,
        view: AgentView,
    ) -> Result<Option<EntityUpdate>, Error> {
        // find what the edit adds, in the groups the agent will be in
        let before = Self::to_view(data);
        let wants = wanted(&before, &view);
        let resolved = resolve(client, &view.groups, &wants).await?;
        build_update(data, &before, &view, &resolved)
    }

    /// Send the update
    async fn send_update(
        client: &Thorium,
        target: &AgentTarget,
        data: &Entity,
        update: &EntityUpdate,
    ) -> Result<(), Error> {
        // widen the targets of added edges only when asked
        let _ = data;
        let mut update = update.clone();
        if target.extend_target_groups
            && let Some(EntityMetadataUpdate::Agent {
                extend_target_groups,
                ..
            }) = &mut update.metadata
        {
            *extend_target_groups = true;
        }
        client.entities.update(target.id, update).await?;
        Ok(())
    }

    /// The comment block at the top of the editor buffer
    fn header(data: &Entity) -> String {
        // the standard header, then how edges are written
        let mut header = default_header::<Self>(data);
        header.push_str(
            "# tools: [{tool: NAME, version: LABEL}]; no version follows the latest version.\n",
        );
        header.push_str("# toolsets: [{toolset: NAME, instance: REVISION}]; no instance follows the latest one.\n");
        header.push_str("# harness: {tool: NAME, version: LABEL} or null; sub_agents: [NAME].\n");
        header
            .push_str("# model_auth: [harness, workflow, group, user] or null for every level.\n");
        header
    }
}
