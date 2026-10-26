//! Loading a toolbox manifest of either format
//!
//! [`load_any`] reads a legacy manifest (`images` and `pipelines`), a v2 manifest, or a hand merged
//! file holding both, and returns the v2 view of it along with a report of what the legacy part
//! mapped to. A legacy entry's image config is kept on its version (see
//! [`super::types::LegacyImage`]) until the spec is built from it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use thorium::Error;
use thorium::client::migrations::{MigrationNote, MigrationReason, SkippedItem};
use thorium::models::{ImageRequest, ImageScaler, NetworkPolicyRequest, PipelineRequest};

use super::types::{
    BuildManifest, FORMAT_VERSION, LegacyImage, PipelineConfig, PipelineManifestV2,
    PipelineVersionV2, StageTool, ToolManifest, ToolVersionManifest, ToolboxManifestV2,
    check_label, check_name,
};
use crate::args::toolbox::ManifestLocation;
use crate::handlers::progress::Bar;
use crate::handlers::toolbox::build::BaseImage;
use crate::handlers::toolbox::manifest::{ImageManifest, ImageVersion, ToolboxManifest};
use crate::handlers::toolbox::shared;

/// The names of the manifest files a toolbox directory may hold, in the order they are tried
const MANIFEST_FILES: [&str; 2] = ["toolbox.v2.json", "toolbox.json"];

/// The top level keys that mark a document as a v2 manifest
const V2_SECTIONS: [&str; 5] = ["tools", "toolsets", "agents", "workflows", "builds"];

/// The format a manifest was read in
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    /// A legacy manifest holding `images` and `pipelines`
    Legacy,
    /// A v2 manifest
    V2,
    /// A manifest holding both legacy and v2 sections
    Merged,
}

impl SourceFormat {
    /// Get the name this format is shown with
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // each format has a fixed name
        match self {
            SourceFormat::Legacy => "legacy",
            SourceFormat::V2 => "v2",
            SourceFormat::Merged => "legacy and v2",
        }
    }
}

/// The tool a pipeline stage was mapped to while reading a legacy manifest
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageMapping {
    /// The pipeline the stage is in
    pub pipeline: String,
    /// The stage
    pub stage: String,
    /// The tool the stage runs
    pub tool: String,
    /// The version label the stage pins (unset follows the tool's latest)
    pub version: Option<String>,
}

/// What reading the legacy part of a manifest converted, skipped, and noted
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConvertReport {
    /// The number of legacy image versions that became tool versions
    pub converted_tools: usize,
    /// The number of legacy entries that became build-only contexts
    pub converted_builds: usize,
    /// The number of legacy pipeline versions that were converted
    pub converted_pipelines: usize,
    /// The tool each converted pipeline stage maps to
    pub mappings: Vec<StageMapping>,
    /// The items that were skipped, each with its reason code
    pub skipped: Vec<SkippedItem>,
    /// Informational notes about converted items
    pub notes: Vec<MigrationNote>,
    /// Warnings that don't skip anything
    pub warnings: Vec<String>,
    /// The names that two sections of the manifest both define
    pub collisions: Vec<String>,
}

impl ConvertReport {
    /// Whether anything was skipped or any problem was raised
    #[cfg(test)]
    #[must_use]
    pub fn has_issues(&self) -> bool {
        // anything skipped, warned about, or colliding counts as an issue
        !self.skipped.is_empty() || !self.warnings.is_empty() || !self.collisions.is_empty()
    }
}

/// A manifest read from disk or a URL, in its v2 form
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The manifest, in the v2 form
    pub manifest: ToolboxManifestV2,
    /// What reading the legacy part of the manifest converted (empty for a v2 manifest)
    pub report: ConvertReport,
    /// The format the manifest was read in
    pub source: SourceFormat,
    /// The directory the manifest was read from, which relative paths inside it are relative to
    pub base_dir: Option<PathBuf>,
}

/// The build fields of a legacy image entry
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyBuildFields {
    /// The build context
    #[serde(default)]
    build_path: String,
    /// Whether the image is built (unset when the entry doesn't say)
    #[serde(default)]
    build_image: Option<bool>,
    /// The registry tags the image carries
    #[serde(default)]
    image_tags: Vec<String>,
    /// The base image settings
    #[serde(default)]
    base_image: Option<BaseImage>,
    /// The directory the entry's manifest lives in
    #[serde(default)]
    dir: String,
}

impl LegacyBuildFields {
    /// Get the build manifest these fields describe, if the entry has any build information
    fn into_build(self) -> Option<BuildManifest> {
        // an entry with no build information at all has no build
        if self.build_path.is_empty()
            && self.image_tags.is_empty()
            && self.build_image.is_none()
            && self.base_image.is_none()
        {
            return None;
        }
        // carry every field the build needs
        Some(BuildManifest {
            dir: self.dir,
            build_path: self.build_path,
            build_image: self.build_image.unwrap_or(true),
            image_tags: self.image_tags,
            base_image: self.base_image,
        })
    }
}

/// Convert a legacy pipeline config into a v2 pipeline config (dropping any stage tools)
impl From<&PipelineRequest> for PipelineConfig {
    /// Copy a pipeline request into a pipeline config
    ///
    /// # Arguments
    ///
    /// * `request` - The request to copy
    fn from(request: &PipelineRequest) -> Self {
        // the stage tools are not part of a config
        PipelineConfig {
            group: request.group.clone(),
            name: request.name.clone(),
            order: request.order.clone(),
            sla: request.sla,
            triggers: request.triggers.clone().into_iter().collect(),
            description: request.description.clone(),
        }
    }
}

/// Find the manifest file a path names, which may be a directory or its `config.toml`
///
/// # Arguments
///
/// * `path` - The path the user gave
fn locate_manifest(path: &Path) -> Result<PathBuf, Error> {
    // a config.toml stands for the directory it is in
    let dir = if path.file_name().is_some_and(|name| name == "config.toml") && path.is_file() {
        path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
    } else if path.is_dir() {
        path.to_path_buf()
    } else {
        return Ok(path.to_path_buf());
    };
    // use the first manifest file the directory holds
    MANIFEST_FILES
        .iter()
        .map(|file| dir.join(file))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            Error::new(format!(
                "Failed to find a toolbox manifest in '{}': it holds no {}; run `thorctl toolbox \
                 build` to generate one",
                dir.display(),
                MANIFEST_FILES.join(" or ")
            ))
        })
}

/// Load a toolbox manifest of either format from a path, a directory, or a URL
///
/// # Arguments
///
/// * `location` - Where the manifest is
/// * `bar` - The progress bar to track the read and any remote configs through
pub async fn load_any(location: &ManifestLocation, bar: &Bar) -> Result<Loaded, Error> {
    // read the bytes and remember the directory they came from
    let (bytes, base_dir) = match location {
        ManifestLocation::Url(url) => (shared::download_manifest_bytes(url, bar).await?, None),
        ManifestLocation::Path(path) => {
            let file = locate_manifest(path)?;
            let bytes = shared::read_manifest_bytes(&file, bar).await?;
            (bytes, file.parent().map(Path::to_path_buf))
        }
    };
    // parse them whatever the format
    parse_any(&bytes, base_dir, bar).await
}

/// Parse a toolbox manifest of either format
///
/// # Arguments
///
/// * `bytes` - The manifest's JSON
/// * `base_dir` - The directory the manifest was read from, if it was read from disk
/// * `bar` - The progress bar to track fetching remote configs of a legacy manifest through
pub async fn parse_any(
    bytes: &[u8],
    base_dir: Option<PathBuf>,
    bar: &Bar,
) -> Result<Loaded, Error> {
    // parse the document generically first so its format can be told
    let mut document: Value = serde_json::from_slice(bytes)
        .map_err(|err| Error::new(format!("Failed to parse toolbox manifest: {err}")))?;
    let Some(object) = document.as_object_mut() else {
        return Err(Error::new(
            "Failed to parse toolbox manifest: the top level must be a JSON object".to_owned(),
        ));
    };
    // refuse a format version this thorctl doesn't know
    if let Some(version) = object.get("format_version")
        && version.as_u64() != Some(u64::from(FORMAT_VERSION))
    {
        return Err(Error::new(format!(
            "Failed to parse toolbox manifest: format_version {version} is not supported (this \
             thorctl reads format_version {FORMAT_VERSION} and legacy manifests)"
        )));
    }
    // split off the legacy sections: `images`, and the pipelines without a `versions` key
    let legacy_images = object.remove("images");
    let mut legacy_pipelines = Map::new();
    if let Some(Value::Object(pipelines)) = object.get_mut("pipelines") {
        let legacy_names: Vec<String> = pipelines
            .iter()
            .filter(|(_, pipeline)| pipeline.get("versions").is_none())
            .map(|(name, _)| name.clone())
            .collect();
        for name in legacy_names {
            if let Some(pipeline) = pipelines.remove(&name) {
                legacy_pipelines.insert(name, pipeline);
            }
        }
    }
    // a document is v2 when it carries v2 markers or has no legacy sections at all
    let has_legacy = legacy_images.is_some() || !legacy_pipelines.is_empty();
    let has_v2 = object.contains_key("format_version")
        || V2_SECTIONS.iter().any(|key| object.contains_key(*key))
        || object
            .get("pipelines")
            .and_then(Value::as_object)
            .is_some_and(|pipelines| !pipelines.is_empty());
    let source = match (has_legacy, has_v2) {
        (true, false) => SourceFormat::Legacy,
        (true, true) => SourceFormat::Merged,
        (false, _) => SourceFormat::V2,
    };
    // read the v2 part; a legacy only document contributes only its top level keys
    let mut v2_document = object.clone();
    if source == SourceFormat::Legacy {
        v2_document.remove("pipelines");
    }
    let mut manifest: ToolboxManifestV2 = serde_json::from_value(Value::Object(v2_document))
        .map_err(|err| Error::new(format!("Failed to parse toolbox manifest: {err}")))?;
    let mut report = ConvertReport::default();
    // project the legacy part and merge it in
    if has_legacy {
        let legacy = read_legacy(&manifest, legacy_images, legacy_pipelines, bar).await?;
        merge_legacy(&mut manifest, &mut report, legacy);
    }
    Ok(Loaded {
        manifest,
        report,
        source,
        base_dir,
    })
}

/// The legacy part of a manifest, projected into v2 sections
#[derive(Default)]
struct LegacyProjection {
    /// The tools the legacy images became
    tools: BTreeMap<String, ToolManifest>,
    /// The build-only contexts the legacy images became
    builds: BTreeMap<String, BTreeMap<String, BuildManifest>>,
    /// The pipelines
    pipelines: BTreeMap<String, PipelineManifestV2>,
    /// The deduplicated network policies the images bundle
    policies: Vec<NetworkPolicyRequest>,
    /// What the projection converted, skipped, and noted
    report: ConvertReport,
    /// The labels each converted tool kept, which pipeline stages resolve their pins against
    tool_labels: BTreeMap<String, BTreeSet<String>>,
    /// The tool each image config name maps to, which pipeline stages may name instead of a tool
    config_names: BTreeMap<String, String>,
    /// The names of the network policies already collected
    policy_names: BTreeSet<String>,
}

/// Read the legacy sections of a manifest and project them into v2 sections
///
/// # Arguments
///
/// * `shell` - The top level of the manifest (its name and settings)
/// * `images` - The legacy `images` section, if any
/// * `pipelines` - The legacy pipeline entries
/// * `bar` - The progress bar to track fetching remote configs through
async fn read_legacy(
    shell: &ToolboxManifestV2,
    images: Option<Value>,
    pipelines: Map<String, Value>,
    bar: &Bar,
) -> Result<LegacyProjection, Error> {
    // read each image entry's build fields before the typed read drops them
    let mut build_fields: BTreeMap<(String, String), LegacyBuildFields> = BTreeMap::new();
    if let Some(Value::Object(entries)) = &images {
        for (name, versions) in entries {
            for (label, entry) in versions.as_object().into_iter().flatten() {
                let fields = serde_json::from_value(entry.clone()).map_err(|err| {
                    Error::new(format!(
                        "Failed to parse legacy image '{name}@{label}': {err}"
                    ))
                })?;
                build_fields.insert((name.clone(), label.clone()), fields);
            }
        }
    }
    // read the legacy manifest with the same types the other toolbox commands use
    let mut legacy: ToolboxManifest = serde_json::from_value(serde_json::json!({
        "name": shell.name,
        "images": images.unwrap_or_else(|| Value::Object(Map::new())),
        "pipelines": Value::Object(pipelines),
    }))
    .map_err(|err| Error::new(format!("Failed to parse legacy toolbox manifest: {err}")))?;
    // fetch any remote configs and network policies the entries point at
    shared::resolve_manifest_configs(&mut legacy, bar).await?;
    Ok(project_legacy(&legacy, &build_fields))
}

/// Get the labels of an image manifest in a stable order
///
/// # Arguments
///
/// * `image` - The image manifest
fn sorted_labels(image: &ImageManifest) -> Vec<&String> {
    // the legacy map is unordered, so sort the labels
    let mut labels: Vec<&String> = image.versions.keys().collect();
    labels.sort();
    labels
}

/// Get why a legacy image version can't become a tool, if it can't
///
/// # Arguments
///
/// * `name` - The image's name
/// * `label` - The version's label
/// * `version` - The legacy image version
fn skip_reason(name: &str, label: &str, version: &ImageVersion) -> Option<MigrationReason> {
    // a name or label that can't be referred to can't become a tool
    if check_name(name).is_err() {
        return Some(MigrationReason::invalid_value("name"));
    }
    if check_label(label).is_err() {
        return Some(MigrationReason::invalid_value("version"));
    }
    // a container image tool needs the image to run
    let config = version.config.as_ref()?;
    if matches!(config.scaler, ImageScaler::K8s | ImageScaler::Windows)
        && config.image.as_deref().is_none_or(str::is_empty)
    {
        return Some(MigrationReason::missing_field("image"));
    }
    None
}

impl LegacyProjection {
    /// Record the description of a legacy version on its tool, keeping the first one
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool being built
    /// * `first` - The label whose description the tool kept, once one was kept
    /// * `name` - The image's name
    /// * `label` - The version's label
    /// * `config` - The version's image config
    fn note_description(
        &mut self,
        tool: &mut ToolManifest,
        first: &mut Option<String>,
        name: &str,
        label: &str,
        config: &ImageRequest,
    ) {
        // an empty description says nothing
        let Some(description) = config.description.as_ref().filter(|text| !text.is_empty()) else {
            return;
        };
        match first {
            // the first description is the tool's
            None => {
                tool.description = Some(description.clone());
                *first = Some(label.to_owned());
            }
            // a later description that differs is noted, not used
            Some(kept) if tool.description.as_ref() != Some(description) => {
                self.report.notes.push(MigrationNote::new(
                    config.group.clone(),
                    name,
                    format!(
                        "the description of {label} differs from {kept}; kept the description of {kept}"
                    ),
                ));
            }
            Some(_) => {}
        }
    }

    /// Project one legacy image into a tool, build-only contexts, or skipped items
    ///
    /// # Arguments
    ///
    /// * `name` - The image's name
    /// * `image` - The legacy image
    /// * `build_fields` - The build fields of each `(image, label)` entry
    fn add_image(
        &mut self,
        name: &str,
        image: &ImageManifest,
        build_fields: &BTreeMap<(String, String), LegacyBuildFields>,
    ) {
        let mut tool = ToolManifest::default();
        let mut builds: BTreeMap<String, BuildManifest> = BTreeMap::new();
        let mut groups: BTreeSet<String> = BTreeSet::new();
        let mut description_from: Option<String> = None;
        for label in sorted_labels(image) {
            let version = &image.versions[label];
            let build = build_fields
                .get(&(name.to_owned(), label.clone()))
                .cloned()
                .and_then(LegacyBuildFields::into_build);
            let item = format!("image {name}@{label}");
            let group = version
                .config
                .as_ref()
                .map_or_else(String::new, |config| config.group.clone());
            // an entry with build information and no config is a build-only context
            if version.config.is_none() && version.config_from.is_none() {
                match build {
                    Some(build) => {
                        builds.insert(label.clone(), build);
                    }
                    None => self.report.skipped.push(SkippedItem::new(
                        group,
                        item,
                        MigrationReason::missing_field("config"),
                    )),
                }
                continue;
            }
            // an entry that can't become a tool is skipped with its reason
            if let Some(reason) = skip_reason(name, label, version) {
                self.report
                    .skipped
                    .push(SkippedItem::new(group, item, reason));
                continue;
            }
            // note the tool's group and description, and the name its stages may use for it
            if let Some(config) = &version.config {
                groups.insert(config.group.clone());
                self.note_description(&mut tool, &mut description_from, name, label, config);
                self.config_names
                    .entry(config.name.clone())
                    .or_insert_with(|| name.to_owned());
            }
            // bundle the version's own network policies, deduplicated across the toolbox
            for policy in &version.network_policies {
                if self.policy_names.insert(policy.name.clone()) {
                    self.policies.push(policy.clone());
                }
            }
            tool.versions.insert(
                label.clone(),
                ToolVersionManifest {
                    dir: version.dir.clone(),
                    spec: None,
                    spec_from: None,
                    build,
                    provides: Vec::new(),
                    network_policies_from: version.network_policies_from.clone(),
                    network_policies: version.network_policies.clone(),
                    legacy: Some(LegacyImage {
                        config: version.config.clone(),
                    }),
                },
            );
        }
        // a tool whose versions disagree on their group can't be one tool
        if groups.len() > 1 {
            self.report.collisions.push(name.to_owned());
        } else if !tool.versions.is_empty() {
            tool.groups = groups.into_iter().collect();
            self.report.converted_tools += tool.versions.len();
            self.tool_labels
                .insert(name.to_owned(), tool.versions.keys().cloned().collect());
            self.tools.insert(name.to_owned(), tool);
        }
        if !builds.is_empty() {
            self.report.converted_builds += builds.len();
            self.builds.insert(name.to_owned(), builds);
        }
    }
}

/// Project legacy images and pipelines into tools, builds, and pipelines
///
/// # Arguments
///
/// * `legacy` - The legacy manifest, with its remote configs resolved
/// * `build_fields` - The build fields of each `(image, label)` entry
fn project_legacy(
    legacy: &ToolboxManifest,
    build_fields: &BTreeMap<(String, String), LegacyBuildFields>,
) -> LegacyProjection {
    let mut projection = LegacyProjection::default();
    // project each image in name order
    let mut image_names: Vec<&String> = legacy.images.keys().collect();
    image_names.sort();
    for name in image_names {
        projection.add_image(name, &legacy.images[name], build_fields);
    }
    // pipelines resolve their stages against the tools that were converted
    project_pipelines(legacy, &mut projection);
    projection
}

/// Project the legacy pipelines into v2 pipelines whose stages name tools
///
/// # Arguments
///
/// * `legacy` - The legacy manifest
/// * `projection` - The projection holding the converted tools, to add the pipelines and mappings to
fn project_pipelines(legacy: &ToolboxManifest, projection: &mut LegacyProjection) {
    let mut pipeline_names: Vec<&String> = legacy.pipelines.keys().collect();
    pipeline_names.sort();
    for name in pipeline_names {
        let mut labels: Vec<&String> = legacy.pipelines[name].versions.keys().collect();
        labels.sort();
        for label in labels {
            let version = &legacy.pipelines[name].versions[label];
            let item = format!("pipeline {name}@{label}");
            let group = version
                .config
                .as_ref()
                .map_or_else(String::new, |config| config.group.clone());
            // a pipeline needs its config to be anything
            if version.config.is_none() && version.config_from.is_none() {
                projection.report.skipped.push(SkippedItem::new(
                    group,
                    item,
                    MigrationReason::missing_field("config"),
                ));
                continue;
            }
            let config = version.config.as_ref().map(PipelineConfig::from);
            // every stage the pipeline pins or orders needs a converted tool
            let mut stage_names: BTreeSet<String> = version.images.keys().cloned().collect();
            if let Some(stages) = config.as_ref().and_then(PipelineConfig::stages) {
                stage_names.extend(stages);
            }
            let mut stage_tools: BTreeMap<String, StageTool> = BTreeMap::new();
            let mut mappings: Vec<StageMapping> = Vec::new();
            let mut unresolved = false;
            for stage in &stage_names {
                // a stage names its tool by manifest key or by the image's config name
                let tool = if projection.tool_labels.contains_key(stage) {
                    Some(stage.clone())
                } else {
                    projection.config_names.get(stage).cloned()
                };
                let pinned = version.images.get(stage).map(|image| image.version.clone());
                // a tool that was dropped (for a group collision, say) backs no stage, and a pin
                // must name a label the tool kept
                let resolved = tool.filter(|tool| {
                    projection.tool_labels.get(tool).is_some_and(|labels| {
                        pinned.as_ref().is_none_or(|label| labels.contains(label))
                    })
                });
                match resolved {
                    Some(tool) => {
                        mappings.push(StageMapping {
                            pipeline: name.clone(),
                            stage: stage.clone(),
                            tool: tool.clone(),
                            version: pinned.clone(),
                        });
                        stage_tools.insert(
                            stage.clone(),
                            StageTool {
                                tool,
                                version: pinned,
                            },
                        );
                    }
                    None => unresolved = true,
                }
            }
            // a pipeline with a stage no tool backs can't run, so it is dropped
            if unresolved {
                projection.report.skipped.push(SkippedItem::new(
                    group,
                    item,
                    MigrationReason::invalid_value("stages.*"),
                ));
                continue;
            }
            projection.report.converted_pipelines += 1;
            projection.report.mappings.extend(mappings);
            projection
                .pipelines
                .entry(name.clone())
                .or_default()
                .versions
                .insert(
                    label.clone(),
                    PipelineVersionV2 {
                        dir: version.dir.clone(),
                        description: version.description.clone(),
                        config,
                        config_from: version.config_from.clone(),
                        stage_tools,
                        incomplete: false,
                    },
                );
        }
    }
}

/// Move the entries of one projected legacy section into the manifest's section
///
/// # Arguments
///
/// * `target` - The manifest's section
/// * `source` - The projected legacy section
/// * `collisions` - Where names both sections define are recorded
fn merge_sections<T>(
    target: &mut BTreeMap<String, T>,
    source: BTreeMap<String, T>,
    collisions: &mut Vec<String>,
) {
    // an entry the target already has is a collision and keeps the target's definition
    for (name, entry) in source {
        match target.entry(name) {
            Entry::Occupied(taken) => collisions.push(taken.key().clone()),
            Entry::Vacant(slot) => {
                slot.insert(entry);
            }
        }
    }
}

/// Merge the projected legacy sections into a manifest, reporting names both define
///
/// # Arguments
///
/// * `manifest` - The manifest to merge into
/// * `report` - The report to record the merge in
/// * `legacy` - The projected legacy sections
fn merge_legacy(
    manifest: &mut ToolboxManifestV2,
    report: &mut ConvertReport,
    legacy: LegacyProjection,
) {
    // start from what the projection reported
    *report = legacy.report;
    // a name that is both a legacy item and a v2 item is a collision; the v2 entry wins
    merge_sections(&mut manifest.tools, legacy.tools, &mut report.collisions);
    merge_sections(&mut manifest.builds, legacy.builds, &mut report.collisions);
    merge_sections(
        &mut manifest.pipelines,
        legacy.pipelines,
        &mut report.collisions,
    );
    // add the bundled network policies that the manifest doesn't already define
    for policy in legacy.policies {
        if !manifest
            .network_policies
            .iter()
            .any(|existing| existing.name == policy.name)
        {
            manifest.network_policies.push(policy);
        }
    }
    report.collisions.sort();
    report.collisions.dedup();
}

/// A source (a spec, a pipeline config, or a network policy) that could not be fetched or read
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFailure {
    /// The item the source belongs to, such as `tool clamav@latest`
    pub item: String,
    /// The field that names the source (`spec_from`, `config_from`, or `network_policies_from`)
    pub field: String,
    /// Why the source could not be used
    pub message: String,
}

/// Read the text a `spec_from` or `config_from` value points at
///
/// # Arguments
///
/// * `source` - An http(s) URL or a path
/// * `base_dir` - The directory relative paths are relative to
async fn read_source(source: &str, base_dir: Option<&Path>) -> Result<String, String> {
    // a URL is downloaded; anything else is a path
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = reqwest::get(source)
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| format!("failed to download '{source}': {err}"))?;
        return response
            .text()
            .await
            .map_err(|err| format!("failed to read '{source}': {err}"));
    }
    // relative paths are relative to the manifest
    let path = Path::new(source);
    let path = match base_dir {
        Some(base) if path.is_relative() => base.join(path),
        _ => path.to_path_buf(),
    };
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))
}

/// Read a JSON or YAML document from a source into a type
///
/// # Arguments
///
/// * `source` - An http(s) URL or a path
/// * `base_dir` - The directory relative paths are relative to
async fn read_document<T: serde::de::DeserializeOwned>(
    source: &str,
    base_dir: Option<&Path>,
) -> Result<T, String> {
    // read the text, then parse it as YAML (which also reads JSON)
    let text = read_source(source, base_dir).await?;
    serde_norway::from_str(&text).map_err(|err| format!("failed to parse '{source}': {err}"))
}

/// Fetch every `spec_from`, `config_from`, and `network_policies_from` a v2 manifest names
///
/// Each source that is read replaces its pointer (`spec_from` becomes `spec`, `config_from`
/// becomes `config`). Sources that can't be read are returned, and their pointers are left in
/// place.
///
/// # Arguments
///
/// * `manifest` - The manifest to resolve in place
/// * `base_dir` - The directory relative paths are relative to
pub async fn resolve_sources(
    manifest: &mut ToolboxManifestV2,
    base_dir: Option<&Path>,
) -> Vec<SourceFailure> {
    let mut failures = Vec::new();
    // resolve every tool version's spec and policy sources
    for (name, tool) in &mut manifest.tools {
        for (label, version) in &mut tool.versions {
            let item = format!("tool {name}@{label}");
            if version.spec.is_none()
                && let Some(source) = version.spec_from.clone()
            {
                match read_document::<Value>(&source, base_dir).await {
                    Ok(spec) => {
                        version.spec = Some(spec);
                        version.spec_from = None;
                    }
                    Err(message) => failures.push(SourceFailure {
                        item: item.clone(),
                        field: "spec_from".to_owned(),
                        message,
                    }),
                }
            }
            resolve_policies(&item, version, base_dir, &mut failures).await;
        }
    }
    // resolve every pipeline version's config source
    for (name, pipeline) in &mut manifest.pipelines {
        for (label, version) in &mut pipeline.versions {
            if version.config.is_none()
                && let Some(source) = version.config_from.clone()
            {
                match read_document::<PipelineRequest>(&source, base_dir).await {
                    Ok(request) => {
                        version.config = Some(PipelineConfig::from(&request));
                        version.config_from = None;
                    }
                    Err(message) => failures.push(SourceFailure {
                        item: format!("pipeline {name}@{label}"),
                        field: "config_from".to_owned(),
                        message,
                    }),
                }
            }
        }
    }
    failures
}

/// Fetch a version's `network_policies_from` sources into its bundled policies
///
/// # Arguments
///
/// * `item` - The item the version is, for reporting
/// * `version` - The version to resolve in place
/// * `base_dir` - The directory relative paths are relative to
/// * `failures` - Where sources that can't be read are recorded
async fn resolve_policies(
    item: &str,
    version: &mut ToolVersionManifest,
    base_dir: Option<&Path>,
    failures: &mut Vec<SourceFailure>,
) {
    // read each source, keeping the ones that fail so they are reported and can be retried
    let sources = std::mem::take(&mut version.network_policies_from);
    for source in sources {
        match read_document::<NetworkPolicyRequest>(&source, base_dir).await {
            Ok(policy) => version.network_policies.push(policy),
            Err(message) => {
                failures.push(SourceFailure {
                    item: item.to_owned(),
                    field: "network_policies_from".to_owned(),
                    message,
                });
                version.network_policies_from.push(source);
            }
        }
    }
}
