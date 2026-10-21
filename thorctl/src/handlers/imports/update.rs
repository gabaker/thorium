//! Update calculation for images and pipelines
//!
//! Pure functions that compute `ImageUpdate`/`PipelineUpdate` structs by
//! diffing the current Thorium state against an incoming request or an
//! editor-resolved view. Every path goes through the normalized
//! [`MergeableImage`]/[`MergeablePipeline`] views, and a `None` result is the one
//! definition of "unchanged" shared by the confirmation screen, the interactive
//! merge prompt and editor, `--overwrite`, `--skip-conflicts`, rollback, `edit`,
//! and `toolbox diff`.

use serde::Serialize;
use thorium::Error;
use thorium::models::{
    BurstableResourcesUpdate, Image, ImageBanUpdate, ImageRequest, ImageUpdate, Pipeline,
    PipelineBanUpdate, PipelineRequest, PipelineUpdate, Resources, ResourcesRequest,
    ResourcesUpdate,
};

use super::merge::{self, MergeableImage, MergeablePipeline};
use crate::utils::diff;
use crate::{calc_remove_add_map, calc_remove_add_vec, set_clear, set_modified, set_modified_opt};

/// Whether an update would change nothing when applied
///
/// A field-by-field diff can still come out empty when two views differ only in
/// something the update API can't express; reporting that as a change would
/// prompt on every import without ever converging.
///
/// # Arguments
///
/// * `update` - The update to check
fn is_noop<T: Serialize + Default>(update: &T) -> bool {
    // serializing plain update data can't fail; a failure is treated as a change
    match (
        serde_json::to_value(update),
        serde_json::to_value(T::default()),
    ) {
        (Ok(update), Ok(noop)) => update == noop,
        _ => false,
    }
}

// ─── Image Update Calculation ────────────────────────────────────────────────

/// Calculate the update from an image to a resolved view of it
///
/// Returns `None` when the editable fields of both views match or the diff comes
/// out empty.
///
/// # Arguments
///
/// * `image` - The current image as stored in Thorium
/// * `resolved` - The target view (incoming request or editor result)
fn image_update_from_view(image: Image, mut resolved: MergeableImage) -> Option<ImageUpdate> {
    // keep the stored resources and security context; the view holds request forms
    let old_resources = image.resources;
    let old_security_context = image.security_context.clone();
    // build the current view in the same normalized form as the target
    let mut current = MergeableImage::from(image);
    // normalize the target too, since an editor result may be in any order
    resolved.normalize();
    // keep values the update API can't clear or set so they never read as changes
    resolved.retain_unsettable(&current);
    // nothing to do when the editable fields already match
    if current.same_editable(&resolved) {
        return None;
    }
    // diff volumes by name into the keep/add and remove sets the update expects
    let (remove_volumes, add_volumes) =
        calc_remove_add_vec!(current.volumes, |vol| vol.name, resolved.volumes, |vol| vol);
    // diff env vars into the set-to-apply and the keys to remove
    let (remove_env, add_env) = calc_remove_add_map!(current.env, resolved.env);
    // modifiers are cleared by sending an empty string
    let modifiers = if current.modifiers.is_some() && resolved.modifiers.is_none() {
        Some(String::new())
    } else {
        set_modified_opt!(current.modifiers, resolved.modifiers)
    };
    // build the update field by field, each helper emitting a change only when the
    // current and resolved values actually differ
    let update = ImageUpdate {
        external: None,
        scaler: set_modified!(current.scaler, resolved.scaler),
        timeout: set_modified_opt!(current.timeout, resolved.timeout),
        resources: calculate_resource_update(old_resources, resolved.resources),
        spawn_limit: set_modified!(current.spawn_limit, resolved.spawn_limit),
        add_volumes,
        remove_volumes,
        add_env,
        remove_env,
        clear_version: set_clear!(current.version, resolved.version),
        version: set_modified_opt!(current.version, resolved.version),
        clear_image: set_clear!(current.image, resolved.image),
        image: set_modified_opt!(current.image, resolved.image),
        clear_lifetime: set_clear!(current.lifetime, resolved.lifetime),
        lifetime: set_modified_opt!(current.lifetime, resolved.lifetime),
        clear_description: set_clear!(current.description, resolved.description),
        description: set_modified_opt!(current.description, resolved.description),
        args: diff::images::calculate_image_args_update(current.args, resolved.args),
        modifiers,
        // a security context removed in the editor resets to the server default
        security_context: diff::images::calculate_security_context_update(
            old_security_context,
            resolved.security_context.unwrap_or_default(),
        ),
        collect_logs: set_modified!(current.collect_logs, resolved.collect_logs),
        generator: set_modified!(current.generator, resolved.generator),
        dependencies: diff::images::calculate_dependencies_update(
            current.dependencies,
            resolved.dependencies,
        ),
        display_type: set_modified!(current.display_type, resolved.display_type),
        output_collection: diff::images::calculate_output_collection_update(
            current.output_collection,
            resolved.output_collection,
        ),
        child_filters: diff::images::calculate_child_filters_update(
            current.child_filters,
            resolved.child_filters,
        ),
        clean_up: diff::images::calculate_clean_up_update(current.clean_up, resolved.clean_up),
        kvm: diff::images::calculate_kvm_update(current.kvm, resolved.kvm),
        // bans are non-editable here (marked `*bans*` in the editor view); they're
        // managed via the dedicated `images bans` subcommands
        bans: ImageBanUpdate::default(),
        network_policies: diff::images::calculate_network_policies_update(
            current.network_policies,
            resolved.network_policies,
        ),
    };
    // an update that changes nothing is not a change
    (!is_noop(&update)).then_some(update)
}

/// Calculate an image update from the current image state and a resolved
/// mergeable image from the editor
///
/// The resolved view means exactly what it says, except for the fields the update
/// API can't clear or set (see [`MergeableImage::retain_unsettable`]).
///
/// # Arguments
///
/// * `image` - The current image as stored in Thorium
/// * `resolved` - The editor-resolved [`MergeableImage`] to diff against
// the Result matches the fallible `EditableEntity::calculate_update` interface
#[allow(clippy::unnecessary_wraps)]
pub fn calculate_image_update_from_mergeable(
    image: Image,
    resolved: MergeableImage,
) -> Result<Option<ImageUpdate>, Error> {
    Ok(image_update_from_view(image, resolved))
}

/// Calculate what updates need to be made to an image based on the image's
/// current state and an incoming import request
///
/// Omitted request fields are resolved against the existing image first (see
/// [`merge::incoming_image_view`]): fields the server defaults on create
/// (`security_context`, an empty `network_policies`) or that the update API can't
/// clear (`timeout`, `kvm`) keep the existing value, while every other omitted
/// field clears it. Descriptions are compared and sent with trailing whitespace
/// trimmed, and set-like fields are compared as sets.
///
/// # Arguments
///
/// * `image` - The current image as stored in Thorium
/// * `req` - The incoming [`ImageRequest`]
pub fn calculate_image_update(image: Image, req: ImageRequest) -> Option<ImageUpdate> {
    // resolve omitted fields against the existing image before diffing
    let incoming = merge::incoming_image_view(&image, req);
    image_update_from_view(image, incoming)
}

// ─── Pipeline Update Calculation ─────────────────────────────────────────────

/// Calculate the update from a pipeline to a resolved view of it
///
/// Returns `None` when both views match or the diff comes out empty.
///
/// # Arguments
///
/// * `pipeline` - The current pipeline as stored in Thorium
/// * `resolved` - The target view (incoming request or editor result)
fn pipeline_update_from_view(
    pipeline: Pipeline,
    mut resolved: MergeablePipeline,
) -> Option<PipelineUpdate> {
    // keep the typed triggers for the diff below
    let current_triggers = pipeline.triggers.clone();
    // build the current view in the same normalized form as the target
    let current = MergeablePipeline::from(pipeline);
    // normalize the target too, since an editor result may carry untrimmed text
    resolved.normalize();
    // nothing to do when the views already match
    if current.same_content(&resolved) {
        return None;
    }
    // diff triggers into the set-to-apply and the keys to remove
    let (remove_triggers, triggers) = calc_remove_add_map!(current_triggers, resolved.triggers);
    // only send a new order when the stages actually differ
    let order = (current.order != resolved.order).then(|| serde_json::Value::from(resolved.order));
    // build the update from the diffs computed above
    let update = PipelineUpdate {
        order,
        sla: set_modified!(current.sla, resolved.sla),
        triggers,
        remove_triggers,
        clear_description: set_clear!(current.description, resolved.description),
        description: set_modified_opt!(current.description, resolved.description),
        // bans are managed via the dedicated pipeline bans subcommands, not imports
        bans: PipelineBanUpdate::default(),
    };
    // an update that changes nothing is not a change
    (!is_noop(&update)).then_some(update)
}

/// Calculate a pipeline update from the current pipeline state and a resolved
/// mergeable pipeline from the editor
///
/// Triggers in the view are typed, so an invalid trigger is rejected by the editor
/// loop before this is called.
///
/// # Arguments
///
/// * `pipeline` - The current pipeline as stored in Thorium
/// * `resolved` - The editor-resolved [`MergeablePipeline`] to diff against
// the Result matches the fallible `EditableEntity::calculate_update` interface
#[allow(clippy::unnecessary_wraps)]
pub fn calculate_pipeline_update_from_mergeable(
    pipeline: Pipeline,
    resolved: MergeablePipeline,
) -> Result<Option<PipelineUpdate>, Error> {
    Ok(pipeline_update_from_view(pipeline, resolved))
}

/// Calculate what updates need to be made to a pipeline based on the pipeline's
/// current state and an incoming import request
///
/// An omitted `sla` keeps the existing SLA (see [`merge::incoming_pipeline_view`]);
/// stage order is compared stage by stage including the stage count, and
/// descriptions are compared and sent with trailing whitespace trimmed. A request
/// order that isn't a valid list of stages is always sent unchanged so the server
/// reports why it's invalid.
///
/// # Arguments
///
/// * `pipeline` - The current pipeline as stored in Thorium
/// * `req` - The incoming [`PipelineRequest`]
pub fn calculate_pipeline_update(
    pipeline: Pipeline,
    req: PipelineRequest,
) -> Option<PipelineUpdate> {
    // an order that can't be normalized is sent as-is for the server to reject
    if req.deserialize_image_order().is_err() {
        let raw_order = req.order.clone();
        let incoming = merge::incoming_pipeline_view(&pipeline, req);
        let mut update = pipeline_update_from_view(pipeline, incoming).unwrap_or_default();
        update.order = Some(raw_order);
        return Some(update);
    }
    // resolve an omitted SLA against the existing pipeline before diffing
    let incoming = merge::incoming_pipeline_view(&pipeline, req);
    pipeline_update_from_view(pipeline, incoming)
}

// ─── Resource Updates ────────────────────────────────────────────────────────

/// Calculate the updates for image resources
///
/// # Arguments
///
/// * `old` - The current resource allocation on the image
/// * `new` - The incoming resource request from the manifest or editor
#[allow(clippy::needless_pass_by_value)]
fn calculate_resource_update(old: Resources, new: ResourcesRequest) -> Option<ResourcesUpdate> {
    // convert the request into the stored form so equivalent spellings compare equal
    let new_cast = Resources::from(new.clone());
    if old == new_cast {
        return None;
    }
    // diff the burstable limits separately since they have their own update type
    let mut burstable = BurstableResourcesUpdate::default();
    if new_cast.burstable.cpu != old.burstable.cpu {
        burstable.cpu = Some(new.burstable.cpu);
    }
    if new_cast.burstable.memory != old.burstable.memory {
        burstable.memory = Some(new.burstable.memory);
    }
    // compare the remaining fields in request form
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use thorium::models::{EventTrigger, SecurityContext};

    /// Build a stored image exactly as the server would create it from a request
    ///
    /// # Arguments
    ///
    /// * `req` - The request to build the stored image from
    fn stored_image(req: ImageRequest) -> Image {
        Image {
            group: req.group,
            name: req.name,
            creator: "tester".to_string(),
            version: req.version,
            scaler: req.scaler,
            image: req.image,
            lifetime: req.lifetime,
            timeout: req.timeout,
            resources: Resources::from(req.resources),
            spawn_limit: req.spawn_limit,
            env: req.env,
            runtime: 600.0,
            volumes: req.volumes,
            args: req.args,
            modifiers: req.modifiers,
            description: req.description,
            security_context: req.security_context.unwrap_or_default(),
            used_by: Vec::new(),
            collect_logs: req.collect_logs,
            generator: req.generator,
            dependencies: req.dependencies,
            display_type: req.display_type,
            output_collection: req.output_collection,
            child_filters: req.child_filters,
            clean_up: req.clean_up,
            kvm: req.kvm,
            bans: HashMap::new(),
            network_policies: req.network_policies,
        }
    }

    /// Build a stored pipeline with the given order and SLA
    ///
    /// # Arguments
    ///
    /// * `order` - The stored stage order
    /// * `sla` - The stored SLA
    fn stored_pipeline(order: &[&[&str]], sla: u64) -> Pipeline {
        Pipeline {
            group: "g".to_string(),
            name: "p".to_string(),
            creator: "tester".to_string(),
            order: order
                .iter()
                .map(|stage| stage.iter().map(|image| (*image).to_string()).collect())
                .collect(),
            sla,
            triggers: HashMap::new(),
            description: None,
            bans: HashMap::new(),
        }
    }

    /// An unchanged request produces no update
    #[test]
    fn identical_image_is_unchanged() {
        let req = ImageRequest::new("g", "n");
        assert!(calculate_image_update(stored_image(req.clone()), req).is_none());
    }

    /// A change confined to args is detected and applied
    #[test]
    fn args_only_change_is_detected() {
        let existing = stored_image(ImageRequest::new("g", "n"));
        let mut req = ImageRequest::new("g", "n");
        req.args.command = Some(vec!["yara".to_string(), "-s".to_string()]);
        let update = calculate_image_update(existing, req).expect("args change");
        assert!(update.args.is_some());
    }

    /// A description differing only in trailing whitespace is not a change
    #[test]
    fn description_trailing_whitespace_is_unchanged() {
        let mut stored = ImageRequest::new("g", "n");
        stored.description = Some("Scans files\n".to_string());
        let mut req = ImageRequest::new("g", "n");
        req.description = Some("Scans files".to_string());
        assert!(calculate_image_update(stored_image(stored), req).is_none());
    }

    /// Server-applied default policies neither read as a change nor get removed
    #[test]
    fn empty_network_policies_keep_server_defaults() {
        let mut stored = ImageRequest::new("g", "n");
        stored.network_policies = ["deny-egress", "allow-dns", "z-last"]
            .iter()
            .map(|policy| (*policy).to_string())
            .collect();
        let existing = stored_image(stored);
        // an empty list is not a change
        let req = ImageRequest::new("g", "n");
        assert!(calculate_image_update(existing.clone(), req.clone()).is_none());
        // an unrelated change never removes the defaults
        let mut req = req;
        req.timeout = Some(900);
        let update = calculate_image_update(existing, req).expect("timeout change");
        assert!(update.network_policies.policies_removed.is_empty());
        assert!(update.network_policies.policies_added.is_empty());
    }

    /// Omitted security context and timeout keep the existing values
    #[test]
    fn omitted_security_context_and_timeout_keep_existing() {
        let mut stored = ImageRequest::new("g", "n");
        stored.timeout = Some(600);
        stored.security_context = Some(SecurityContext {
            user: Some(1000),
            ..SecurityContext::default()
        });
        let existing = stored_image(stored);
        // omitting both is not a change
        let req = ImageRequest::new("g", "n");
        assert!(calculate_image_update(existing.clone(), req.clone()).is_none());
        // an unrelated change never resets the security context or timeout
        let mut req = req;
        req.description = Some("new".to_string());
        let update = calculate_image_update(existing, req).expect("description change");
        assert!(update.security_context.is_none());
        assert!(update.timeout.is_none());
    }

    /// Removing the modifiers sends the empty string the API treats as a clear
    #[test]
    fn removed_modifiers_are_cleared() {
        let mut stored = ImageRequest::new("g", "n");
        stored.modifiers = Some("mod".to_string());
        let update = calculate_image_update(stored_image(stored), ImageRequest::new("g", "n"))
            .expect("modifiers change");
        assert_eq!(update.modifiers, Some(String::new()));
    }

    /// An omitted SLA keeps the existing SLA
    #[test]
    fn omitted_sla_keeps_existing() {
        let existing = stored_pipeline(&[&["a"]], 3600);
        let req = PipelineRequest::new("g", "p", serde_json::json!([["a"]]));
        assert!(calculate_pipeline_update(existing, req).is_none());
    }

    /// Adding or removing stages is detected without panicking
    #[test]
    fn stage_count_changes_are_detected() {
        // a longer incoming order
        let existing = stored_pipeline(&[&["a"]], 3600);
        let req = PipelineRequest::new("g", "p", serde_json::json!([["a"], ["b"]]));
        let update = calculate_pipeline_update(existing, req).expect("added stage");
        assert_eq!(update.order, Some(serde_json::json!([["a"], ["b"]])));
        // a shorter incoming order
        let existing = stored_pipeline(&[&["a"], &["b"]], 3600);
        let req = PipelineRequest::new("g", "p", serde_json::json!([["a"]]));
        let update = calculate_pipeline_update(existing, req).expect("removed stage");
        assert_eq!(update.order, Some(serde_json::json!([["a"]])));
    }

    /// An invalid order is sent as-is so the server can reject it
    #[test]
    fn invalid_order_is_sent_raw() {
        let existing = stored_pipeline(&[&["a"]], 3600);
        let req = PipelineRequest::new("g", "p", serde_json::json!({"bad": 1}));
        let update = calculate_pipeline_update(existing, req).expect("invalid order");
        assert_eq!(update.order, Some(serde_json::json!({"bad": 1})));
    }

    /// Trigger changes are detected and applied
    #[test]
    fn trigger_changes_are_detected() {
        let existing = stored_pipeline(&[&["a"]], 3600);
        let req = PipelineRequest::new("g", "p", serde_json::json!([["a"]]))
            .trigger("new", EventTrigger::NewSample);
        let update = calculate_pipeline_update(existing, req).expect("trigger change");
        assert!(update.triggers.contains_key("new"));
    }

    /// Rolling back to a snapshot restores exact policies, including removing ones
    /// the import added
    #[test]
    fn revert_to_snapshot_removes_added_policies() {
        let original = stored_image(ImageRequest::new("g", "n"));
        let mut live = original.clone();
        live.network_policies.insert("added".to_string());
        let revert = calculate_image_update_from_mergeable(live, MergeableImage::from(original))
            .unwrap()
            .expect("revert");
        assert!(revert.network_policies.policies_removed.contains("added"));
    }

    /// A pipeline editor result with an invalid trigger fails to parse, so the
    /// editor loop re-opens the editor instead of failing the import later
    #[test]
    fn invalid_trigger_fails_editor_parse() {
        let yaml = "order: [[a]]\nsla: 10\ntriggers:\n  t: Tagg\ndescription: null\n";
        assert!(serde_norway::from_str::<MergeablePipeline>(yaml).is_err());
        let valid = "order: [[a]]\nsla: 10\ntriggers:\n  t: NewSample\ndescription: null\n";
        assert!(serde_norway::from_str::<MergeablePipeline>(valid).is_ok());
    }
}
