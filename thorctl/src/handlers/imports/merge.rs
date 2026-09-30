//! Merge-conflict resolution and the normalized change model for imports
//!
//! Every import flow (`images import`, `pipelines import`, `toolbox import`) and
//! `images`/`pipelines edit` compare resources through the [`MergeableImage`] and
//! [`MergeablePipeline`] views defined here. The same views drive the confirmation
//! screen, the per-resource merge prompt, the editor, `--overwrite`,
//! `--skip-conflicts`, rollback, and `toolbox diff` (see
//! [`super::update::calculate_image_update`]), so all of them agree on whether a
//! resource changed.
//!
//! When an import encounters images or pipelines that already exist, this module
//! also drives the interactive merge workflow: prompting the user for an action,
//! generating YAML with conflict markers, opening the editor, and applying the
//! resulting update.

use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thorium::models::{
    ChildFilters, Cleanup, Dependencies, EventTrigger, Image, ImageArgs, ImageBan, ImageLifetime,
    ImageRequest, ImageScaler, ImageUpdate, ImageVersion, Kvm, OutputCollection, OutputDisplayType,
    Pipeline, PipelineRequest, PipelineUpdate, Resources, ResourcesRequest, SecurityContext,
    SpawnLimits, Volume,
};
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use super::ImportOutcome;
use super::categorize::Categorized;
use super::editor;
use super::kind::ImportKind;
use super::rollback::Journal;
use super::update;
use crate::handlers::progress::Bar;
use crate::utils;

// ─── Curated Editor Key Order ────────────────────────────────────────────────

/// Curated top-level key order for image configs and the image editor view
///
/// This one list orders every image config thorctl writes or shows — `images export`,
/// `toolbox export`, `toolbox init`, `images edit`, the import merge editor, and
/// `toolbox diff` — so the same image always reads the same way. It mirrors the UI's
/// image edit views: the basic fields follow the form in
/// `ui/src/components/pages/images/Fields.tsx`, and the sections follow the edit mode of
/// `ui/src/components/pages/images/ImageInfo.tsx`. `clean_up` has no UI section, so it
/// sits beside `kvm`.
///
/// Covers every `MergeableImage` field and every `build_image_config` key. Keys not
/// listed still appear (sorted) at the end — see [`crate::utils::curated_yaml`]. Plain
/// names; the helper also matches the `*name*` static-marked forms.
#[rustfmt::skip]
pub const IMAGE_FIELD_ORDER: &[&str] = &[
    // identity and server-managed fields (marked *...* in the editor and ignored on save)
    "name", "group", "creator", "bans",
    // basic fields, in UI form order (runtime is read-only but keeps its UI slot)
    "description", "version", "scaler", "image", "timeout", "lifetime", "runtime",
    "display_type", "spawn_limit", "collect_logs", "generator",
    // sections, in UI edit-mode order
    "resources", "args", "output_collection", "dependencies", "env", "volumes",
    "network_policies", "child_filters", "modifiers", "clean_up", "kvm", "security_context",
];

/// Curated top-level key order for pipeline configs and the pipeline editor view
///
/// Shared by `pipelines export`, `toolbox export`, `toolbox init`, `pipelines edit`, the
/// import merge editor, and `toolbox diff`. It mirrors the UI's pipeline create/edit
/// form: the fields in `ui/src/components/pages/pipelines/Fields.tsx` (name, group,
/// description, SLA), then the order, then the triggers. `name`/`group` are absent from
/// the edit view, where they are simply skipped.
pub const PIPELINE_FIELD_ORDER: &[&str] =
    &["name", "group", "description", "sla", "order", "triggers"];

// ─── Mergeable Structs ───────────────────────────────────────────────────────

/// The top-level keys of a [`MergeableImage`] that are identity or server-managed
/// fields: shown in the editor for context but never compared or applied
const NON_EDITABLE_IMAGE_KEYS: &[&str] = &["*group*", "*name*", "*creator*", "*runtime*", "*bans*"];

/// Normalize a description into its canonical comparison form
///
/// `toolbox build` injects the description from `description.md`, trimming trailing
/// whitespace (build.rs `apply_description_md`), while the live image/pipeline in
/// Thorium keeps whatever was stored (often with a trailing newline). Trimming
/// trailing whitespace on every side — and collapsing an empty result to `None` —
/// makes the comparison and the generated update insensitive to that round-trip.
///
/// # Arguments
///
/// * `description` - The raw description to normalize
fn normalize_description(description: Option<String>) -> Option<String> {
    description
        .map(|text| text.trim_end().to_string())
        .filter(|text| !text.is_empty())
}

/// An image converted to a common format for editing, merging, and diffing.
///
/// Used by standalone `images edit`, every image import flow, and `toolbox diff`.
/// Identity fields (group, name, creator) and server-managed fields (runtime,
/// bans) are `Option` — populated when editing an existing image via
/// `From<Image>`, left as `None` when converting from an `ImageRequest`. Fields
/// set to `None` are omitted from serialized YAML. Unknown keys are rejected so a
/// typo in the editor is reported instead of silently ignored.
///
/// Both `From` conversions [`normalize`](Self::normalize) the view, so set-like
/// lists are sorted and descriptions are trimmed before anything is compared.
/// Views are compared and rendered through [`canonical_value`](Self::canonical_value),
/// which sorts map keys and `HashSet`-backed arrays, so comparisons are insensitive
/// to set/map order and sensitive to the order of real lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeableImage {
    /// The group the image belongs to (identity, non-editable; `None` from a request)
    #[serde(rename = "*group*", skip_serializing_if = "Option::is_none", default)]
    pub group: Option<String>,
    /// The image's name (identity, non-editable; `None` from a request)
    #[serde(rename = "*name*", skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// The user that created the image (server-managed, non-editable; `None` from a request)
    #[serde(rename = "*creator*", skip_serializing_if = "Option::is_none", default)]
    pub creator: Option<String>,
    /// The image's version, or `None` for a versionless image
    pub version: Option<ImageVersion>,
    /// The scaler that schedules this image
    pub scaler: ImageScaler,
    /// The container image URL (trimmed), or `None` when not set
    pub image: Option<String>,
    /// How long the image lives before it is reaped, or `None` for no limit
    pub lifetime: Option<ImageLifetime>,
    /// The per-job timeout in seconds, or `None` for the default
    pub timeout: Option<u64>,
    /// The compute resources requested for each job
    pub resources: ResourcesRequest,
    /// The limit on how many copies of this image can run at once
    pub spawn_limit: SpawnLimits,
    /// The environment variables to set, where `None` unsets the variable
    pub env: HashMap<String, Option<String>>,
    /// The image's average runtime (server-managed, non-editable; `None` from a request)
    #[serde(rename = "*runtime*", skip_serializing_if = "Option::is_none", default)]
    pub runtime: Option<f64>,
    /// The volumes mounted into each job, sorted by name
    pub volumes: Vec<Volume>,
    /// The command-line argument layout passed to the tool
    pub args: ImageArgs,
    /// Free-form scaler modifiers, or `None` when not set (an empty string is `None`)
    pub modifiers: Option<String>,
    /// The image's description (normalized via [`normalize_description`])
    pub description: Option<String>,
    /// The container security context, or `None` for the server default
    pub security_context: Option<SecurityContext>,
    /// Whether the agent collects job stdout/stderr as logs
    pub collect_logs: bool,
    /// Whether this image generates children (and reruns itself until done)
    pub generator: bool,
    /// The other images this image depends on for inputs
    pub dependencies: Dependencies,
    /// How this image's results are displayed in the UI
    pub display_type: OutputDisplayType,
    /// How and where this image's output is collected
    pub output_collection: OutputCollection,
    /// The filters controlling which children are submitted back to Thorium
    pub child_filters: ChildFilters,
    /// The cleanup tool to run after each job, or `None` for none
    pub clean_up: Option<Cleanup>,
    /// The KVM/VM configuration for this image, or `None` for a non-VM image
    pub kvm: Option<Kvm>,
    /// The bans on this image keyed by ban id (server-managed, non-editable; `None` from a request)
    #[serde(rename = "*bans*", skip_serializing_if = "Option::is_none", default)]
    pub bans: Option<HashMap<Uuid, ImageBan>>,
    /// The network policies applied to this image's jobs
    pub network_policies: HashSet<String>,
}

impl MergeableImage {
    /// Put the view into its canonical comparison form
    ///
    /// Lists that the update API treats as sets (volumes matched by name, dependency
    /// image/name lists, output file names and groups) are sorted, since their order
    /// can't be changed by an update and carries no meaning. Descriptions are trimmed
    /// (see [`normalize_description`]), and the container image and modifiers are
    /// trimmed/collapsed the way the API stores them, so an empty value is `None`.
    pub(crate) fn normalize(&mut self) {
        // volumes are diffed by name, so their list order carries no meaning
        self.volumes
            .sort_by(|left, right| left.name.cmp(&right.name));
        // dependency lists are diffed as add/remove sets
        self.dependencies.ephemeral.names.sort_unstable();
        self.dependencies.results.images.sort_unstable();
        self.dependencies.results.names.sort_unstable();
        self.dependencies.children.images.sort_unstable();
        self.dependencies.filesystems.images.sort_unstable();
        // output file names are diffed as a set and groups are a set of group names
        self.output_collection.files.names.sort_unstable();
        self.output_collection.groups.sort_unstable();
        // trim descriptions so a description.md round-trip doesn't read as drift
        self.description = normalize_description(self.description.take());
        // the API trims the container image and treats an empty one as unset
        self.image = self
            .image
            .take()
            .map(|image| image.trim().to_string())
            .filter(|image| !image.is_empty());
        // the API stores an empty modifiers string as unset
        self.modifiers = self
            .modifiers
            .take()
            .filter(|modifiers| !modifiers.is_empty());
    }

    /// Keep the current value for fields the image update API can't clear or set
    ///
    /// An `ImageUpdate` has no way to clear a timeout or KVM settings, or to change
    /// an output file handler's `entities` path. Leaving those differences in place
    /// would report a change that applying the update can never converge, so an
    /// omitted timeout/KVM config and any `entities` value are taken from `current`.
    ///
    /// # Arguments
    ///
    /// * `current` - The view of the image as it exists in Thorium
    pub(crate) fn retain_unsettable(&mut self, current: &MergeableImage) {
        // a timeout can be changed but never cleared
        if self.timeout.is_none() {
            self.timeout = current.timeout;
        }
        // KVM settings can be changed but never cleared
        if self.kvm.is_none() {
            self.kvm.clone_from(&current.kvm);
        }
        // the files handler's entities path has no update field at all
        self.output_collection
            .files
            .entities
            .clone_from(&current.output_collection.files.entities);
    }

    /// Clear the identity and server-managed fields, leaving only editable ones
    pub(crate) fn strip_non_editable(&mut self) {
        self.group = None;
        self.name = None;
        self.creator = None;
        self.runtime = None;
        self.bans = None;
    }

    /// Serialize the view with object keys and set-valued arrays in canonical order
    ///
    /// Used for both comparing and rendering views, so two views with the same sets
    /// always compare equal and render identically run-to-run.
    pub(crate) fn canonical_value(&self) -> serde_json::Value {
        // serializing plain data into a JSON value can't fail; Null only compares
        // equal to another Null, which can't hide a real difference in practice
        let mut value = serde_json::to_value(self).unwrap_or_default();
        // objects are sorted maps already; sort the HashSet-backed arrays too
        crate::utils::sort_set_fields(&mut value, crate::utils::IMAGE_SET_FIELDS);
        value
    }

    /// Serialize only the editable fields, for comparing two views
    pub(crate) fn editable_value(&self) -> serde_json::Value {
        // start from the canonical form so set order never reads as a change
        let mut value = self.canonical_value();
        // drop identity/server-managed fields; imports and edits never change them
        if let Some(map) = value.as_object_mut() {
            map.retain(|key, _| !NON_EDITABLE_IMAGE_KEYS.contains(&key.as_str()));
        }
        value
    }

    /// Whether two views have the same editable content
    ///
    /// # Arguments
    ///
    /// * `other` - The view to compare against
    pub(crate) fn same_editable(&self, other: &MergeableImage) -> bool {
        self.editable_value() == other.editable_value()
    }
}

impl From<Image> for MergeableImage {
    /// Build a mergeable image from an existing Thorium image, populating the
    /// identity and server-managed fields the request form lacks
    ///
    /// # Arguments
    ///
    /// * `image` - The existing Thorium image to convert
    fn from(image: Image) -> Self {
        let mut view = Self {
            // identity + server-managed fields are present on a live image; carry them
            // so the editor view shows them (marked static) and mirroring has values
            group: Some(image.group),
            name: Some(image.name),
            creator: Some(image.creator),
            version: image.version,
            scaler: image.scaler,
            image: image.image,
            lifetime: image.lifetime,
            timeout: image.timeout,
            // the live image carries full `Resources`; convert to the request shape
            // the editor edits in
            resources: image.resources.into(),
            spawn_limit: image.spawn_limit,
            env: image.env,
            runtime: Some(image.runtime),
            volumes: image.volumes,
            args: image.args,
            modifiers: image.modifiers,
            description: image.description,
            // a live image always has a concrete security context; wrap it to match
            // the request-sourced side
            security_context: Some(image.security_context),
            collect_logs: image.collect_logs,
            generator: image.generator,
            dependencies: image.dependencies,
            display_type: image.display_type,
            output_collection: image.output_collection,
            child_filters: image.child_filters,
            clean_up: image.clean_up,
            kvm: image.kvm,
            bans: Some(image.bans),
            network_policies: image.network_policies,
        };
        // sort set-like lists and trim text fields into the comparison form
        view.normalize();
        view
    }
}

impl From<ImageRequest> for MergeableImage {
    /// Build a mergeable image from a request exactly as the server would create it
    ///
    /// Identity and server-managed fields are left unset since a request never
    /// carries them. An omitted security context becomes the server default. To
    /// compare a request against an existing image, use [`incoming_image_view`],
    /// which resolves omitted fields against that image instead.
    ///
    /// # Arguments
    ///
    /// * `req` - The incoming image request
    fn from(req: ImageRequest) -> Self {
        let mut view = Self {
            // a request never carries identity/server-managed fields; leave them unset
            // so they're omitted from the serialized editor view
            group: None,
            name: None,
            creator: None,
            version: req.version,
            scaler: req.scaler,
            image: req.image,
            lifetime: req.lifetime,
            timeout: req.timeout,
            // round-trip through the stored form so equivalent spellings compare equal
            resources: ResourcesRequest::from(Resources::from(req.resources)),
            spawn_limit: req.spawn_limit,
            env: req.env,
            // server-managed, never present on a request
            runtime: None,
            volumes: req.volumes,
            args: req.args,
            modifiers: req.modifiers,
            description: req.description,
            // the server creates an image without a security context with the default
            security_context: Some(req.security_context.unwrap_or_default()),
            collect_logs: req.collect_logs,
            generator: req.generator,
            dependencies: req.dependencies,
            display_type: req.display_type,
            output_collection: req.output_collection,
            child_filters: req.child_filters,
            clean_up: req.clean_up,
            kvm: req.kvm,
            // server-managed, never present on a request
            bans: None,
            network_policies: req.network_policies,
        };
        // sort set-like lists and trim text fields into the comparison form
        view.normalize();
        view
    }
}

/// Build the incoming view of an image request compared against an existing image
///
/// This defines what an omitted field in an import request means for an image that
/// already exists. Fields with no server-side default mean exactly what they say
/// (an omitted description, version, lifetime, clean-up, ... clears it). Fields the
/// server fills in on create, or that the update API can't clear, keep the existing
/// image's value instead:
///
/// - `security_context`: the server applies its default on create, and only admins
///   may set one, so an omitted context never resets a custom one
/// - `network_policies`: on create the server applies the group's default network
///   policies to a K8s image whose list is empty, so an empty list keeps whatever
///   policies the image has rather than reading as a change or removing them
/// - `timeout`, `kvm`, and the output files handler's `entities` path: the update
///   API can't clear/set them (see [`MergeableImage::retain_unsettable`])
///
/// Identity and server-managed fields are mirrored from the existing image so the
/// merge editor shows them as shared context rather than conflicts.
///
/// # Arguments
///
/// * `existing` - The image as it exists in Thorium
/// * `req` - The incoming image request
pub(crate) fn incoming_image_view(existing: &Image, mut req: ImageRequest) -> MergeableImage {
    // an omitted security context keeps the existing one
    if req.security_context.is_none() {
        req.security_context = Some(existing.security_context.clone());
    }
    // an empty policy list keeps the existing (often server-defaulted) policies
    if req.network_policies.is_empty() {
        req.network_policies.clone_from(&existing.network_policies);
    }
    // build both views in their normalized comparison form
    let current = MergeableImage::from(existing.clone());
    let mut incoming = MergeableImage::from(req);
    // keep the values the update API can't clear or set
    incoming.retain_unsettable(&current);
    // show identity/server-managed fields as shared context in the editor
    mirror_non_editable_fields(&mut incoming, &current);
    incoming
}

/// A pipeline converted to a common format for merge comparison and YAML editing.
/// Only contains editable fields — group, name, creator, and bans are excluded.
///
/// Triggers are typed, so an invalid trigger typed in the editor is a parse error
/// that re-opens the editor instead of failing the import after the editor closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeablePipeline {
    /// The stage ordering, as groups of image names that run together in sequence
    pub order: Vec<Vec<String>>,
    /// The pipeline's SLA in seconds
    pub sla: u64,
    /// The pipeline's triggers keyed by name
    #[serde(default)]
    pub triggers: HashMap<String, EventTrigger>,
    /// The pipeline's description (normalized via [`normalize_description`])
    pub description: Option<String>,
}

impl MergeablePipeline {
    /// Put the view into its canonical comparison form by trimming the description
    pub(crate) fn normalize(&mut self) {
        self.description = normalize_description(self.description.take());
    }

    /// Whether two views have the same content
    ///
    /// # Arguments
    ///
    /// * `other` - The view to compare against
    pub(crate) fn same_content(&self, other: &MergeablePipeline) -> bool {
        // serializing plain data into a JSON value can't fail; triggers are a map, so
        // this comparison is insensitive to trigger order
        serde_json::to_value(self).unwrap_or_default()
            == serde_json::to_value(other).unwrap_or_default()
    }
}

impl From<Pipeline> for MergeablePipeline {
    /// Build a mergeable pipeline from an existing Thorium pipeline
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The existing Thorium pipeline to convert
    fn from(pipeline: Pipeline) -> Self {
        let mut view = Self {
            order: pipeline.order,
            sla: pipeline.sla,
            triggers: pipeline.triggers,
            description: pipeline.description,
        };
        // trim the description into the comparison form
        view.normalize();
        view
    }
}

impl From<PipelineRequest> for MergeablePipeline {
    /// Build a mergeable pipeline from a request, applying a one-week SLA when the
    /// request omits one
    ///
    /// An order that isn't a valid list of stages becomes an empty order here; the
    /// update calculation sends such an order to the server unchanged so it can
    /// report why it's invalid. To compare a request against an existing pipeline,
    /// use [`incoming_pipeline_view`], which keeps the existing SLA instead.
    ///
    /// # Arguments
    ///
    /// * `req` - The incoming pipeline request
    fn from(req: PipelineRequest) -> Self {
        // deserialize the order from the flexible Value format to Vec<Vec<String>>
        let order: Vec<Vec<String>> = req
            .deserialize_image_order()
            .unwrap_or_default()
            .into_iter()
            .map(|inner| inner.into_iter().map(String::from).collect())
            .collect();
        let mut view = Self {
            order,
            // a request without an explicit SLA gets the server default of one week
            sla: req.sla.unwrap_or(thorium::models::DEFAULT_PIPELINE_SLA),
            triggers: req.triggers,
            description: req.description,
        };
        // trim the description into the comparison form
        view.normalize();
        view
    }
}

/// Build the incoming view of a pipeline request compared against an existing pipeline
///
/// An omitted `sla` keeps the existing pipeline's SLA rather than resetting it to a
/// default, matching how [`incoming_image_view`] treats fields the server fills in
/// on create. Every other field means exactly what the request says.
///
/// # Arguments
///
/// * `existing` - The pipeline as it exists in Thorium
/// * `req` - The incoming pipeline request
pub(crate) fn incoming_pipeline_view(
    existing: &Pipeline,
    mut req: PipelineRequest,
) -> MergeablePipeline {
    // an omitted SLA keeps the existing one
    if req.sla.is_none() {
        req.sla = Some(existing.sla);
    }
    MergeablePipeline::from(req)
}

// ─── Per-Resource Prompt ─────────────────────────────────────────────────────

/// The action the user wants to take for a changed resource
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MergeAction {
    /// Open the editor to review and resolve conflicts
    Edit,
    /// Keep the existing configuration unchanged
    Skip,
    /// Accept all incoming changes
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
        "\n{} '{}' has changes:",
        resource_type.bright_yellow(),
        utils::resource_id(group, name).bright_blue(),
    );
    let items = &[
        "Edit   - Open editor to review and resolve conflicts",
        "Skip   - Keep the existing configuration unchanged",
        "Apply  - Accept all incoming changes",
        "Quit   - Stop processing remaining resources",
    ];
    let selection = dialoguer::Select::new()
        .items(items)
        .default(0)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
    Ok(match selection {
        0 => MergeAction::Edit,
        2 => MergeAction::Apply,
        3 => MergeAction::Quit,
        _ => MergeAction::Skip,
    })
}

/// Copy identity and server-managed fields from `current` onto `incoming`
///
/// An import request never carries an image's identity fields (group, name,
/// creator) or its server-managed fields (runtime, bans), and this merge can't
/// edit them — so the incoming side always omits them. Left alone, the line-based
/// conflict diff would flag every such field as a conflict (a value on the current
/// side, nothing on the incoming side). Mirroring them makes both sides match so
/// they render as shared context instead. Change detection and
/// `calculate_image_update*` ignore these fields, so this is display-only.
///
/// # Arguments
///
/// * `incoming` - The incoming view to copy the fields onto
/// * `current` - The current view to copy the fields from
fn mirror_non_editable_fields(incoming: &mut MergeableImage, current: &MergeableImage) {
    incoming.group.clone_from(&current.group);
    incoming.name.clone_from(&current.name);
    incoming.creator.clone_from(&current.creator);
    incoming.runtime = current.runtime;
    incoming.bans.clone_from(&current.bans);
}

// ─── Single-Resource Interactive Merge ───────────────────────────────────────

/// The conflict-marker label for the Thorium side of a merge
const CURRENT_LABEL: &str = "Current (Thorium)";
/// The conflict-marker label for the imported side of a merge
const INCOMING_LABEL: &str = "Incoming (Import)";

/// The result of resolving a merge conflict in the editor
#[derive(Debug, PartialEq, Eq)]
pub enum MergeEdit<U> {
    /// The resolved state differs from the current resource and this update applies it
    Update(U),
    /// The resolved state matches the current resource, so there is nothing to apply
    NoChanges,
    /// The user cancelled the edit
    Cancelled,
}

impl<U> MergeEdit<U> {
    /// Wrap a calculated update, where `None` means the resolved state changed nothing
    ///
    /// # Arguments
    ///
    /// * `update` - The update calculated from the resolved state, if any
    fn from_update(update: Option<U>) -> Self {
        match update {
            Some(update) => MergeEdit::Update(update),
            None => MergeEdit::NoChanges,
        }
    }
}

/// Resolve an image merge conflict via the editor and return the resulting
/// image update, or whether the user's edits changed nothing or they cancelled
///
/// # Arguments
///
/// * `image` - The current image in Thorium
/// * `req` - The incoming image request
/// * `conf` - The Thorctl config
/// * `editor_override` - Optional editor override from the CLI
pub(crate) async fn merge_image_interactive(
    image: &Image,
    req: &ImageRequest,
    conf: &CtlConf,
    editor_override: Option<&str>,
) -> Result<MergeEdit<ImageUpdate>, Error> {
    // build both sides in the same normalized form change detection uses, with
    // omitted incoming fields resolved against the current image
    let current = MergeableImage::from(image.clone());
    let incoming = incoming_image_view(image, req.clone());
    // serialize both in the curated editor key order (see IMAGE_FIELD_ORDER); nested
    // maps are key-sorted and set-like lists sorted, so reordering never conflicts
    let current_yaml = crate::utils::curated_yaml(&current.canonical_value(), IMAGE_FIELD_ORDER)
        .map_err(|err| Error::new(format!("Failed to serialize current image to YAML: {err}")))?;
    let incoming_yaml = crate::utils::curated_yaml(&incoming.canonical_value(), IMAGE_FIELD_ORDER)
        .map_err(|err| Error::new(format!("Failed to serialize incoming image to YAML: {err}")))?;
    // generate the conflict YAML
    let conflict_yaml = editor::generate_conflict_view(
        &current_yaml,
        &incoming_yaml,
        CURRENT_LABEL,
        INCOMING_LABEL,
    );
    // open the editor; parse errors re-open it via the editor loop's Edit/Cancel prompt
    let editor_cmd = editor::resolve_editor(editor_override, conf);
    let label = format!("{}-{}", image.group, image.name);
    let resolved: MergeableImage =
        match editor::editor_loop(&conflict_yaml, &label, editor_cmd).await? {
            Some(resolved) => resolved,
            None => return Ok(MergeEdit::Cancelled),
        };
    // calculate update from the current image to the resolved state
    update::calculate_image_update_from_mergeable(image.clone(), resolved)
        .map(MergeEdit::from_update)
}

/// Resolve a pipeline merge conflict via the editor and return the resulting
/// pipeline update, or whether the user's edits changed nothing or they cancelled
///
/// # Arguments
///
/// * `pipeline` - The current pipeline in Thorium
/// * `req` - The incoming pipeline request
/// * `conf` - The Thorctl config
/// * `editor_override` - Optional editor override from the CLI
pub(crate) async fn merge_pipeline_interactive(
    pipeline: &Pipeline,
    req: &PipelineRequest,
    conf: &CtlConf,
    editor_override: Option<&str>,
) -> Result<MergeEdit<PipelineUpdate>, Error> {
    // build both sides in the same normalized form change detection uses, with an
    // omitted incoming SLA resolved against the current pipeline
    let current = MergeablePipeline::from(pipeline.clone());
    let incoming = incoming_pipeline_view(pipeline, req.clone());
    // serialize both in the curated editor key order (see PIPELINE_FIELD_ORDER);
    // nested maps (triggers) are key-sorted so reordering never conflicts
    let current_yaml =
        crate::utils::curated_yaml(&current, PIPELINE_FIELD_ORDER).map_err(|err| {
            Error::new(format!(
                "Failed to serialize current pipeline to YAML: {err}"
            ))
        })?;
    let incoming_yaml =
        crate::utils::curated_yaml(&incoming, PIPELINE_FIELD_ORDER).map_err(|err| {
            Error::new(format!(
                "Failed to serialize incoming pipeline to YAML: {err}"
            ))
        })?;
    // generate the conflict YAML
    let conflict_yaml = editor::generate_conflict_view(
        &current_yaml,
        &incoming_yaml,
        CURRENT_LABEL,
        INCOMING_LABEL,
    );
    // open the editor; triggers are typed, so an invalid trigger is a parse error
    // that re-opens the editor via the editor loop's Edit/Cancel prompt
    let editor_cmd = editor::resolve_editor(editor_override, conf);
    let label = format!("{}-{}", pipeline.group, pipeline.name);
    let resolved: MergeablePipeline =
        match editor::editor_loop(&conflict_yaml, &label, editor_cmd).await? {
            Some(resolved) => resolved,
            None => return Ok(MergeEdit::Cancelled),
        };
    // calculate update from the current pipeline to the resolved state
    update::calculate_pipeline_update_from_mergeable(pipeline.clone(), resolved)
        .map(MergeEdit::from_update)
}

// ─── Batch Interactive Merge ─────────────────────────────────────────────────

/// Interactively handle existing resources that have changes, prompting the user
/// for each one to Edit (merge editor), Skip, Apply (accept incoming), or Quit
///
/// Only resources whose computed update is non-empty ([`ImportKind::calculate_update`],
/// the same predicate as the confirmation screen and `--overwrite`) are prompted
/// for. If an edit or apply fails (an editor error, or the server rejecting the
/// update), the error is shown and the same resource is prompted for again so the
/// user can retry, skip it, or quit. A cancelled edit also prompts for the same
/// resource again.
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to apply updates
/// * `existing` - Imported resources that already exist in Thorium
/// * `conf` - The Thorctl config (used for the default editor)
/// * `editor_override` - Optional editor command that overrides the configured editor
/// * `progress` - The progress bar (suspended during interactive prompts)
/// * `journal` - The journal to snapshot pre-update state in for rollback
pub async fn interactive_merge<K: ImportKind>(
    thorium: &Thorium,
    existing: Vec<&Categorized<K>>,
    conf: &CtlConf,
    editor_override: Option<&str>,
    progress: &Bar,
    journal: &Journal,
) -> Result<ImportOutcome, Error> {
    // keep only resources with an effective change, computing each update once so
    // Apply reuses it
    let changed: Vec<(&Categorized<K>, &K::Existing, K::Update)> = existing
        .into_iter()
        .filter_map(|item| {
            // resources that don't exist yet are handled by the create pass
            let current = item.existing.as_ref()?;
            // an empty update means nothing would change, so there's nothing to prompt
            let update = K::calculate_update(current.clone(), item.request.clone())?;
            Some((item, current, update))
        })
        .collect();
    for (item, current, incoming_update) in changed {
        // resolve the resource's identity for the prompt and messages
        let group = K::group(&item.request);
        let name = K::name(&item.request);
        // re-prompt for this resource until an action completes
        loop {
            // suspend the progress bar for interactive prompts
            let action = progress.suspend(|| prompt_merge_action(K::TITLE, group, name))?;
            // run the chosen action, yielding the error of a failed edit/apply
            let result = match action {
                MergeAction::Edit => {
                    // open the editor to resolve this resource's conflicts
                    let edited = progress
                        .suspend_async(K::merge_interactive(
                            current,
                            &item.request,
                            conf,
                            editor_override,
                        ))
                        .await;
                    match edited {
                        Ok(MergeEdit::Update(update)) => {
                            apply_merge_update::<K>(
                                thorium, item, current, &update, progress, journal,
                            )
                            .await
                        }
                        Ok(MergeEdit::Cancelled) => {
                            // a cancelled edit changes nothing, so prompt for this resource again
                            progress.suspend(|| {
                                println!(
                                    "Edit cancelled for {} '{}'",
                                    K::NOUN,
                                    utils::resource_id(group, name),
                                );
                            });
                            continue;
                        }
                        Ok(MergeEdit::NoChanges) => {
                            progress.suspend(|| {
                                println!(
                                    "{} No changes detected for {} '{}'",
                                    "Skipped:".bright_blue(),
                                    K::NOUN,
                                    utils::resource_id(group, name),
                                );
                            });
                            Ok(())
                        }
                        Err(err) => Err(Error::new(format!(
                            "Failed to edit {} '{}': {err}",
                            K::NOUN,
                            item.label()
                        ))),
                    }
                }
                MergeAction::Skip => {
                    progress.info_anonymous(format!(
                        "Skipping {} '{}'",
                        K::NOUN,
                        item.label().bright_yellow()
                    ));
                    Ok(())
                }
                MergeAction::Apply => {
                    apply_merge_update::<K>(
                        thorium,
                        item,
                        current,
                        &incoming_update,
                        progress,
                        journal,
                    )
                    .await
                }
                MergeAction::Quit => {
                    progress.suspend(|| println!("Stopping further resource processing."));
                    return Ok(ImportOutcome::Quit);
                }
            };
            // a failed edit/apply is shown and this resource is prompted for again
            match result {
                Ok(()) => break,
                Err(err) => progress.error(format!("{err}")),
            }
        }
    }
    Ok(ImportOutcome::Completed)
}

/// Apply a resolved update to Thorium, snapshot the prior state for rollback, and
/// print the success line
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to apply the update
/// * `item` - The categorized resource being updated (source of group/name)
/// * `current` - The pre-update resource state to snapshot for rollback
/// * `update` - The resolved update payload to apply
/// * `progress` - The progress bar to print the success line through
/// * `journal` - The journal to record the pre-update snapshot in
async fn apply_merge_update<K: ImportKind>(
    thorium: &Thorium,
    item: &Categorized<K>,
    current: &K::Existing,
    update: &K::Update,
    progress: &Bar,
    journal: &Journal,
) -> Result<(), Error> {
    // resolve the resource's group and name for the update call and messages
    let group = K::group(&item.request);
    let name = K::name(&item.request);
    K::update(thorium, group, name, update)
        .await
        .map_err(|err| {
            Error::new(format!(
                "Failed to update {} '{}': {}",
                K::NOUN,
                item.label(),
                err
            ))
        })?;
    // snapshot the pre-update state so the update can be reverted
    K::record_updated(journal, current.clone());
    // print the success line without colliding with the progress bar
    progress.suspend(|| {
        println!(
            "{} {} {}",
            K::TITLE.bright_green(),
            format!("'{}'", utils::resource_id(group, name)).yellow(),
            "updated successfully!".bright_green()
        );
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::imports::editor::generate_conflict_view;
    use std::collections::HashMap;
    use thorium::models::ImageRequest;

    /// A calculated update becomes `Update` and no update becomes `NoChanges`
    #[test]
    fn merge_edit_from_update() {
        assert_eq!(MergeEdit::from_update(Some(1)), MergeEdit::Update(1));
        assert_eq!(MergeEdit::<i32>::from_update(None), MergeEdit::NoChanges);
    }

    /// Build a MergeableImage with the non-editable fields populated
    fn populated_mergeable_image() -> MergeableImage {
        let mut image = MergeableImage::from(ImageRequest::new("g", "n"));
        image.group = Some("g".to_string());
        image.name = Some("n".to_string());
        image.creator = Some("c".to_string());
        image.runtime = Some(1.0);
        image.bans = Some(HashMap::new());
        image
    }

    /// Every non-editable image field renders with the `*<field>*` static marker so
    /// users can't mistake it for editable
    #[test]
    fn non_editable_image_fields_are_static_marked() {
        let yaml = serde_norway::to_string(&populated_mergeable_image()).unwrap();
        for marked in ["*group*", "*name*", "*creator*", "*runtime*", "*bans*"] {
            assert!(
                yaml.contains(marked),
                "expected static marker {marked} in:\n{yaml}"
            );
        }
    }

    /// Every serialized top-level field of a MergeableImage is listed in
    /// IMAGE_FIELD_ORDER, so the curated order stays complete as fields are added
    /// (the `curated_yaml` fallback still emits unlisted keys, but they'd lose their
    /// curated position — this guards against that drift)
    #[test]
    fn image_field_order_covers_serialized_fields() {
        let value = serde_json::to_value(populated_mergeable_image()).unwrap();
        for key in value.as_object().unwrap().keys() {
            // strip the *...* static marker before matching the plain-name list
            let plain = key.trim_matches('*');
            assert!(
                IMAGE_FIELD_ORDER.contains(&plain),
                "MergeableImage field '{key}' missing from IMAGE_FIELD_ORDER"
            );
        }
    }

    /// A description.md round-trip trims trailing whitespace while the live value
    /// keeps it; both must normalize to the same form so a fresh export diffs clean
    #[test]
    fn description_normalization_ignores_trailing_whitespace() {
        assert_eq!(
            normalize_description(Some("hello\n".to_string())),
            Some("hello".to_string())
        );
        assert_eq!(
            normalize_description(Some("hello".to_string())),
            Some("hello".to_string())
        );
        // whitespace-only and empty descriptions collapse to None on both sides
        assert_eq!(normalize_description(Some("  \n".to_string())), None);
        assert_eq!(normalize_description(Some(String::new())), None);
        assert_eq!(normalize_description(None), None);
    }

    /// Identity and server-managed fields present only on the current (Thorium)
    /// side must not surface as conflicts once mirrored onto the incoming side
    #[test]
    fn mirroring_removes_spurious_server_field_conflicts() {
        // the current image (from Thorium) carries identity + server-managed fields
        let mut current = MergeableImage::from(ImageRequest::new("static", "exiftool"));
        current.group = Some("static".to_string());
        current.name = Some("exiftool".to_string());
        current.creator = Some("test".to_string());
        current.runtime = Some(1.5);
        current.bans = Some(HashMap::new());
        // a manifest-sourced request omits all of those (editable fields match)
        let mut incoming = MergeableImage::from(ImageRequest::new("static", "exiftool"));

        // without mirroring, those fields diff and produce a conflict block
        let before = generate_conflict_view(
            &serde_norway::to_string(&current).unwrap(),
            &serde_norway::to_string(&incoming).unwrap(),
            "Current (Thorium)",
            "Incoming (Manifest)",
        );
        assert!(
            before.contains("<<<<<<<"),
            "expected a conflict before mirroring"
        );

        // after mirroring, the two sides match and there is no conflict
        mirror_non_editable_fields(&mut incoming, &current);
        let after = generate_conflict_view(
            &serde_norway::to_string(&current).unwrap(),
            &serde_norway::to_string(&incoming).unwrap(),
            "Current (Thorium)",
            "Incoming (Manifest)",
        );
        assert!(
            !after.contains("<<<<<<<"),
            "unexpected conflict after mirroring:\n{after}"
        );
    }

    /// A request that omits the security context must surface the server default
    /// (an object), not `null`, so it matches an existing image and doesn't read as
    /// drift in `toolbox diff` or the merge view
    #[test]
    fn omitted_security_context_defaults_to_object() {
        let mergeable = MergeableImage::from(ImageRequest::new("static", "exiftool"));
        let value = serde_json::to_value(&mergeable).unwrap();
        assert!(
            value["security_context"].is_object(),
            "expected the default security context object, got {:?}",
            value["security_context"]
        );
    }

    /// Canonical YAML (used to build the conflict view) must still deserialize
    /// back into the mergeable types, since the editor parses the resolved text
    #[test]
    fn canonical_yaml_round_trips_mergeables() {
        let image = MergeableImage::from(ImageRequest::new("static", "exiftool"));
        let image_yaml = crate::utils::canonical_yaml(&image).unwrap();
        serde_norway::from_str::<MergeableImage>(&image_yaml)
            .expect("canonical image YAML must deserialize");

        let pipeline = MergeablePipeline::from(PipelineRequest::new(
            "static",
            "p",
            serde_json::json!([["a", "b"]]),
        ));
        let pipeline_yaml = crate::utils::canonical_yaml(&pipeline).unwrap();
        serde_norway::from_str::<MergeablePipeline>(&pipeline_yaml)
            .expect("canonical pipeline YAML must deserialize");
    }

    /// A typo'd key in the image editor is rejected rather than silently ignored
    #[test]
    fn mergeable_image_rejects_unknown_keys() {
        let image = MergeableImage::from(ImageRequest::new("static", "exiftool"));
        let mut value = serde_json::to_value(&image).unwrap();
        value["timout"] = serde_json::json!(900);
        assert!(serde_json::from_value::<MergeableImage>(value).is_err());
    }

    /// Set-valued fields render in sorted order so views are stable run-to-run
    #[test]
    fn set_fields_serialize_sorted() {
        let mut req = ImageRequest::new("static", "exiftool");
        req.network_policies = ["c", "a", "b"].iter().map(|p| (*p).to_string()).collect();
        req.child_filters.mime = ["z", "x", "y"].iter().map(|p| (*p).to_string()).collect();
        let value = MergeableImage::from(req).canonical_value();
        assert_eq!(
            value["network_policies"],
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(
            value["child_filters"]["mime"],
            serde_json::json!(["x", "y", "z"])
        );
    }
}
