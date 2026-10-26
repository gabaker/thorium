//! Handles the pipeline edit and update commands
//!
//! Both read the pipeline as a [`PipelineView`], apply the change the user asked for (an editor,
//! `--file`, `--patch`, `--set`, `--unset`, or the typed `update` flags) and send one pipeline
//! update through the shared edit flow.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};
use thorium::models::{EventTrigger, Pipeline, PipelineUpdate};
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use super::view::PipelineView;
use crate::args::helpers::EditArgs;
use crate::args::pipelines::{EditPipeline, UpdatePipeline};
use crate::handlers::edit::{Editable, default_header, edit_with, update_with};
use crate::handlers::imports::merge::MergeablePipeline;
use crate::handlers::imports::update::calculate_pipeline_update_from_mergeable;
use crate::utils;
use crate::utils::pipelines::stage_tools::{self, StageToolRef};

/// Which pipeline to edit
#[derive(Debug, Clone)]
pub struct PipelineAt {
    /// The pipeline's group
    pub group: String,
    /// The pipeline's name
    pub name: String,
}

/// A pipeline and the tools behind its stages, by name
#[derive(Debug, Clone)]
pub struct PipelineData {
    /// The pipeline as stored
    pub pipeline: Pipeline,
    /// The tool behind each stage, by name
    pub tools: BTreeMap<String, StageToolRef>,
}

/// The pipeline resource for the shared edit flow
pub struct PipelineEdit;

/// Pick the stage tools of a view that differ from the stored ones
///
/// # Arguments
///
/// * `data` - The pipeline as fetched
/// * `view` - The edited view
fn changed_tools(data: &PipelineData, view: &PipelineView) -> BTreeMap<String, StageToolRef> {
    // a stage whose tool is named exactly as before keeps its stored id
    view.stage_tools
        .iter()
        .filter(|(stage, tool)| data.tools.get(*stage) != Some(*tool))
        .map(|(stage, tool)| (stage.clone(), tool.clone()))
        .collect()
}

/// Build the merge view an edited view stands for
///
/// # Arguments
///
/// * `data` - The pipeline as fetched
/// * `view` - The edited view
/// * `resolved` - The ids of the stage tools that changed
fn merge_view(
    data: &PipelineData,
    view: PipelineView,
    resolved: &BTreeMap<String, Uuid>,
) -> MergeablePipeline {
    // keep the stored id of every stage whose tool did not change
    let stage_tools = view
        .stage_tools
        .keys()
        .filter_map(|stage| {
            resolved
                .get(stage)
                .or_else(|| data.pipeline.stage_tools.get(stage))
                .map(|id| (stage.clone(), *id))
        })
        .collect();
    // an SLA left out of the view keeps the stored one
    MergeablePipeline {
        order: view.order,
        sla: view.sla.unwrap_or(data.pipeline.sla),
        triggers: view.triggers.into_iter().collect::<HashMap<_, _>>(),
        description: view.description,
        stage_tools,
    }
}

impl Editable for PipelineEdit {
    type Client = Thorium;
    type Target = PipelineAt;
    type Data = PipelineData;
    type View = PipelineView;
    type Update = PipelineUpdate;
    const KIND: &'static str = "Pipeline";
    const FIELD_ORDER: &'static [&'static str] =
        &["description", "sla", "order", "triggers", "stage_tools"];

    /// Read the pipeline and name the tools behind its stages
    async fn fetch(client: &Thorium, target: &PipelineAt) -> Result<PipelineData, Error> {
        // read the pipeline
        let pipeline = client.pipelines.get(&target.group, &target.name).await?;
        // name the tool behind each stage
        let tools = stage_tools::describe(client, &pipeline.stage_tools).await?;
        Ok(PipelineData { pipeline, tools })
    }

    /// Build the view of a pipeline
    fn to_view(data: &PipelineData) -> PipelineView {
        PipelineView::new(&data.pipeline, &data.tools)
    }

    /// The pipeline's group and name
    fn id(data: &PipelineData) -> String {
        utils::resource_id(&data.pipeline.group, &data.pipeline.name)
    }

    /// The pipeline's name
    fn name(data: &PipelineData) -> String {
        data.pipeline.name.clone()
    }

    /// Check the view before anything is sent
    fn validate(view: &PipelineView, _data: &PipelineData) -> Result<(), Error> {
        view.check()
    }

    /// Resolve the tools that changed and diff the view against the stored pipeline
    async fn calculate_update(
        client: &Thorium,
        data: &PipelineData,
        view: PipelineView,
    ) -> Result<Option<PipelineUpdate>, Error> {
        // find the stored ids of the tools the view changed
        let changed = changed_tools(data, &view);
        let resolved = stage_tools::resolve(client, &data.pipeline.group, &changed).await?;
        // diff the view against the pipeline
        let merged = merge_view(data, view, &resolved);
        calculate_pipeline_update_from_mergeable(data.pipeline.clone(), merged)
    }

    /// Send the update to Thorium
    async fn send_update(
        client: &Thorium,
        target: &PipelineAt,
        _data: &PipelineData,
        update: &PipelineUpdate,
    ) -> Result<(), Error> {
        client
            .pipelines
            .update(&target.group, &target.name, update)
            .await?;
        Ok(())
    }

    /// The comment block at the top of the editor buffer
    fn header(data: &PipelineData) -> String {
        // start from the standard header
        let mut header = default_header::<Self>(data);
        // explain how stages get their tools
        header
            .push_str("# stage_tools gives every stage its tool: {tool: NAME, version: LABEL}.\n");
        header.push_str("# A null version follows the tool's latest version.\n");
        header.push_str("# A new stage needs an entry in both order and stage_tools.\n");
        header
    }
}

/// Find the pipeline an edit or update names
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `pipeline` - The pipeline, as `PIPELINE`, `GROUP/PIPELINE` or `PIPELINE:GROUP`
/// * `group` - The group given with `--group` or as a second argument, if any
async fn find_target(
    thorium: &Thorium,
    pipeline: &str,
    group: Option<&str>,
) -> Result<PipelineAt, Error> {
    // split a qualified name
    let parsed = crate::args::pipelines::PipelineTarget::parse(pipeline)?;
    // a group given twice must agree
    if let (Some(given), Some(named)) = (group, parsed.group.as_deref())
        && given != named
    {
        return Err(Error::new(format!(
            "'{pipeline}' names group '{named}' but --group is '{given}'"
        )));
    }
    // search the user's groups when none was named
    let group = match group.map(ToOwned::to_owned).or(parsed.group) {
        Some(group) => group,
        None => utils::pipelines::find_pipeline_group(thorium, &parsed.pipeline).await?,
    };
    Ok(PipelineAt {
        group,
        name: parsed.pipeline,
    })
}

/// Edit a pipeline in an editor, or from `--file`, `--patch`, `--set` and `--unset`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl conf
/// * `cmd` - The edit pipeline command that was run
pub async fn edit(thorium: Thorium, conf: &CtlConf, cmd: &EditPipeline) -> Result<(), Error> {
    // find the pipeline
    let target = find_target(&thorium, &cmd.pipeline, cmd.group_name()).await?;
    // `-e` is the short name of `--editor`
    let mut args = cmd.edit.clone();
    if args.editor.is_none() {
        args.editor.clone_from(&cmd.editor_short);
    }
    // the flow prints its own result
    edit_with::<PipelineEdit>(&thorium, conf, &target, &args).await?;
    Ok(())
}

/// Turn the typed `update` flags into merge patches over the view
///
/// # Arguments
///
/// * `cmd` - The update pipeline command that was run
pub fn flag_patches(cmd: &UpdatePipeline) -> Result<Vec<String>, Error> {
    // each flag becomes one patch, applied in this order after any `--patch`
    let mut patches: Vec<Value> = Vec::new();
    if let Some(order) = &cmd.order {
        patches.push(json!({ "order": order.0 }));
    }
    if let Some(sla) = cmd.sla {
        patches.push(json!({ "sla": sla }));
    }
    if let Some(description) = &cmd.description {
        patches.push(json!({ "description": description }));
    }
    if cmd.clear_description {
        patches.push(json!({ "description": null }));
    }
    for raw in &cmd.add_trigger {
        let (name, trigger) = parse_trigger(raw)?;
        let trigger = serde_json::to_value(trigger)
            .map_err(|err| Error::new(format!("Failed to serialize trigger '{name}': {err}")))?;
        patches.push(json!({ "triggers": { name: trigger } }));
    }
    for name in &cmd.remove_trigger {
        patches.push(json!({ "triggers": { name: null } }));
    }
    for (stage, tool) in &cmd.stage_tool {
        // leaving the version out follows the tool's latest version
        let mut entry = json!({ "tool": tool.tool });
        if let Some(version) = &tool.version {
            entry["version"] = json!(version);
        } else {
            entry["version"] = Value::Null;
        }
        patches.push(json!({ "stage_tools": { stage: entry } }));
    }
    for stage in &cmd.remove_stage_tool {
        patches.push(json!({ "stage_tools": { stage: null } }));
    }
    Ok(patches.iter().map(Value::to_string).collect())
}

/// Parse a `NAME=JSON` or `NAME=@FILE` trigger argument
///
/// # Arguments
///
/// * `raw` - The argument
pub fn parse_trigger(raw: &str) -> Result<(String, EventTrigger), Error> {
    // the name is everything before the first =
    let Some((name, body)) = raw.split_once('=') else {
        return Err(Error::new(format!(
            "'{raw}' is not a trigger like NAME=JSON or NAME=@FILE"
        )));
    };
    if name.trim().is_empty() {
        return Err(Error::new(format!("'{raw}' names no trigger")));
    }
    // the body is JSON or YAML text, or a file holding it
    let trigger = match body.strip_prefix('@') {
        Some(path) => utils::spec_io::read_document(path, "--trigger")?,
        None => utils::spec_io::parse_document(body, "--trigger")?,
    };
    // it must be a trigger the API knows
    let trigger = serde_json::from_value::<EventTrigger>(trigger)
        .map_err(|err| Error::new(format!("Trigger '{name}' is not valid: {err}")))?;
    Ok((name.trim().to_owned(), trigger))
}

/// Update a pipeline from flags, `--file`, `--patch`, `--set` and `--unset`, never opening an editor
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update pipeline command that was run
pub async fn update(thorium: Thorium, cmd: &UpdatePipeline) -> Result<(), Error> {
    // something must be asked for
    if !cmd.has_flag_changes() && !cmd.edit.has_changes() {
        return Err(Error::new(format!(
            "Nothing to update for pipeline '{}': pass a flag such as --order, --sla or --stage-tool, or --file, --set, --unset or --patch",
            cmd.pipeline
        )));
    }
    // the typed flags join the patches the user wrote
    let mut args: EditArgs = cmd.edit.clone();
    args.patch.extend(flag_patches(cmd)?);
    // find the pipeline
    let target = find_target(&thorium, &cmd.pipeline, cmd.group.as_deref()).await?;
    // the flow prints its own result
    update_with::<PipelineEdit>(&thorium, &target, &args).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
