//! Handles the pipeline create command

use serde_json::json;
use thorium::models::PipelineRequest;
use thorium::{Error, Thorium};

use super::edit::parse_trigger;
use super::view::PipelineView;
use crate::args::pipelines::CreatePipeline;
use crate::utils::output::OutputFormat;
use crate::utils::pipelines::stage_tools;
use crate::utils::{self, spec_io};

/// Build the view of the pipeline a create command describes
///
/// The `--spec` document comes first, then the flags add to it or replace what it holds.
///
/// # Arguments
///
/// * `cmd` - The create pipeline command that was run
pub fn build_view(cmd: &CreatePipeline) -> Result<PipelineView, Error> {
    // start from the spec document or from nothing
    let mut view = match &cmd.spec {
        Some(source) => {
            let document = spec_io::read_document(source, "--spec")?;
            serde_json::from_value::<PipelineView>(document)
                .map_err(|err| Error::new(format!("Invalid pipeline spec: {err}")))?
        }
        None => PipelineView {
            description: None,
            sla: None,
            order: Vec::new(),
            triggers: std::collections::BTreeMap::new(),
            stage_tools: std::collections::BTreeMap::new(),
        },
    };
    // each --stage is one step of the order and replaces the spec's order
    if !cmd.stage.is_empty() {
        view.order = cmd
            .stage
            .iter()
            .flat_map(|stage| stage.0.iter().cloned())
            .collect();
    }
    // stage tools from the flags replace the spec's tool for the same stage
    for (stage, tool) in &cmd.stage_tool {
        view.stage_tools.insert(stage.clone(), tool.clone());
    }
    // scalar flags replace the spec's values
    if let Some(sla) = cmd.sla {
        view.sla = Some(sla);
    }
    if let Some(description) = &cmd.description {
        view.description = Some(description.clone());
    }
    // triggers from the flags are added to the spec's
    for raw in &cmd.trigger {
        let (name, trigger) = parse_trigger(raw)?;
        view.triggers.insert(name, trigger);
    }
    Ok(view)
}

/// Create a pipeline
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create pipeline command that was run
pub async fn create(thorium: &Thorium, cmd: &CreatePipeline) -> Result<(), Error> {
    // describe the pipeline and check it before anything is sent
    let view = build_view(cmd)?;
    view.check()?;
    // find the id of each stage's tool in the pipeline's group
    let ids = stage_tools::resolve(thorium, &cmd.group, &view.stage_tools).await?;
    // build the request
    let mut req = PipelineRequest::new(&cmd.group, &cmd.name, json!(view.order));
    req.sla = view.sla;
    req.description.clone_from(&view.description);
    req.triggers = view.triggers.clone().into_iter().collect();
    req.stage_tools = ids;
    // a dry run shows the pipeline and stops
    let id = utils::resource_id(&cmd.group, &cmd.name);
    if cmd.dry_run {
        return print_dry_run(&id, &view, cmd.format.format);
    }
    // create the pipeline
    thorium.pipelines.create(&req).await?;
    // print the new pipeline in the requested format
    match cmd.format.format {
        Some(format @ (OutputFormat::Json | OutputFormat::Yaml)) => {
            let created = thorium.pipelines.get(&cmd.group, &cmd.name).await?;
            let document = serde_json::to_value(&created)
                .map_err(|err| Error::new(format!("Failed to serialize the pipeline: {err}")))?;
            print_document(&document, format)
        }
        _ => {
            println!("Created pipeline '{id}'");
            Ok(())
        }
    }
}

/// Print a document in a machine format
///
/// # Arguments
///
/// * `document` - The document to print
/// * `format` - Json or yaml
fn print_document(document: &serde_json::Value, format: OutputFormat) -> Result<(), Error> {
    // render the document in the format asked for
    let text = if format == OutputFormat::Yaml {
        spec_io::render_yaml(document)?
    } else {
        spec_io::render_json(document)?
    };
    print!("{text}");
    Ok(())
}

/// Print the pipeline a dry run would create
///
/// # Arguments
///
/// * `id` - The group and name of the pipeline
/// * `view` - The pipeline that would be created
/// * `format` - The format to print in
fn print_dry_run(id: &str, view: &PipelineView, format: Option<OutputFormat>) -> Result<(), Error> {
    // the pipeline as a document
    let pipeline = serde_json::to_value(view)
        .map_err(|err| Error::new(format!("Failed to serialize the pipeline: {err}")))?;
    match format {
        // machine formats print one document
        Some(format @ (OutputFormat::Json | OutputFormat::Yaml)) => print_document(
            &json!({"kind": "pipeline", "id": id, "dry_run": true, "pipeline": pipeline}),
            format,
        ),
        // people get a sentence and the pipeline
        _ => {
            println!("Dry run: pipeline '{id}' was not created. It would have:");
            print!("{}", spec_io::render_yaml(&pipeline)?);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
