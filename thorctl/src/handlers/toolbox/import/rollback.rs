//! Undoing a partly applied import
//!
//! The journal's entries are replayed newest first, so pipelines go before the workflows, agents,
//! and toolsets they name, those go before the tool versions they use, and groups go last. An
//! entry that can't be undone is reported and the rest are still attempted.

use std::collections::{BTreeMap, BTreeSet};
use thorium::Error;
use thorium::models::{
    Entity, EntityMetadata, EntityMetadataUpdate, EntityUpdate, ModelAuthSource, Pipeline,
    PipelineRequest, PipelineUpdate,
};
use uuid::Uuid;

use super::classify::{auth_levels, live_agent_view, live_toolset_view, live_workflow_view};
use super::journal::{Entry, RootUndo};
use super::world::ImportApi;
use crate::handlers::toolbox::v2::plan_types::ItemKind;

/// What a rollback did
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollbackOutcome {
    /// What was undone, in the order it was undone
    pub undone: Vec<String>,
    /// What could not be undone, with why
    pub failed: Vec<String>,
}

/// Undo every journaled change, newest first
///
/// # Arguments
///
/// * `api` - The API to undo the changes through
/// * `entries` - The journal's entries in the order the changes were made
pub async fn rollback(api: &impl ImportApi, entries: Vec<Entry>) -> RollbackOutcome {
    let mut outcome = RollbackOutcome::default();
    // undo each change, newest first, without stopping at one that can't be undone
    for entry in entries.into_iter().rev() {
        let line = entry.describe();
        match undo(api, entry).await {
            Ok(()) => outcome.undone.push(line),
            Err(err) => outcome.failed.push(format!("{line}: {err}")),
        }
    }
    outcome
}

/// Undo one change
///
/// # Arguments
///
/// * `api` - The API to undo the change through
/// * `entry` - The change to undo
async fn undo(api: &impl ImportApi, entry: Entry) -> Result<(), Error> {
    match entry {
        Entry::CreatedGroup { name } => api.delete_group(&name).await,
        Entry::CreatedPolicy { name, id } => api.delete_policy(&name, id).await,
        Entry::UpdatedPolicy { original } => api.restore_policy(*original).await,
        // a tool goes after every version it has
        Entry::CreatedTool { id, .. } => {
            for instance in api.tool_instances(id).await? {
                api.delete_entity(instance.id).await?;
            }
            api.delete_entity(id).await
        }
        Entry::PublishedVersion { id, .. } | Entry::CreatedEntity { id, .. } => {
            api.delete_entity(id).await
        }
        Entry::UpdatedToolRoot { id, undo, .. } => undo_root(api, id, &undo).await,
        Entry::UpdatedEntity { kind, original } => undo_entity(api, kind, &original).await,
        Entry::CreatedPipeline { group, name } => api.delete_pipeline(&group, &name).await,
        Entry::UpdatedPipeline { original } => undo_pipeline(api, &original).await,
    }
}

/// Put the root of a tool back
///
/// # Arguments
///
/// * `api` - The API to write through
/// * `id` - The tool's id
/// * `undo` - What to put back
async fn undo_root(api: &impl ImportApi, id: Uuid, undo: &RootUndo) -> Result<(), Error> {
    let mut update = EntityUpdate::default();
    // the description, the groups, and the harness settings go in one update
    match &undo.description {
        Some(Some(description)) => update.description = Some(description.clone()),
        Some(None) => update.clear_description = true,
        None => {}
    }
    update.remove_groups.clone_from(&undo.remove_groups);
    let harness_changed = undo.harness_env.is_some()
        || !undo.enable_models.is_empty()
        || !undo.disable_models.is_empty();
    if harness_changed {
        let (harness_env, clear_harness_env) = match &undo.harness_env {
            Some(Some(env)) => (
                Some(serde_json::to_string(env).map_err(|err| Error::new(err.to_string()))?),
                None,
            ),
            Some(None) => (None, Some(true)),
            None => (None, None),
        };
        update.metadata = Some(EntityMetadataUpdate::Tool {
            version: None,
            spec: None,
            provides: Vec::new(),
            harness_env,
            clear_harness_env,
            disable_models: undo.disable_models.clone(),
            enable_models: undo.enable_models.clone(),
        });
    }
    let changed = update.description.is_some()
        || update.clear_description
        || !update.remove_groups.is_empty()
        || update.metadata.is_some();
    if changed {
        api.update_entity(id, update).await?;
    }
    // the tags the import added are removed from the groups the tool is in
    if !undo.remove_tags.is_empty() {
        let live = api.entity(id).await?;
        api.untag_entity(id, &live.groups, &undo.remove_tags)
            .await?;
    }
    Ok(())
}

/// Put a toolset, agent, or workflow back the way it was
///
/// # Arguments
///
/// * `api` - The API to write through
/// * `kind` - What kind of item it is
/// * `original` - The item as it was before the import changed it
async fn undo_entity(api: &impl ImportApi, kind: ItemKind, original: &Entity) -> Result<(), Error> {
    // read the item as it is now
    let live = api.entity(original.id).await?;
    match kind {
        ItemKind::Toolset => {
            api.update_toolset(&live, live_toolset_view(original))
                .await?;
        }
        ItemKind::Agent => {
            api.update_agent(&live, live_agent_view(original)).await?;
        }
        _ => {
            // the original's agents are known by name, and so are the live ones
            let mut agents: BTreeMap<String, Uuid> = BTreeMap::new();
            for entity in [original, &live] {
                if let EntityMetadata::WorkflowCatalog(meta) = &entity.metadata {
                    agents.extend(
                        meta.agents
                            .iter()
                            .map(|agent| (agent.name.clone(), agent.id)),
                    );
                }
            }
            let wanted = live_workflow_view(original);
            if let Some(update) = workflow_update(&live, &wanted, &agents).map_err(Error::new)? {
                api.update_entity(live.id, update).await?;
            }
        }
    }
    Ok(())
}

/// Build the update that brings a workflow in line with a view
///
/// Returns `None` when the workflow already matches.
///
/// # Arguments
///
/// * `live` - The workflow as it is now
/// * `wanted` - The view the workflow should have
/// * `agents` - The id of every agent the view or the workflow names
#[allow(clippy::too_many_lines)]
pub fn workflow_update(
    live: &Entity,
    wanted: &super::classify::WorkflowView,
    agents: &BTreeMap<String, Uuid>,
) -> Result<Option<EntityUpdate>, String> {
    let current = live_workflow_view(live);
    // a workflow that already matches needs nothing
    if &current == wanted {
        return Ok(None);
    }
    // the entity's own fields
    let mut update = EntityUpdate {
        add_groups: wanted
            .groups
            .iter()
            .filter(|group| !current.groups.contains(group))
            .cloned()
            .collect(),
        remove_groups: current
            .groups
            .iter()
            .filter(|group| !wanted.groups.contains(group))
            .cloned()
            .collect(),
        ..EntityUpdate::default()
    };
    if current.description != wanted.description {
        match &wanted.description {
            Some(description) => update.description = Some(description.clone()),
            None => update.clear_description = true,
        }
    }
    // the agents to add and remove, and the entry agent
    let id_of = |name: &String| {
        agents
            .get(name)
            .copied()
            .ok_or_else(|| format!("agent {name} is not known"))
    };
    let mut add_agents = Vec::new();
    for name in wanted
        .agents
        .iter()
        .filter(|name| !current.agents.contains(name))
    {
        add_agents.push(id_of(name)?);
    }
    let mut remove_agents = Vec::new();
    for name in current
        .agents
        .iter()
        .filter(|name| !wanted.agents.contains(name))
    {
        remove_agents.push(id_of(name)?);
    }
    let (entry_agent, clear_entry_agent) = if current.entry_agent == wanted.entry_agent {
        (None, None)
    } else {
        match &wanted.entry_agent {
            Some(name) => (Some(id_of(name)?), None),
            None => (None, Some(true)),
        }
    };
    // the settings that differ
    let limits = if current.limits == wanted.limits {
        None
    } else {
        Some(
            serde_json::from_value(wanted.limits.clone())
                .map_err(|err| format!("the workflow limits are not valid: {err}"))?,
        )
    };
    let default_models = if current.default_models == wanted.default_models {
        None
    } else {
        Some(
            serde_json::from_value(wanted.default_models.clone())
                .map_err(|err| format!("the default models are not valid: {err}"))?,
        )
    };
    let model_auth = (current.model_auth != wanted.model_auth).then(|| ModelAuthSource {
        levels: auth_levels(&wanted.model_auth),
    });
    let auto_instance =
        (current.auto_instance != wanted.auto_instance).then_some(wanted.auto_instance);
    // the catalog's own fields go in the metadata update
    let metadata_changed = entry_agent.is_some()
        || clear_entry_agent.is_some()
        || limits.is_some()
        || default_models.is_some()
        || model_auth.is_some()
        || auto_instance.is_some()
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
    // nothing set means nothing changed
    let unchanged = update.add_groups.is_empty()
        && update.remove_groups.is_empty()
        && update.description.is_none()
        && !update.clear_description
        && update.metadata.is_none();
    Ok((!unchanged).then_some(update))
}

/// Put a pipeline back the way it was
///
/// # Arguments
///
/// * `api` - The API to write through
/// * `original` - The pipeline as it was before the import changed it
async fn undo_pipeline(api: &impl ImportApi, original: &Pipeline) -> Result<(), Error> {
    // read the pipeline as it is now and ask for what it was
    let live = api
        .pipelines(&original.group)
        .await?
        .into_iter()
        .find(|pipeline| pipeline.name == original.name)
        .ok_or_else(|| Error::new(format!("pipeline {} is gone", original.name)))?;
    let wanted = request_of(original);
    if let Some(update) = pipeline_update(&live, &wanted) {
        api.update_pipeline(&original.group, &original.name, &update)
            .await?;
    }
    Ok(())
}

/// Get the request that would create a pipeline as it is
///
/// # Arguments
///
/// * `pipeline` - The pipeline
#[must_use]
pub fn request_of(pipeline: &Pipeline) -> PipelineRequest {
    // the request holds the same fields, with the order as JSON
    PipelineRequest {
        group: pipeline.group.clone(),
        name: pipeline.name.clone(),
        order: serde_json::json!(pipeline.order),
        sla: Some(pipeline.sla),
        triggers: pipeline.triggers.clone(),
        description: pipeline.description.clone(),
        stage_tools: pipeline.stage_tools.clone(),
    }
}

/// Build the update that brings a pipeline in line with a request
///
/// Returns `None` when the pipeline already matches.
///
/// # Arguments
///
/// * `live` - The pipeline as it is now
/// * `wanted` - The request the pipeline should match
#[must_use]
pub fn pipeline_update(live: &Pipeline, wanted: &PipelineRequest) -> Option<PipelineUpdate> {
    let mut update = PipelineUpdate::default();
    // the order, when it differs
    if !wanted.compare_order(&live.order) {
        update.order = Some(wanted.order.clone());
    }
    // the SLA, which defaults to a week when the request sets none
    if let Some(sla) = wanted.sla
        && sla != live.sla
    {
        update.sla = Some(sla);
    }
    // the triggers to add or change, and the ones to remove
    for (name, trigger) in &wanted.triggers {
        if live.triggers.get(name) != Some(trigger) {
            update.triggers.insert(name.clone(), trigger.clone());
        }
    }
    update.remove_triggers = live
        .triggers
        .keys()
        .filter(|name| !wanted.triggers.contains_key(*name))
        .cloned()
        .collect();
    // the description
    if wanted.description != live.description {
        match &wanted.description {
            Some(description) => update.description = Some(description.clone()),
            None => update.clear_description = true,
        }
    }
    // the tool behind each stage
    for (stage, tool) in &wanted.stage_tools {
        if live.stage_tools.get(stage) != Some(tool) {
            update.add_stage_tools.insert(stage.clone(), *tool);
        }
    }
    let removed: BTreeSet<&String> = live
        .stage_tools
        .keys()
        .filter(|stage| !wanted.stage_tools.contains_key(*stage))
        .collect();
    update.remove_stage_tools = removed.into_iter().cloned().collect();
    // nothing set means nothing changed
    let unchanged = update.order.is_none()
        && update.sla.is_none()
        && update.triggers.is_empty()
        && update.remove_triggers.is_empty()
        && update.description.is_none()
        && !update.clear_description
        && update.add_stage_tools.is_empty()
        && update.remove_stage_tools.is_empty();
    (!unchanged).then_some(update)
}
