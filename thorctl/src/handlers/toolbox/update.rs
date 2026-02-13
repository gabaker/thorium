//! Update calculation for images and pipelines
//!
//! Pure functions that compute `ImageUpdate`/`PipelineUpdate` structs by
//! diffing the current Thorium state against an incoming or editor-resolved
//! state. Used by both force-update and interactive merge paths.

use std::collections::HashMap;
use thorium::models::{
    BurstableResourcesUpdate, EventTrigger, Image, ImageBanUpdate, ImageRequest, ImageUpdate,
    Pipeline, PipelineBanUpdate, PipelineRequest, PipelineUpdate, Resources, ResourcesRequest,
    ResourcesUpdate, SecurityContext, SecurityContextUpdate,
};

use super::merge::MergeableImage;
use super::merge::MergeablePipeline;
use crate::utils::diff;
use crate::{
    calc_remove_add_map, calc_remove_add_vec, set_clear, set_modified, set_modified_new_opt,
    set_modified_opt,
};

// ─── Image Update Calculation ────────────────────────────────────────────────

/// Calculate an image update from the current image state and a resolved
/// mergeable image from the editor
pub fn calculate_image_update_from_mergeable(
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
            calculate_security_context_update(image.security_context, Some(resolved_sc))
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

/// Calculate what updates need to be made to an image based on the image's
/// current state and the request from the toolbox manifest
#[allow(clippy::needless_pass_by_value)]
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

// ─── Pipeline Update Calculation ─────────────────────────────────────────────

/// Calculate a pipeline update from the current pipeline state and a resolved
/// mergeable pipeline from the editor
pub fn calculate_pipeline_update_from_mergeable(
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

/// Calculate what updates need to be made to a pipeline based on the pipeline's
/// current state and the request from the toolbox manifest
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

// ─── Resource & Security Context Updates ─────────────────────────────────────

/// Calculate the updates for image resources
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

/// Calculate the updates to a security context
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
