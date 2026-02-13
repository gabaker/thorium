//! Merge-conflict resolution for toolbox imports
//!
//! When a toolbox import encounters images or pipelines that already exist,
//! this module handles the interactive merge workflow: prompting the user for
//! an action, generating YAML with conflict markers, opening the editor,
//! and calculating the appropriate update to apply.

use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thorium::models::{
    ChildFilters, Cleanup, Dependencies, Image, ImageArgs, ImageLifetime, ImageRequest,
    ImageScaler, ImageUpdate, ImageVersion, Kvm, OutputCollection, OutputDisplayType, Pipeline,
    PipelineRequest, PipelineUpdate, ResourcesRequest, SecurityContext, SpawnLimits, Volume,
};
use thorium::{CtlConf, Error, Thorium};

use super::categorize::{CategorizedImage, CategorizedPipeline};
use super::editor;
use super::update;
use crate::handlers::progress::Bar;

// ─── Mergeable Structs ───────────────────────────────────────────────────────

/// An image converted to a common format for merge comparison and YAML editing.
/// Only contains editable fields — group, name, creator, runtime, used_by, and
/// bans are excluded since they are either identity fields or server-managed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeableImage {
    pub version: Option<ImageVersion>,
    pub scaler: ImageScaler,
    pub image: Option<String>,
    pub lifetime: Option<ImageLifetime>,
    pub timeout: Option<u64>,
    pub resources: ResourcesRequest,
    pub spawn_limit: SpawnLimits,
    pub env: HashMap<String, Option<String>>,
    pub volumes: Vec<Volume>,
    pub args: ImageArgs,
    pub modifiers: Option<String>,
    pub description: Option<String>,
    pub security_context: Option<SecurityContext>,
    pub collect_logs: bool,
    pub generator: bool,
    pub dependencies: Dependencies,
    pub display_type: OutputDisplayType,
    pub output_collection: OutputCollection,
    pub child_filters: ChildFilters,
    pub clean_up: Option<Cleanup>,
    pub kvm: Option<Kvm>,
    pub network_policies: HashSet<String>,
}

impl From<Image> for MergeableImage {
    fn from(image: Image) -> Self {
        Self {
            version: image.version,
            scaler: image.scaler,
            image: image.image,
            lifetime: image.lifetime,
            timeout: image.timeout,
            resources: image.resources.into(),
            spawn_limit: image.spawn_limit,
            env: image.env,
            volumes: image.volumes,
            args: image.args,
            modifiers: image.modifiers,
            description: image.description,
            security_context: Some(image.security_context),
            collect_logs: image.collect_logs,
            generator: image.generator,
            dependencies: image.dependencies,
            display_type: image.display_type,
            output_collection: image.output_collection,
            child_filters: image.child_filters,
            clean_up: image.clean_up,
            kvm: image.kvm,
            network_policies: image.network_policies,
        }
    }
}

impl From<ImageRequest> for MergeableImage {
    fn from(req: ImageRequest) -> Self {
        Self {
            version: req.version,
            scaler: req.scaler,
            image: req.image,
            lifetime: req.lifetime,
            timeout: req.timeout,
            resources: req.resources,
            spawn_limit: req.spawn_limit,
            env: req.env,
            volumes: req.volumes,
            args: req.args,
            modifiers: req.modifiers,
            description: req.description,
            security_context: req.security_context,
            collect_logs: req.collect_logs,
            generator: req.generator,
            dependencies: req.dependencies,
            display_type: req.display_type,
            output_collection: req.output_collection,
            child_filters: req.child_filters,
            clean_up: req.clean_up,
            kvm: req.kvm,
            network_policies: req.network_policies,
        }
    }
}

/// A pipeline converted to a common format for merge comparison and YAML editing.
/// Only contains editable fields — group, name, creator, and bans are excluded.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeablePipeline {
    pub order: Vec<Vec<String>>,
    pub sla: u64,
    pub triggers: HashMap<String, serde_json::Value>,
    pub description: Option<String>,
}

impl From<Pipeline> for MergeablePipeline {
    fn from(pipeline: Pipeline) -> Self {
        Self {
            order: pipeline.order,
            sla: pipeline.sla,
            triggers: pipeline
                .triggers
                .into_iter()
                .map(|(k, v)| (k, serde_json::to_value(v).unwrap_or_default()))
                .collect(),
            description: pipeline.description,
        }
    }
}

impl From<PipelineRequest> for MergeablePipeline {
    fn from(req: PipelineRequest) -> Self {
        // deserialize the order from the flexible Value format to Vec<Vec<String>>
        let order: Vec<Vec<String>> = req
            .deserialize_image_order()
            .unwrap_or_default()
            .into_iter()
            .map(|inner| inner.into_iter().map(String::from).collect())
            .collect();
        Self {
            order,
            sla: req.sla.unwrap_or(604_800),
            triggers: req
                .triggers
                .into_iter()
                .map(|(k, v)| (k, serde_json::to_value(v).unwrap_or_default()))
                .collect(),
            description: req.description,
        }
    }
}

// ─── Per-Resource Prompt ─────────────────────────────────────────────────────

/// The action the user wants to take for a changed resource
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MergeAction {
    /// Open the editor to review and resolve conflicts
    Edit,
    /// Keep the existing configuration unchanged
    Skip,
    /// Accept all incoming changes from the manifest
    Apply,
    /// Stop processing remaining resources
    Quit,
}

/// Prompt the user for what action to take on a changed resource
///
/// # Arguments
///
/// * `resource_type` - Either "Image" or "Pipeline"
/// * `group` - The group the resource is in
/// * `name` - The name of the resource
fn prompt_merge_action(resource_type: &str, group: &str, name: &str) -> Result<MergeAction, Error> {
    println!(
        "\n{} '{}:{}' has changes:",
        resource_type.bright_yellow(),
        group.bright_blue(),
        name.bright_blue(),
    );
    let items = &[
        "Edit   - Open editor to review and resolve conflicts",
        "Skip   - Keep the existing configuration unchanged",
        "Apply  - Accept all incoming changes from the manifest",
        "Quit   - Stop processing remaining resources",
    ];
    let selection = dialoguer::Select::new()
        .items(items)
        .default(0)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
    Ok(match selection {
        0 => MergeAction::Edit,
        1 => MergeAction::Skip,
        2 => MergeAction::Apply,
        3 => MergeAction::Quit,
        _ => MergeAction::Skip,
    })
}

// ─── Single-Resource Interactive Merge ───────────────────────────────────────

/// Resolve an image merge conflict via the editor and return the resulting
/// image update, or None if the user's edits result in no changes or they
/// cancelled
///
/// # Arguments
///
/// * `image` - The current image in Thorium
/// * `req` - The incoming image request from the manifest
/// * `conf` - The Thorctl config
/// * `editor_override` - Optional editor override from the CLI
fn merge_image_interactive(
    image: &Image,
    req: &ImageRequest,
    conf: &CtlConf,
    editor_override: Option<&str>,
) -> Result<Option<ImageUpdate>, Error> {
    let current = MergeableImage::from(image.clone());
    let incoming = MergeableImage::from(req.clone());
    // serialize both to YAML
    let current_yaml = serde_yaml::to_string(&current)
        .map_err(|err| Error::new(format!("Failed to serialize current image to YAML: {err}")))?;
    let incoming_yaml = serde_yaml::to_string(&incoming)
        .map_err(|err| Error::new(format!("Failed to serialize incoming image to YAML: {err}")))?;
    // generate the conflict YAML
    let conflict_yaml = editor::generate_conflict_yaml(&current_yaml, &incoming_yaml);
    // open the editor
    let editor_cmd = editor_override.unwrap_or(&conf.default_editor);
    let label = format!("{}-{}", image.group, image.name);
    let resolved_yaml = match editor::editor_loop(&conflict_yaml, &label, editor_cmd)? {
        Some(yaml) => yaml,
        None => return Ok(None),
    };
    // deserialize the resolved YAML
    let resolved: MergeableImage = serde_yaml::from_str(&resolved_yaml).map_err(|err| {
        Error::new(format!(
            "Failed to parse resolved image YAML: {err}"
        ))
    })?;
    // calculate update from the current image to the resolved state
    Ok(update::calculate_image_update_from_mergeable(image.clone(), resolved))
}

/// Resolve a pipeline merge conflict via the editor and return the resulting
/// pipeline update, or None if the user's edits result in no changes or they
/// cancelled
///
/// # Arguments
///
/// * `pipeline` - The current pipeline in Thorium
/// * `req` - The incoming pipeline request from the manifest
/// * `conf` - The Thorctl config
/// * `editor_override` - Optional editor override from the CLI
fn merge_pipeline_interactive(
    pipeline: &Pipeline,
    req: &PipelineRequest,
    conf: &CtlConf,
    editor_override: Option<&str>,
) -> Result<Option<PipelineUpdate>, Error> {
    let current = MergeablePipeline::from(pipeline.clone());
    let incoming = MergeablePipeline::from(req.clone());
    // serialize both to YAML
    let current_yaml = serde_yaml::to_string(&current)
        .map_err(|err| Error::new(format!("Failed to serialize current pipeline to YAML: {err}")))?;
    let incoming_yaml = serde_yaml::to_string(&incoming)
        .map_err(|err| Error::new(format!("Failed to serialize incoming pipeline to YAML: {err}")))?;
    // generate the conflict YAML
    let conflict_yaml = editor::generate_conflict_yaml(&current_yaml, &incoming_yaml);
    // open the editor
    let editor_cmd = editor_override.unwrap_or(&conf.default_editor);
    let label = format!("{}-{}", pipeline.group, pipeline.name);
    let resolved_yaml = match editor::editor_loop(&conflict_yaml, &label, editor_cmd)? {
        Some(yaml) => yaml,
        None => return Ok(None),
    };
    // deserialize the resolved YAML
    let resolved: MergeablePipeline = serde_yaml::from_str(&resolved_yaml).map_err(|err| {
        Error::new(format!(
            "Failed to parse resolved pipeline YAML: {err}"
        ))
    })?;
    // calculate update from the current pipeline to the resolved state
    Ok(update::calculate_pipeline_update_from_mergeable(pipeline.clone(), resolved))
}

// ─── Batch Interactive Merge ─────────────────────────────────────────────────

/// Interactively handle existing images that have changes, prompting the user
/// for each one
pub async fn interactive_merge_images(
    thorium: &Thorium,
    existing_images: Vec<&CategorizedImage>,
    conf: &CtlConf,
    editor_override: Option<&str>,
    progress: &Bar,
) -> Result<(), Error> {
    // filter to only images with actual changes
    let changed_images: Vec<_> = existing_images
        .into_iter()
        .filter(|img| {
            img.existing
                .as_ref()
                .is_some_and(|existing| existing != &img.request)
        })
        .collect();
    if changed_images.is_empty() {
        return Ok(());
    }
    for img in changed_images {
        let existing = img.existing.as_ref().unwrap();
        // suspend the progress bar for interactive prompts
        let action = progress.suspend(|| {
            prompt_merge_action("Image", &img.request.group, &img.request.name)
        })?;
        match action {
            MergeAction::Edit => {
                // open the editor for this image
                let image_update = progress.suspend(|| {
                    merge_image_interactive(
                        existing,
                        &img.request,
                        conf,
                        editor_override,
                    )
                })?;
                if let Some(image_update) = image_update {
                    thorium
                        .images
                        .update(&img.request.group, &img.request.name, &image_update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating image '{}:{}': {}",
                                img.image_name, img.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Image".bright_green(),
                        format!("'{}:{}'", img.request.group, img.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                } else {
                    println!(
                        "{} No changes detected for image '{}:{}'",
                        "Skipped:".bright_blue(),
                        img.request.group,
                        img.request.name
                    );
                }
            }
            MergeAction::Skip => {
                progress.info_anonymous(format!(
                    "Skipping image '{}:{}'",
                    img.image_name.bright_yellow(),
                    img.version_name.bright_yellow()
                ));
            }
            MergeAction::Apply => {
                if let Some(image_update) =
                    update::calculate_image_update(existing.clone(), img.request.clone())
                {
                    thorium
                        .images
                        .update(&img.request.group, &img.request.name, &image_update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating image '{}:{}': {}",
                                img.image_name, img.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Image".bright_green(),
                        format!("'{}:{}'", img.request.group, img.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                }
            }
            MergeAction::Quit => {
                println!("Stopping further resource processing.");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Interactively handle existing pipelines that have changes, prompting the user
/// for each one
pub async fn interactive_merge_pipelines(
    thorium: &Thorium,
    existing_pipelines: Vec<&CategorizedPipeline>,
    conf: &CtlConf,
    editor_override: Option<&str>,
    progress: &Bar,
) -> Result<(), Error> {
    // filter to only pipelines with actual changes
    let changed_pipelines: Vec<_> = existing_pipelines
        .into_iter()
        .filter(|pipe| {
            pipe.existing
                .as_ref()
                .is_some_and(|existing| existing != &pipe.request)
        })
        .collect();
    if changed_pipelines.is_empty() {
        return Ok(());
    }
    for pipe in changed_pipelines {
        let existing = pipe.existing.as_ref().unwrap();
        let action = progress.suspend(|| {
            prompt_merge_action("Pipeline", &pipe.request.group, &pipe.request.name)
        })?;
        match action {
            MergeAction::Edit => {
                let pipeline_update = progress.suspend(|| {
                    merge_pipeline_interactive(
                        existing,
                        &pipe.request,
                        conf,
                        editor_override,
                    )
                })?;
                if let Some(pipeline_update) = pipeline_update {
                    thorium
                        .pipelines
                        .update(&pipe.request.group, &pipe.request.name, &pipeline_update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating pipeline '{}:{}': {}",
                                pipe.pipeline_name, pipe.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Pipeline".bright_green(),
                        format!("'{}:{}'", pipe.request.group, pipe.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                } else {
                    println!(
                        "{} No changes detected for pipeline '{}:{}'",
                        "Skipped:".bright_blue(),
                        pipe.request.group,
                        pipe.request.name
                    );
                }
            }
            MergeAction::Skip => {
                progress.info_anonymous(format!(
                    "Skipping pipeline '{}:{}'",
                    pipe.pipeline_name.bright_yellow(),
                    pipe.version_name.bright_yellow()
                ));
            }
            MergeAction::Apply => {
                if let Some(pipeline_update) =
                    update::calculate_pipeline_update(existing.clone(), pipe.request.clone())
                {
                    thorium
                        .pipelines
                        .update(&pipe.request.group, &pipe.request.name, &pipeline_update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating pipeline '{}:{}': {}",
                                pipe.pipeline_name, pipe.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Pipeline".bright_green(),
                        format!("'{}:{}'", pipe.request.group, pipe.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                }
            }
            MergeAction::Quit => {
                println!("Stopping further resource processing.");
                return Ok(());
            }
        }
    }
    Ok(())
}
