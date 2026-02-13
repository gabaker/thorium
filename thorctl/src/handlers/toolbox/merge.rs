//! Merge-conflict editor for resolving differences between existing Thorium
//! resources and incoming toolbox manifest configurations
//!
//! When a toolbox import encounters images or pipelines that already exist,
//! this module generates YAML files with git-style conflict markers, opens
//! the user's editor, validates the resolved YAML, and calculates the
//! appropriate update to apply.

use colored::Colorize;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use thorium::models::{
    BurstableResourcesUpdate, ChildFilters, Cleanup, Dependencies, EventTrigger, Image, ImageArgs,
    ImageBanUpdate, ImageLifetime, ImageRequest, ImageScaler, ImageUpdate, ImageVersion, Kvm,
    OutputCollection, OutputDisplayType, Pipeline, PipelineBanUpdate, PipelineRequest,
    PipelineUpdate, Resources, ResourcesRequest, ResourcesUpdate, SecurityContext,
    SecurityContextUpdate, SpawnLimits, Volume,
};
use thorium::{CtlConf, Error};
use uuid::Uuid;

use crate::utils::diff;
use crate::{
    calc_remove_add_map, calc_remove_add_vec, set_clear, set_modified, set_modified_new_opt,
    set_modified_opt,
};

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

// ─── Merge Conflict Generation ───────────────────────────────────────────────

/// Generate a YAML string with git-style merge conflict markers showing the
/// differences between two YAML representations
///
/// # Arguments
///
/// * `current_yaml` - The YAML string representing the current Thorium state
/// * `incoming_yaml` - The YAML string representing the incoming manifest state
fn generate_conflict_yaml(current_yaml: &str, incoming_yaml: &str) -> String {
    let diff = TextDiff::from_lines(current_yaml, incoming_yaml);
    let mut output = String::new();
    // buffer for collecting consecutive changed lines
    let mut current_lines: Vec<&str> = Vec::new();
    let mut incoming_lines: Vec<&str> = Vec::new();

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                // flush any buffered conflict before writing the equal line
                flush_conflict(&mut output, &mut current_lines, &mut incoming_lines);
                output.push_str(change.value());
            }
            ChangeTag::Delete => {
                current_lines.push(change.value());
            }
            ChangeTag::Insert => {
                incoming_lines.push(change.value());
            }
        }
    }
    // flush any remaining conflict at the end
    flush_conflict(&mut output, &mut current_lines, &mut incoming_lines);
    output
}

/// Flush buffered conflict lines into the output with git-style markers
fn flush_conflict(output: &mut String, current_lines: &mut Vec<&str>, incoming_lines: &mut Vec<&str>) {
    if current_lines.is_empty() && incoming_lines.is_empty() {
        return;
    }
    output.push_str("<<<<<<< Current (Thorium)\n");
    for line in current_lines.drain(..) {
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }
    output.push_str("=======\n");
    for line in incoming_lines.drain(..) {
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }
    output.push_str(">>>>>>> Incoming (Manifest)\n");
}

/// Check if the content contains any unresolved merge conflict markers.
/// Returns the line number of the first conflict marker found, if any.
fn find_conflict_markers(content: &str) -> Option<usize> {
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("<<<<<<<")
            || trimmed == "======="
            || trimmed.starts_with(">>>>>>>")
        {
            return Some(line_num + 1);
        }
    }
    None
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
pub fn prompt_merge_action(resource_type: &str, group: &str, name: &str) -> Result<MergeAction, Error> {
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

// ─── Editor Loop ─────────────────────────────────────────────────────────────

/// Open a merge conflict file in the user's editor with a validation loop.
/// Re-opens the editor if there are unresolved conflict markers or YAML
/// syntax errors, displaying helpful error messages with line numbers.
///
/// # Arguments
///
/// * `content` - The initial content with merge conflict markers
/// * `label` - A label for the temp file (e.g., "image-group-name")
/// * `editor` - The editor command to use
///
/// Returns the validated, resolved YAML content as a string
fn editor_loop(content: &str, label: &str, editor: &str) -> Result<String, Error> {
    // create a temp directory
    let temp_dir = std::env::temp_dir().join("thorium");
    std::fs::create_dir_all(&temp_dir).map_err(|err| {
        Error::new(format!(
            "Failed to create temporary directory '{}': {}",
            temp_dir.to_string_lossy(),
            err
        ))
    })?;
    let temp_path = temp_dir.join(format!("merge-{}-{}.yml", label, Uuid::new_v4()));
    // write initial content
    write_temp_file(&temp_path, content)?;
    loop {
        // open the editor
        let status = std::process::Command::new(editor)
            .arg(&temp_path)
            .status()
            .map_err(|err| {
                let _ = std::fs::remove_file(&temp_path);
                Error::new(format!("Unable to open editor '{editor}': {err}"))
            })?;
        if !status.success() {
            let _ = std::fs::remove_file(&temp_path);
            return Err(match status.code() {
                Some(code) => Error::new(format!("Editor '{editor}' exited with error code: {code}")),
                None => Error::new(format!("Editor '{editor}' exited with error!")),
            });
        }
        // read back the file
        let resolved = std::fs::read_to_string(&temp_path).map_err(|err| {
            let _ = std::fs::remove_file(&temp_path);
            Error::new(format!("Failed to read temporary file: {err}"))
        })?;
        // check for unresolved conflict markers
        if let Some(line) = find_conflict_markers(&resolved) {
            eprintln!(
                "{} Unresolved merge conflict marker found at line {}. Please resolve all conflicts before saving.",
                "Error:".bright_red().bold(),
                line.to_string().bright_yellow(),
            );
            eprintln!("Re-opening editor...\n");
            continue;
        }
        // validate YAML syntax by attempting to parse as serde_yaml::Value
        match serde_yaml::from_str::<serde_yaml::Value>(&resolved) {
            Ok(_) => {
                // valid YAML — clean up and return
                let _ = std::fs::remove_file(&temp_path);
                return Ok(resolved);
            }
            Err(err) => {
                // extract location info from the error
                let location = err.location();
                if let Some(loc) = location {
                    eprintln!(
                        "{} YAML syntax error at line {}, column {}: {}",
                        "Error:".bright_red().bold(),
                        loc.line().to_string().bright_yellow(),
                        loc.column().to_string().bright_yellow(),
                        err,
                    );
                } else {
                    eprintln!(
                        "{} YAML syntax error: {}",
                        "Error:".bright_red().bold(),
                        err,
                    );
                }
                eprintln!("Re-opening editor...\n");
            }
        }
    }
}

/// Write content to a temp file, creating or overwriting it
fn write_temp_file(path: &std::path::Path, content: &str) -> Result<(), Error> {
    let mut file = std::fs::File::create(path).map_err(|err| {
        Error::new(format!(
            "Failed to create temporary file '{}': {}",
            path.to_string_lossy(),
            err
        ))
    })?;
    file.write_all(content.as_bytes()).map_err(|err| {
        let _ = std::fs::remove_file(path);
        Error::new(format!("Failed to write temporary file: {err}"))
    })?;
    Ok(())
}

// ─── Image Merge ─────────────────────────────────────────────────────────────

/// Resolve an image merge conflict via the editor and return the resulting
/// image update, or None if the user's edits result in no changes
///
/// # Arguments
///
/// * `image` - The current image in Thorium
/// * `req` - The incoming image request from the manifest
/// * `conf` - The Thorctl config
/// * `editor_override` - Optional editor override from the CLI
pub fn merge_image_interactive(
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
    let conflict_yaml = generate_conflict_yaml(&current_yaml, &incoming_yaml);
    // open the editor
    let editor = editor_override.unwrap_or(&conf.default_editor);
    let label = format!("{}-{}", image.group, image.name);
    let resolved_yaml = editor_loop(&conflict_yaml, &label, editor)?;
    // deserialize the resolved YAML
    let resolved: MergeableImage = serde_yaml::from_str(&resolved_yaml).map_err(|err| {
        Error::new(format!(
            "Failed to parse resolved image YAML: {err}"
        ))
    })?;
    // calculate update from the current image to the resolved state
    Ok(calculate_image_update_from_mergeable(image.clone(), resolved))
}

/// Calculate an image update from the current image state and a resolved
/// mergeable image from the editor
fn calculate_image_update_from_mergeable(
    mut image: Image,
    mut resolved: MergeableImage,
) -> Option<ImageUpdate> {
    // check if nothing changed by comparing the resolved against current
    let current = MergeableImage::from(image.clone());
    let current_yaml = serde_yaml::to_string(&current).unwrap_or_default();
    let resolved_yaml = serde_yaml::to_string(&resolved).unwrap_or_default();
    if current_yaml == resolved_yaml {
        return None;
    }
    // build an ImageRequest-like struct from the resolved for diff calculation
    let (remove_volumes, add_volumes) =
        calc_remove_add_vec!(image.volumes, |vol| vol.name, resolved.volumes, |vol| vol);
    let (remove_env, add_env) = calc_remove_add_map!(image.env, resolved.env);
    Some(ImageUpdate {
        external: None,
        scaler: set_modified!(image.scaler, resolved.scaler),
        timeout: set_modified_opt!(image.timeout, resolved.timeout),
        resources: calculate_resource_update(image.resources, resolved.resources),
        spawn_limit: set_modified!(image.spawn_limit, resolved.spawn_limit),
        add_volumes,
        remove_volumes,
        add_env,
        remove_env,
        clear_version: set_clear!(image.version, resolved.version),
        version: set_modified_opt!(image.version, resolved.version),
        clear_image: set_clear!(image.image, resolved.image),
        image: set_modified_opt!(image.image, resolved.image),
        clear_lifetime: set_clear!(image.lifetime, resolved.lifetime),
        lifetime: set_modified_opt!(image.lifetime, resolved.lifetime),
        clear_description: set_clear!(image.description, resolved.description),
        args: diff::images::calculate_image_args_update(image.args, resolved.args),
        modifiers: set_modified_opt!(image.modifiers, resolved.modifiers),
        description: set_modified_opt!(image.description, resolved.description),
        security_context: {
            let resolved_sc = resolved.security_context.unwrap_or_default();
            diff::images::calculate_security_context_update(image.security_context, resolved_sc)
        },
        collect_logs: set_modified!(image.collect_logs, resolved.collect_logs),
        generator: set_modified!(image.generator, resolved.generator),
        dependencies: diff::images::calculate_dependencies_update(
            image.dependencies,
            resolved.dependencies,
        ),
        display_type: set_modified!(image.display_type, resolved.display_type),
        output_collection: diff::images::calculate_output_collection_update(
            image.output_collection,
            resolved.output_collection,
        ),
        child_filters: diff::images::calculate_child_filters_update(
            image.child_filters,
            resolved.child_filters,
        ),
        clean_up: diff::images::calculate_clean_up_update(image.clean_up, resolved.clean_up),
        kvm: diff::images::calculate_kvm_update(image.kvm, resolved.kvm),
        bans: ImageBanUpdate::default(),
        network_policies: diff::images::calculate_network_policies_update(
            image.network_policies,
            resolved.network_policies,
        ),
    })
}

// ─── Pipeline Merge ──────────────────────────────────────────────────────────

/// Resolve a pipeline merge conflict via the editor and return the resulting
/// pipeline update, or None if the user's edits result in no changes
///
/// # Arguments
///
/// * `pipeline` - The current pipeline in Thorium
/// * `req` - The incoming pipeline request from the manifest
/// * `conf` - The Thorctl config
/// * `editor_override` - Optional editor override from the CLI
pub fn merge_pipeline_interactive(
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
    let conflict_yaml = generate_conflict_yaml(&current_yaml, &incoming_yaml);
    // open the editor
    let editor = editor_override.unwrap_or(&conf.default_editor);
    let label = format!("{}-{}", pipeline.group, pipeline.name);
    let resolved_yaml = editor_loop(&conflict_yaml, &label, editor)?;
    // deserialize the resolved YAML
    let resolved: MergeablePipeline = serde_yaml::from_str(&resolved_yaml).map_err(|err| {
        Error::new(format!(
            "Failed to parse resolved pipeline YAML: {err}"
        ))
    })?;
    // calculate update from the current pipeline to the resolved state
    Ok(calculate_pipeline_update_from_mergeable(pipeline.clone(), resolved))
}

/// Calculate a pipeline update from the current pipeline state and a resolved
/// mergeable pipeline from the editor
fn calculate_pipeline_update_from_mergeable(
    pipeline: Pipeline,
    resolved: MergeablePipeline,
) -> Option<PipelineUpdate> {
    // check if nothing changed
    let current = MergeablePipeline::from(pipeline.clone());
    let current_yaml = serde_yaml::to_string(&current).unwrap_or_default();
    let resolved_yaml = serde_yaml::to_string(&resolved).unwrap_or_default();
    if current_yaml == resolved_yaml {
        return None;
    }
    // deserialize triggers back to the correct type
    let mut resolved_triggers: HashMap<String, EventTrigger> = resolved
        .triggers
        .into_iter()
        .filter_map(|(k, v)| {
            serde_json::from_value(v).ok().map(|trigger| (k, trigger))
        })
        .collect();
    let (remove_triggers, triggers) =
        calc_remove_add_map!(pipeline.triggers, resolved_triggers);
    Some(PipelineUpdate {
        order: if pipeline.order == resolved.order {
            None
        } else {
            Some(serde_json::to_value(&resolved.order).unwrap_or_default())
        },
        sla: set_modified_new_opt!(pipeline.sla, Some(resolved.sla)),
        triggers,
        remove_triggers,
        clear_description: set_clear!(pipeline.description, resolved.description),
        description: set_modified_opt!(pipeline.description, resolved.description),
        bans: PipelineBanUpdate::default(),
    })
}

// ─── Direct Update Calculation (for --force) ─────────────────────────────────

/// Calculate the updates for image resources (moved from update/images.rs)
#[allow(clippy::needless_pass_by_value)]
fn calculate_resource_update(
    old: Resources,
    new: ResourcesRequest,
) -> Option<ResourcesUpdate> {
    let new_cast = Resources::from(new.clone());
    if old == new_cast {
        return None;
    }
    let mut burstable = BurstableResourcesUpdate::default();
    if new_cast.burstable.cpu != old.burstable.cpu {
        burstable.cpu = Some(new.burstable.cpu);
    }
    if new_cast.burstable.memory != old.burstable.memory {
        burstable.memory = Some(new.burstable.memory);
    }
    let new: ResourcesRequest = new_cast.into();
    let old: ResourcesRequest = old.into();
    Some(ResourcesUpdate {
        cpu: set_modified!(old.cpu, new.cpu),
        memory: set_modified!(old.memory, new.memory),
        ephemeral_storage: set_modified!(old.ephemeral_storage, new.ephemeral_storage),
        nvidia_gpu: set_modified!(old.nvidia_gpu, new.nvidia_gpu),
        amd_gpu: set_modified!(old.amd_gpu, new.amd_gpu),
        burstable,
    })
}

/// Calculate the updates to a security context (moved from update/images.rs)
#[allow(clippy::needless_pass_by_value)]
fn calculate_security_context_update(
    old: SecurityContext,
    new: Option<SecurityContext>,
) -> Option<SecurityContextUpdate> {
    match new {
        Some(new) if old == new => None,
        Some(new) => Some(SecurityContextUpdate {
            user: set_modified_opt!(old.user, new.user),
            group: set_modified_opt!(old.group, new.group),
            allow_privilege_escalation: set_modified!(
                old.allow_privilege_escalation,
                new.allow_privilege_escalation
            ),
            clear_user: set_clear!(old.user, new.user),
            clear_group: set_clear!(old.group, new.group),
        }),
        None if old == SecurityContext::default() => None,
        None => Some(
            SecurityContextUpdate::default()
                .clear_user()
                .clear_group()
                .disallow_escalation(),
        ),
    }
}

/// Calculate what updates need to be made to an image based on the image's
/// current state and the request from the toolbox manifest (moved from update/images.rs)
pub fn calculate_image_update(mut image: Image, mut req: ImageRequest) -> Option<ImageUpdate> {
    if image == req {
        return None;
    }
    let (remove_volumes, add_volumes) =
        calc_remove_add_vec!(image.volumes, |vol| vol.name, req.volumes, |vol| vol);
    let (remove_env, add_env) = calc_remove_add_map!(image.env, req.env);
    let update = ImageUpdate {
        clear_version: set_clear!(image.version, req.version),
        clear_image: set_clear!(image.image, req.image),
        clear_lifetime: set_clear!(image.lifetime, req.lifetime),
        clear_description: set_clear!(image.description, req.description),
        version: set_modified_opt!(image.version, req.version),
        external: None,
        image: set_modified_opt!(image.image, req.image),
        scaler: set_modified!(image.scaler, req.scaler),
        lifetime: set_modified_opt!(image.lifetime, req.lifetime),
        timeout: set_modified_opt!(image.timeout, req.timeout),
        resources: calculate_resource_update(image.resources, req.resources),
        spawn_limit: set_modified!(image.spawn_limit, req.spawn_limit),
        add_volumes,
        remove_volumes,
        add_env,
        remove_env,
        args: diff::images::calculate_image_args_update(image.args, req.args),
        modifiers: set_modified_opt!(image.modifiers, req.modifiers),
        description: set_modified_opt!(image.description, req.description),
        security_context: calculate_security_context_update(
            image.security_context,
            req.security_context,
        ),
        collect_logs: set_modified!(image.collect_logs, req.collect_logs),
        generator: set_modified!(image.generator, req.generator),
        dependencies: diff::images::calculate_dependencies_update(
            image.dependencies,
            req.dependencies,
        ),
        display_type: set_modified!(image.display_type, req.display_type),
        output_collection: diff::images::calculate_output_collection_update(
            image.output_collection,
            req.output_collection,
        ),
        child_filters: diff::images::calculate_child_filters_update(
            image.child_filters,
            req.child_filters,
        ),
        clean_up: diff::images::calculate_clean_up_update(image.clean_up, req.clean_up),
        kvm: diff::images::calculate_kvm_update(image.kvm, req.kvm),
        bans: ImageBanUpdate::default(),
        network_policies: diff::images::calculate_network_policies_update(
            image.network_policies,
            req.network_policies,
        ),
    };
    Some(update)
}

/// Calculate what updates need to be made to a pipeline based on the pipeline's
/// current state and the request from the toolbox manifest (moved from update/pipelines.rs)
#[allow(clippy::needless_pass_by_value)]
pub fn calculate_pipeline_update(
    pipeline: Pipeline,
    mut req: PipelineRequest,
) -> Option<PipelineUpdate> {
    if pipeline == req {
        return None;
    }
    let (remove_triggers, triggers) = calc_remove_add_map!(pipeline.triggers, req.triggers);
    Some(PipelineUpdate {
        order: (!req.compare_order(&pipeline.order)).then_some(req.order),
        sla: set_modified_new_opt!(pipeline.sla, req.sla),
        triggers,
        remove_triggers,
        clear_description: set_clear!(pipeline.description, req.description),
        description: set_modified_opt!(pipeline.description, req.description),
        bans: PipelineBanUpdate::default(),
    })
}
