//! Editing workflows: the view people edit and the update it becomes
//!
//! The view names everything (the agents and the entry agent) instead of carrying IDs. An edit
//! is turned into an update by diffing the edited view against the view of the workflow as it
//! was read; only the agents that are new need to be looked up, and the agents that are gone are
//! removed by the IDs the workflow already has.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::{
    Entity, EntityMetadataUpdate, EntityUpdate, ModelAuthLevel, ModelAuthSource, ModelMatcher,
    WorkflowCatalogEntity, WorkflowCatalogEntityRequest, WorkflowLimits,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::workflow_meta;
use crate::args::agents::ModelAuthArg;
use crate::args::toolsets::Switch;
use crate::args::workflows::{CreateWorkflow, UpdateWorkflow};
use crate::handlers::agents::refs::{diff, duplicate, resolve_agent, set_key as set};
use crate::handlers::edit::{Editable, default_header};
use crate::utils::patch::{EditOp, PathSeg, merge_patch};

#[cfg(test)]
mod tests;

/// The run-wide limits of a workflow as people edit them
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsView {
    /// The most sub-agent invocations a run has going at once; unset uses the harness default
    #[serde(default)]
    pub max_alive_sub_agents: Option<u32>,
}

/// The document people edit for a workflow
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowView {
    /// The workflow's ID, shown for context and ignored on save
    #[serde(rename = "*id*", default)]
    pub id: Option<String>,
    /// The workflow's name
    pub name: String,
    /// What the workflow is for
    #[serde(default)]
    pub description: Option<String>,
    /// The groups the workflow is in
    pub groups: Vec<String>,
    /// The names of the AI agents in the workflow
    pub agents: Vec<String>,
    /// The name of the AI agent that receives a run's inputs first
    #[serde(default)]
    pub entry_agent: Option<String>,
    /// The run-wide limits
    pub limits: LimitsView,
    /// The models agents without their own models list accept, highest priority first
    pub default_models: Vec<ModelMatcher>,
    /// The levels that may supply model auth; unset allows every level
    #[serde(default)]
    pub model_auth: Option<Vec<ModelAuthArg>>,
    /// Whether new instances are built automatically when the workflow or its agents change
    pub auto_instance: bool,
}

/// Which workflow to edit and how to widen the groups of its agents
#[derive(Debug, Clone, Copy)]
pub struct WorkflowTarget {
    /// The workflow's ID
    pub id: Uuid,
    /// Whether to add the workflow's groups to agents that miss some of them
    pub extend_target_groups: bool,
}

/// The IDs the agents an edit adds were found to name
#[derive(Debug, Default)]
pub struct Resolved {
    /// The ID of each added agent, by the name the view gives it
    pub agents: BTreeMap<String, Uuid>,
}

/// The editing of workflows by the shared edit flow
pub struct WorkflowEditable;

/// Get a workflow's metadata, or an empty workflow for another kind of entity
///
/// # Arguments
///
/// * `data` - The entity to read
fn meta_of(data: &Entity) -> WorkflowCatalogEntity {
    // only workflows have workflow metadata
    workflow_meta(data).cloned().unwrap_or_default()
}

/// Check the fields of a view for mistakes that need no server to find
///
/// # Arguments
///
/// * `view` - The edited view
fn check_fields(view: &WorkflowView) -> Result<(), String> {
    // a workflow needs a name and a group
    if view.name.trim().is_empty() {
        return Err("name must not be blank".to_owned());
    }
    if view.groups.is_empty() {
        return Err("a workflow needs at least one group".to_owned());
    }
    if let Some(group) = duplicate(&view.groups) {
        return Err(format!("group {group} is listed twice"));
    }
    // an agent is listed once
    if let Some(agent) = duplicate(&view.agents) {
        return Err(format!("agent {agent} is listed twice"));
    }
    // a model auth level is listed once
    if let Some(levels) = &view.model_auth
        && let Some(level) = duplicate(levels)
    {
        return Err(format!("model auth level {level:?} is listed twice"));
    }
    // a default model names a model
    if view
        .default_models
        .iter()
        .any(|matcher| matcher.model_id.trim().is_empty())
    {
        return Err("a default model needs a model_id".to_owned());
    }
    Ok(())
}

/// Check a view for mistakes that need no server to find
///
/// # Arguments
///
/// * `view` - The edited view
fn check(view: &WorkflowView) -> Result<(), String> {
    // the fields are valid on their own
    check_fields(view)?;
    // the entry agent is one of the workflow's agents
    if let Some(entry) = &view.entry_agent
        && !view.agents.contains(entry)
    {
        return Err(format!(
            "entry_agent {entry} must be one of the workflow's agents"
        ));
    }
    Ok(())
}

/// The model auth levels a view allows, with none listed meaning every level
///
/// # Arguments
///
/// * `view` - The view to read
fn auth_levels(view: &WorkflowView) -> BTreeSet<ModelAuthArg> {
    // an unset or empty list allows every level
    view.model_auth.iter().flatten().copied().collect()
}

/// Build the workflow a create command describes from its spec and flags
///
/// A new workflow starts from the defaults of a new workflow, takes what the spec sets, and then
/// what the flags set: a flag replaces the spec's value (groups and lists included), and agents
/// from flags are added to the spec's agents. The agents and entry agent may be names or IDs.
///
/// # Arguments
///
/// * `spec` - The spec document, if one was given
/// * `cmd` - The create command
pub fn create_view(spec: Option<&Value>, cmd: &CreateWorkflow) -> Result<WorkflowView, Error> {
    // start from the defaults of a new workflow and lay the spec over them
    let mut doc = json!({
        "name": "",
        "groups": [],
        "agents": [],
        "limits": {},
        "default_models": [],
        "auto_instance": true,
    });
    if let Some(spec) = spec {
        merge_patch(&mut doc, spec);
    }
    // lay the flags over that
    if let Some(name) = &cmd.name {
        doc["name"] = json!(name);
    }
    if !cmd.group.is_empty() {
        doc["groups"] = json!(cmd.group);
    }
    if let Some(description) = &cmd.description {
        doc["description"] = json!(description);
    }
    if let Some(entry) = &cmd.entry {
        doc["entry_agent"] = json!(entry);
    }
    if !cmd.default_model.is_empty() {
        let models: Vec<ModelMatcher> = cmd
            .default_model
            .iter()
            .map(|model_id| ModelMatcher {
                model_id: model_id.clone(),
                ..ModelMatcher::default()
            })
            .collect();
        doc["default_models"] = serde_json::to_value(models)
            .map_err(|err| Error::new(format!("Failed to convert the default models: {err}")))?;
    }
    if let Some(max) = cmd.max_alive_sub_agents {
        doc["limits"] = json!({ "max_alive_sub_agents": max });
    }
    if !cmd.model_auth.is_empty() {
        let levels: BTreeSet<ModelAuthArg> = cmd
            .model_auth
            .iter()
            .map(|level| ModelAuthArg::from(ModelAuthLevel::from(*level)))
            .collect();
        doc["model_auth"] = json!(levels);
    }
    if cmd.no_auto_instance {
        doc["auto_instance"] = json!(false);
    }
    // read the result as a view, which refuses unknown keys
    let mut view: WorkflowView = serde_json::from_value(doc)
        .map_err(|err| Error::new(format!("Invalid workflow spec: {err}")))?;
    // agents named by flags follow the spec's agents
    view.agents.extend(cmd.agent.iter().cloned());
    check_fields(&view).map_err(Error::new)?;
    Ok(view)
}

/// Build the request that creates a workflow
///
/// Settings that match a new workflow's defaults are left out, so the API's own defaults apply.
///
/// # Arguments
///
/// * `view` - The workflow to create
/// * `agents` - The IDs of the view's agents
/// * `entry` - The ID of the entry agent
/// * `extend_target_groups` - Whether to add the workflow's groups to agents that miss some
pub fn create_request(
    view: &WorkflowView,
    agents: &[Uuid],
    entry: Option<Uuid>,
    extend_target_groups: bool,
) -> Result<WorkflowCatalogEntityRequest, Error> {
    // an agent is in the workflow once, whether it was named or given by ID
    if duplicate(agents).is_some() {
        return Err(Error::new("an agent is listed twice"));
    }
    // the entry agent is one of the workflow's agents
    if let Some(entry) = entry
        && !agents.contains(&entry)
    {
        return Err(Error::new(format!(
            "the entry agent {entry} must be one of the workflow's agents"
        )));
    }
    // include every agent and the entry agent
    let mut request = WorkflowCatalogEntityRequest::default();
    for agent in agents {
        request = request.agent(*agent);
    }
    if let Some(entry) = entry {
        request = request.entry_agent(entry);
    }
    // set only what differs from a new workflow
    for matcher in &view.default_models {
        request = request.default_model(matcher.clone());
    }
    if view.limits.max_alive_sub_agents.is_some() {
        request = request.limits(WorkflowLimits {
            max_alive_sub_agents: view.limits.max_alive_sub_agents,
        });
    }
    let levels = auth_levels(view);
    if !levels.is_empty() {
        request = request.model_auth(ModelAuthSource {
            levels: levels.iter().copied().map(ModelAuthLevel::from).collect(),
        });
    }
    if !view.auto_instance {
        request = request.auto_instance(false);
    }
    if extend_target_groups {
        request = request.extend_target_groups();
    }
    Ok(request)
}

/// Find the names of the agents an edit adds
///
/// # Arguments
///
/// * `before` - The view of the workflow as it was read
/// * `after` - The edited view
#[must_use]
pub fn wanted(before: &WorkflowView, after: &WorkflowView) -> Vec<String> {
    // the agents the edited view lists that the workflow didn't have
    let mut added = diff(&before.agents, &after.agents).0;
    added.sort();
    added
}

/// Look up the agents an edit adds
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups the workflow will be in, which the agents are looked up in
/// * `wants` - The names of the agents to look up
pub async fn resolve(
    thorium: &Thorium,
    groups: &[String],
    wants: &[String],
) -> Result<Resolved, Error> {
    // find each agent the edit adds
    let mut resolved = Resolved::default();
    for name in wants {
        let id = resolve_agent(thorium, groups, name).await?;
        resolved.agents.insert(name.clone(), id);
    }
    Ok(resolved)
}

/// Turn an edited view into the update that applies it
///
/// Returns `None` when the view holds no change.
///
/// # Arguments
///
/// * `data` - The workflow as it was read, whose agents' IDs removals use
/// * `before` - The view of the workflow as it was read
/// * `after` - The edited view
/// * `resolved` - The IDs of the agents the edit adds
pub fn build_update(
    data: &Entity,
    before: &WorkflowView,
    after: &WorkflowView,
    resolved: &Resolved,
) -> Result<Option<EntityUpdate>, Error> {
    // the agents the workflow has now, by name
    let existing: BTreeMap<String, Uuid> = meta_of(data)
        .agents
        .iter()
        .map(|agent| (agent.name.clone(), agent.id))
        .collect();
    // diff the agents into the IDs to add and remove
    let (added, removed) = diff(&before.agents, &after.agents);
    let mut add_agents = Vec::new();
    for name in &added {
        let id = resolved
            .agents
            .get(name)
            .ok_or_else(|| Error::new(format!("The new agent {name} was not looked up")))?;
        add_agents.push(*id);
    }
    let mut remove_agents = Vec::new();
    for name in &removed {
        let id = existing
            .get(name)
            .ok_or_else(|| Error::new(format!("The workflow has no agent {name} to remove")))?;
        remove_agents.push(*id);
    }
    // a new entry agent is one the workflow has or is getting, and none clears it
    let mut entry_agent = None;
    let mut clear_entry_agent = None;
    if before.entry_agent != after.entry_agent {
        match &after.entry_agent {
            Some(name) => {
                let id = existing
                    .get(name)
                    .or_else(|| resolved.agents.get(name))
                    .ok_or_else(|| {
                        Error::new(format!("The entry agent {name} was not looked up"))
                    })?;
                entry_agent = Some(*id);
            }
            None => clear_entry_agent = Some(true),
        }
    }
    // the settings that changed
    let limits = (before.limits != after.limits).then_some(WorkflowLimits {
        max_alive_sub_agents: after.limits.max_alive_sub_agents,
    });
    let default_models =
        (before.default_models != after.default_models).then(|| after.default_models.clone());
    let levels = auth_levels(after);
    let model_auth = (auth_levels(before) != levels).then(|| ModelAuthSource {
        levels: levels.iter().copied().map(ModelAuthLevel::from).collect(),
    });
    let auto_instance =
        (before.auto_instance != after.auto_instance).then_some(after.auto_instance);
    // the workflow's own fields go in the entity update
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
    // the settings and agents go in the metadata update
    let metadata_changed = entry_agent.is_some()
        || clear_entry_agent.is_some()
        || auto_instance.is_some()
        || limits.is_some()
        || default_models.is_some()
        || model_auth.is_some()
        || !add_agents.is_empty()
        || !remove_agents.is_empty();
    if metadata_changed {
        update.metadata = Some(EntityMetadataUpdate::WorkflowCatalog {
            entry_agent,
            clear_entry_agent,
            auto_instance,
            limits,
            default_models,
            model_auth,
            add_agents,
            remove_agents,
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

/// Turn the typed flags of `workflows update` into edits of the view
///
/// # Arguments
///
/// * `cmd` - The update command
/// * `view` - The view of the workflow as it was read, which group changes are relative to
pub fn typed_ops(cmd: &UpdateWorkflow, view: &WorkflowView) -> Result<Vec<EditOp>, Error> {
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
    // group changes start from the groups the workflow is in
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
    // the run-wide limit
    if let Some(max) = cmd.max_alive_sub_agents {
        ops.push(EditOp::Set {
            path: vec![
                PathSeg::Key("limits".to_owned()),
                PathSeg::Key("max_alive_sub_agents".to_owned()),
            ],
            value: json!(max),
        });
    }
    // default models replace the list, keeping the matcher of a model that was already listed
    if !cmd.default_model.is_empty() {
        let models: Vec<ModelMatcher> = cmd
            .default_model
            .iter()
            .map(|model_id| {
                view.default_models
                    .iter()
                    .find(|matcher| matcher.model_id == *model_id)
                    .cloned()
                    .unwrap_or_else(|| ModelMatcher {
                        model_id: model_id.clone(),
                        ..ModelMatcher::default()
                    })
            })
            .collect();
        let models = serde_json::to_value(models)
            .map_err(|err| Error::new(format!("Failed to convert the default models: {err}")))?;
        ops.push(set("default_models", models));
    }
    if cmd.clear_default_models {
        ops.push(set("default_models", json!([])));
    }
    // model auth levels are listed once, in precedence order
    if !cmd.model_auth.is_empty() {
        let levels: BTreeSet<ModelAuthArg> = cmd.model_auth.iter().copied().collect();
        ops.push(set("model_auth", json!(levels)));
    }
    if cmd.clear_model_auth {
        ops.push(set("model_auth", Value::Null));
    }
    // the automatic instancing switch
    if let Some(state) = cmd.auto_instance {
        ops.push(set("auto_instance", json!(state == Switch::On)));
    }
    Ok(ops)
}

impl Editable for WorkflowEditable {
    type Client = Thorium;
    type Target = WorkflowTarget;
    type Data = Entity;
    type View = WorkflowView;
    type Update = EntityUpdate;
    const KIND: &'static str = "Workflow";
    const FIELD_ORDER: &'static [&'static str] = &[
        "*id*",
        "name",
        "description",
        "groups",
        "agents",
        "entry_agent",
        "limits",
        "default_models",
        "model_auth",
        "auto_instance",
    ];

    /// Read the workflow with its agents filled in
    async fn fetch(client: &Thorium, target: &WorkflowTarget) -> Result<Entity, Error> {
        client.entities.get(target.id).await
    }

    /// Build the view people edit from a workflow
    fn to_view(data: &Entity) -> WorkflowView {
        // name each agent, keeping the lists in a fixed order
        let meta = meta_of(data);
        let mut agents: Vec<String> = meta.agents.iter().map(|agent| agent.name.clone()).collect();
        agents.sort();
        let entry_agent = meta.entry_agent.and_then(|id| {
            meta.agents
                .iter()
                .find(|agent| agent.id == id)
                .map(|agent| agent.name.clone())
        });
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
        WorkflowView {
            id: Some(data.id.to_string()),
            name: data.name.clone(),
            description: data.description.clone(),
            groups,
            agents,
            entry_agent,
            limits: LimitsView {
                max_alive_sub_agents: meta.limits.max_alive_sub_agents,
            },
            default_models: meta.default_models.clone(),
            model_auth: (!levels.is_empty()).then(|| levels.into_iter().collect()),
            auto_instance: meta.auto_instance,
        }
    }

    /// The workflow's ID
    fn id(data: &Entity) -> String {
        data.id.to_string()
    }

    /// The workflow's name
    fn name(data: &Entity) -> String {
        data.name.clone()
    }

    /// Check an edited view before anything is sent
    fn validate(view: &WorkflowView, data: &Entity) -> Result<(), Error> {
        // the view's own checks need nothing from the workflow
        let _ = data;
        check(view).map_err(Error::new)
    }

    /// Look up what the edit adds and build the update
    async fn calculate_update(
        client: &Thorium,
        data: &Entity,
        view: WorkflowView,
    ) -> Result<Option<EntityUpdate>, Error> {
        // find the agents the edit adds, in the groups the workflow will be in
        let before = Self::to_view(data);
        let resolved = resolve(client, &view.groups, &wanted(&before, &view)).await?;
        build_update(data, &before, &view, &resolved)
    }

    /// Send the update
    async fn send_update(
        client: &Thorium,
        target: &WorkflowTarget,
        data: &Entity,
        update: &EntityUpdate,
    ) -> Result<(), Error> {
        // widen the groups of added agents only when asked
        let _ = data;
        let mut update = update.clone();
        if target.extend_target_groups
            && let Some(EntityMetadataUpdate::WorkflowCatalog {
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
        // the standard header, then how the agents and settings are written
        let mut header = default_header::<Self>(data);
        header.push_str("# agents: [NAME]; entry_agent: NAME or null, one of the agents.\n");
        header.push_str("# default_models: [{model_id: ID}], highest priority first.\n");
        header
            .push_str("# model_auth: [harness, workflow, group, user] or null for every level.\n");
        header
    }
}
