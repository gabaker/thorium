//! Converting a legacy toolbox into the current format
//!
//! The loader projects a legacy manifest into tools, builds, and pipelines but leaves each
//! tool's spec to be built from the image config it remembers. This module builds those specs
//! with the same conversion the migration of stored images uses, rewrites dependency names
//! into tool references, and re-checks the pipelines against the tools that survived.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use thorium::Error;
use thorium::client::migrations::{
    MigrationNote, MigrationReason, SkippedItem, ToolIdMap, convert_image_request,
    resolve_request_stages,
};
use thorium::models::{ImageRequest, PipelineRequest, ToolKind, ToolVersion};
use uuid::Uuid;

use super::report::{ConvertIssue, ConvertNote, ConvertSummary, issue_code, issue_meaning};
use crate::args::toolbox::ConvertToolbox;
use crate::handlers::progress::Bar;
use crate::handlers::toolbox::v2::load::{Loaded, SourceFormat, StageMapping, parse_any};
use crate::handlers::toolbox::v2::refs::{Ref, unresolve_slots};
use crate::handlers::toolbox::v2::types::{
    PipelineConfig, PipelineVersionV2, StageTool, ToolboxManifestV2,
};

/// The choices that change what a conversion produces
#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// The group to put every tool and pipeline in instead of their own
    pub group_override: Option<String>,
    /// Whether pipeline stages follow their tool's latest version instead of pinning one
    pub follow_latest: bool,
    /// Whether an image entry with no `image` takes the first of its build tags
    pub fill_image_from_tags: bool,
    /// Whether a pipeline with an unresolved stage is kept, flagged incomplete
    pub keep_incomplete_pipelines: bool,
}

impl From<&ConvertToolbox> for ConvertOptions {
    /// Take the conversion choices from the command line
    ///
    /// # Arguments
    ///
    /// * `cmd` - The `toolbox convert` command
    fn from(cmd: &ConvertToolbox) -> Self {
        // copy the four flags that shape the conversion
        ConvertOptions {
            group_override: cmd.group_override.clone(),
            follow_latest: cmd.follow_latest,
            fill_image_from_tags: cmd.fill_image_from_tags,
            keep_incomplete_pipelines: cmd.keep_incomplete_pipelines,
        }
    }
}

/// A converted toolbox and what the conversion reported
#[derive(Debug, Clone)]
pub struct Converted {
    /// The manifest in the current format
    pub manifest: ToolboxManifestV2,
    /// What the conversion produced and reported
    pub summary: ConvertSummary,
}

/// What preparing a legacy document before it is loaded changed
#[derive(Debug, Default)]
struct Prepared {
    /// The notes about image entries that were given an image
    notes: Vec<ConvertNote>,
    /// The names of the pipelines that came from the legacy part of the document
    legacy_pipelines: BTreeSet<String>,
    /// The images whose versions name more than one group, which were left out of the document
    collisions: Vec<String>,
}

/// One legacy image version to convert into a tool version
#[derive(Debug)]
struct Candidate {
    /// The image's name, which is the tool's name
    name: String,
    /// The version's label
    label: String,
    /// The group the tool is in
    group: String,
    /// The image config to convert
    request: ImageRequest,
}

/// Whether a legacy scaler runs a container image that the entry must name
///
/// # Arguments
///
/// * `config` - The legacy image config as JSON
fn needs_container_image(config: &Map<String, Value>) -> bool {
    // an entry that doesn't name its scaler uses the default, which is Kubernetes
    config
        .get("scaler")
        .and_then(Value::as_str)
        .is_none_or(|scaler| matches!(scaler, "K8s" | "Windows"))
}

/// Find the images whose convertible versions are in more than one group
///
/// # Arguments
///
/// * `images` - The legacy `images` section
fn collided_images(images: &Map<String, Value>) -> BTreeSet<String> {
    // check each image on its own
    let mut collided = BTreeSet::new();
    for (name, labels) in images {
        // the groups of the versions that can become tool versions
        let groups: BTreeSet<&str> = labels
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(_, entry)| entry.get("config").and_then(Value::as_object))
            .filter(|config| {
                let has_image = config
                    .get("image")
                    .and_then(Value::as_str)
                    .is_some_and(|image| !image.is_empty());
                has_image || !needs_container_image(config)
            })
            .filter_map(|config| config.get("group").and_then(Value::as_str))
            .collect();
        // more than one group means the versions can't be one tool
        if groups.len() > 1 {
            collided.insert(name.clone());
        }
    }
    collided
}

/// Apply the group override and the image fill to the legacy sections of a document
///
/// # Arguments
///
/// * `document` - The manifest document, edited in place
/// * `options` - The choices that shape the conversion
fn prepare_document(document: &mut Value, options: &ConvertOptions) -> Prepared {
    let mut prepared = Prepared::default();
    // only an object can hold sections to prepare
    let Some(object) = document.as_object_mut() else {
        return prepared;
    };
    // walk every image version that carries a config
    if let Some(Value::Object(images)) = object.get_mut("images") {
        for (name, labels) in images.iter_mut() {
            for (label, entry) in labels.as_object_mut().into_iter().flatten() {
                // the first build tag is what an entry without an image can be filled from
                let tag = entry
                    .get("image_tags")
                    .and_then(|tags| tags.get(0))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let Some(config) = entry.get_mut("config").and_then(Value::as_object_mut) else {
                    continue;
                };
                // every tool goes in the override group when there is one
                if let Some(group) = &options.group_override {
                    config.insert("group".to_owned(), Value::String(group.clone()));
                }
                // an entry for a container scaler with no image takes its first tag when asked
                let missing = config
                    .get("image")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty);
                if let (true, true, true, Some(tag)) = (
                    options.fill_image_from_tags,
                    missing,
                    needs_container_image(config),
                    tag,
                ) {
                    prepared.notes.push(ConvertNote {
                        code: "note:filled:image".to_owned(),
                        item: format!("image {name}@{label}"),
                        message: format!("had no image, so its first build tag `{tag}` was used"),
                    });
                    config.insert("image".to_owned(), Value::String(tag));
                }
            }
        }
    }
    // an image whose versions are in different groups can't be one tool, so it is set aside for
    // the report; the pipelines that stage it then have no tool to resolve to
    if let Some(Value::Object(images)) = object.get_mut("images") {
        let collided = collided_images(images);
        images.retain(|name, _| !collided.contains(name));
        prepared.collisions = collided.into_iter().collect();
    }
    // a pipeline without a `versions` key is a legacy pipeline
    if let Some(Value::Object(pipelines)) = object.get_mut("pipelines") {
        for (name, labels) in pipelines.iter_mut() {
            // a pipeline with versions is already current
            if labels.get("versions").is_some() {
                continue;
            }
            prepared.legacy_pipelines.insert(name.clone());
            // its config moves to the override group too
            let Some(group) = &options.group_override else {
                continue;
            };
            // every version's config
            for entry in labels
                .as_object_mut()
                .into_iter()
                .flatten()
                .map(|(_, entry)| entry)
            {
                if let Some(config) = entry.get_mut("config").and_then(Value::as_object_mut) {
                    config.insert("group".to_owned(), Value::String(group.clone()));
                }
            }
        }
    }
    // hand back what the report needs to know
    prepared
}

/// Name the image version a conversion issue is about
///
/// # Arguments
///
/// * `name` - The image's name
/// * `label` - The version's label
fn image_item(name: &str, label: &str) -> String {
    // the same naming the loader's skipped items use
    format!("image {name}@{label}")
}

/// Build an issue from a code
///
/// # Arguments
///
/// * `group` - The group of the item
/// * `item` - The item the issue is about
/// * `code` - The issue's code
/// * `skipped` - Whether the item was left out of the converted toolbox
fn issue(group: &str, item: String, code: String, skipped: bool) -> ConvertIssue {
    // the message follows from the code
    ConvertIssue {
        message: issue_meaning(&code),
        code,
        group: group.to_owned(),
        item,
        skipped,
    }
}

/// Get the issue a skipped item stands for
///
/// # Arguments
///
/// * `skipped` - The skipped item
/// * `item` - The name to give the item, when it is more specific than the skipped item's
/// * `dropped` - Whether the item was left out of the converted toolbox
fn issue_of(skipped: &SkippedItem, item: Option<String>, dropped: bool) -> ConvertIssue {
    // the code is the migration's, with a dropped dependency name shown as such
    issue(
        &skipped.group,
        item.unwrap_or_else(|| skipped.item.clone()),
        issue_code(&skipped.reason),
        dropped,
    )
}

/// Get the code a note is shown with
///
/// # Arguments
///
/// * `note` - The note's text
fn note_code(note: &str) -> &'static str {
    // the conversion words its notes in a few fixed ways
    if note.starts_with("dropped fields") {
        "note:dropped-field"
    } else if note.starts_with("dropped `") {
        "note:dropped-dependency"
    } else {
        "note:conversion"
    }
}

/// Convert a migration's note into a report note
///
/// # Arguments
///
/// * `item` - The item to name in the note
/// * `code` - The note's code
/// * `note` - The migration's note
fn report_note(item: String, code: &str, note: &MigrationNote) -> ConvertNote {
    // the text is the migration's
    ConvertNote {
        code: code.to_owned(),
        item,
        message: note.note.clone(),
    }
}

/// Collect the legacy image versions that are to become tool versions
///
/// A version with no config to convert is removed and reported.
///
/// # Arguments
///
/// * `manifest` - The manifest holding the projected tools
/// * `issues` - Where issues are recorded
fn candidates(manifest: &mut ToolboxManifestV2, issues: &mut Vec<ConvertIssue>) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut empty_tools = Vec::new();
    // look at every version of every tool
    for (name, tool) in &mut manifest.tools {
        let mut gone = Vec::new();
        for (label, version) in &tool.versions {
            // only versions that came from a legacy image are converted
            let Some(legacy) = &version.legacy else {
                continue;
            };
            match &legacy.config {
                Some(request) => found.push(Candidate {
                    name: name.clone(),
                    label: label.clone(),
                    group: request.group.clone(),
                    request: request.clone(),
                }),
                // an entry whose config never resolved has nothing to convert
                None => {
                    issues.push(issue(
                        tool.groups.first().map_or("", String::as_str),
                        image_item(name, label),
                        MigrationReason::missing_field("config").code(),
                        true,
                    ));
                    gone.push(label.clone());
                }
            }
        }
        // the versions with nothing to convert are not versions
        for label in gone {
            tool.versions.remove(&label);
        }
        // and a tool with no version left is not a tool
        if tool.versions.is_empty() {
            empty_tools.push(name.clone());
        }
    }
    for name in empty_tools {
        manifest.tools.remove(&name);
    }
    found
}

/// Build the specs of the legacy tool versions
///
/// # Arguments
///
/// * `manifest` - The manifest holding the projected tools, whose specs are built in place
/// * `summary` - Where issues, warnings, and notes are recorded
fn build_specs(manifest: &mut ToolboxManifestV2, summary: &mut ConvertSummary) {
    // gather the versions to convert
    let found = candidates(manifest, &mut summary.issues);
    // name every image that is to be converted, then drop those whose dependency lists would
    // empty (which can empty the lists of the images that name them in turn)
    let mut map = ToolIdMap::from_names(
        found
            .iter()
            .map(|candidate| (candidate.group.as_str(), candidate.name.as_str())),
    );
    let mut settled: BTreeMap<(String, String), MigrationReason> = BTreeMap::new();
    map.settle_dependencies(
        found.iter().map(|candidate| {
            (
                candidate.group.as_str(),
                candidate.name.as_str(),
                &candidate.request.dependencies,
            )
        }),
        &mut settled,
    );
    // a tool whose dependencies can't be satisfied is left out entirely
    for ((group, name), reason) in &settled {
        manifest.tools.remove(name);
        summary.issues.push(issue(
            group,
            format!("image {name}"),
            issue_code(reason),
            true,
        ));
    }
    // the reverse of the map turns the ids a conversion writes back into tool names
    let names: HashMap<Uuid, String> = map
        .iter()
        .map(|(_, name, id)| (id, name.to_owned()))
        .collect();
    // convert each version whose tool survived
    for candidate in found {
        if map.get(&candidate.group, &candidate.name).is_none() {
            continue;
        }
        let item = image_item(&candidate.name, &candidate.label);
        let label = ToolVersion::from(candidate.label.as_str());
        match convert_image_request(
            &candidate.group,
            &candidate.name,
            label,
            &candidate.request,
            &map,
        ) {
            Ok(migration) => {
                // the spec as JSON, with the tool ids it holds written as references to tools
                let mut spec = serde_json::to_value(&migration.tool.spec).unwrap_or(Value::Null);
                unresolve_slots(&mut spec, |id, _| {
                    names.get(&id).map(|name| Ref::Tool {
                        name: name.clone(),
                        label: None,
                    })
                });
                // the spec replaces the legacy config the version remembered
                if let Some(version) = manifest
                    .tools
                    .get_mut(&candidate.name)
                    .and_then(|tool| tool.versions.get_mut(&candidate.label))
                {
                    version.spec = Some(spec);
                    version.legacy = None;
                }
                // dropped dependency names are issues, and the rest is warned about or noted
                for dropped in &migration.issues {
                    // a dropped name keeps the tool
                    summary
                        .issues
                        .push(issue_of(dropped, Some(item.clone()), false));
                }
                for warning in &migration.warnings {
                    // a secret-looking env name is named, never its value
                    summary
                        .warnings
                        .push(report_note(item.clone(), "warning:secret-env", warning));
                }
                for note in &migration.notes {
                    // informational notes keep their text
                    summary
                        .notes
                        .push(report_note(item.clone(), note_code(&note.note), note));
                }
            }
            // a version the conversion rejects is left out, with its reason
            Err(skipped) => {
                summary.issues.push(issue_of(&skipped, Some(item), true));
                if let Some(tool) = manifest.tools.get_mut(&candidate.name) {
                    tool.versions.remove(&candidate.label);
                }
            }
        }
    }
    // a tool with no version left is not a tool, and the rest are Image tools
    manifest.tools.retain(|_, tool| !tool.versions.is_empty());
    for tool in manifest.tools.values_mut() {
        tool.kind = Some(ToolKind::Image);
    }
}

/// Get the label of the tool version a stage pins is missing, if it pins one that isn't there
///
/// # Arguments
///
/// * `manifest` - The converted manifest
/// * `stage` - The stage's tool
fn pin_is_missing(manifest: &ToolboxManifestV2, stage: &StageTool) -> bool {
    // a pin names a version the tool must have
    match (&stage.version, manifest.tools.get(&stage.tool)) {
        (Some(label), Some(tool)) => !tool.versions.contains_key(label),
        (None, Some(_)) => false,
        (_, None) => true,
    }
}

/// Re-check the legacy pipelines against the tools that were converted
///
/// A pipeline whose stages name a tool that is gone, that is in another group, or that lacks the
/// pinned version is left out (or kept flagged when asked).
///
/// # Arguments
///
/// * `manifest` - The converted manifest, whose pipelines are checked in place
/// * `legacy` - The names of the pipelines that came from legacy entries
/// * `options` - The choices that shape the conversion
/// * `issues` - Where issues are recorded
fn recheck_pipelines(
    manifest: &mut ToolboxManifestV2,
    legacy: &BTreeSet<String>,
    options: &ConvertOptions,
    issues: &mut Vec<ConvertIssue>,
) {
    // the tools that exist now, by group and name
    let map = ToolIdMap::from_names(manifest.tools.iter().flat_map(|(name, tool)| {
        tool.groups
            .iter()
            .map(move |group| (group.as_str(), name.as_str()))
    }));
    // find the pipeline versions with stages that no tool can run
    let mut verdicts: Vec<(String, String, String, Vec<String>)> = Vec::new();
    for (name, pipeline) in &manifest.pipelines {
        // only pipelines converted from legacy entries are checked
        if !legacy.contains(name) {
            continue;
        }
        for (label, version) in &pipeline.versions {
            // the stages whose tools can't run them
            let group = version
                .config
                .as_ref()
                .map_or("", |config| config.group.as_str());
            let tools = version
                .stage_tools
                .values()
                .map(|stage| stage.tool.as_str());
            let resolution = resolve_request_stages(group, name, tools, &map);
            // a stage fails when its tool is gone, in another group, or lacks the pinned version
            let unresolved: Vec<String> = version
                .stage_tools
                .iter()
                .filter(|(_, stage)| {
                    pin_is_missing(manifest, stage)
                        || (version.config.is_some()
                            && !resolution.stages.contains_key(&stage.tool))
                })
                .map(|(stage, _)| stage.clone())
                .collect();
            // remember the version for the second pass, which edits the manifest
            if !unresolved.is_empty() {
                verdicts.push((name.clone(), label.clone(), group.to_owned(), unresolved));
            }
        }
    }
    // keep or drop each failing version, reporting it either way
    for (name, label, group, unresolved) in verdicts {
        let item = format!("pipeline {name}@{label}");
        let code = MigrationReason::invalid_value("stages.*").code();
        if options.keep_incomplete_pipelines {
            // keep the pipeline with the stages that do resolve, flagged
            if let Some(version) = manifest
                .pipelines
                .get_mut(&name)
                .and_then(|pipeline| pipeline.versions.get_mut(&label))
            {
                for stage in &unresolved {
                    version.stage_tools.remove(stage);
                }
                version.incomplete = true;
            }
            // it is still an issue, so strict conversions refuse it, but it is not skipped
            issues.push(issue(&group, item, code, false));
        } else {
            // leave the pipeline out
            if let Some(pipeline) = manifest.pipelines.get_mut(&name) {
                pipeline.versions.remove(&label);
            }
            issues.push(issue(&group, item, code, true));
        }
    }
    // a pipeline with no version left is gone
    manifest
        .pipelines
        .retain(|_, pipeline| !pipeline.versions.is_empty());
}

/// Rebuild a legacy pipeline version as an incomplete pipeline, keeping the stages that resolve
///
/// Returns `None` when the pipeline's config is not in the document itself.
///
/// # Arguments
///
/// * `manifest` - The converted manifest holding the tools the stages resolve against
/// * `legacy` - The legacy pipeline version as it is in the document
fn rebuild_incomplete(manifest: &ToolboxManifestV2, legacy: &Value) -> Option<PipelineVersionV2> {
    // the config must be inline for the pipeline to be rebuilt
    let request = legacy
        .get("config")
        .and_then(|config| serde_json::from_value::<PipelineRequest>(config.clone()).ok())?;
    let config = PipelineConfig::from(&request);
    // every stage the pipeline orders or pins
    let mut stage_names: BTreeSet<String> = legacy
        .get("images")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|images| images.keys().cloned())
        .collect();
    stage_names.extend(config.stages().unwrap_or_default());
    // keep the stages whose tool and pinned version exist
    let mut stage_tools = BTreeMap::new();
    for stage in stage_names {
        let version = legacy
            .get("images")
            .and_then(|images| images.get(&stage))
            .and_then(|image| image.get("version"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let tool = StageTool {
            tool: stage.clone(),
            version,
        };
        if !pin_is_missing(manifest, &tool) {
            stage_tools.insert(stage, tool);
        }
    }
    // the text fields are the legacy pipeline's
    let text = |key: &str| {
        legacy
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(PipelineVersionV2 {
        dir: text("dir"),
        description: text("description"),
        config: Some(config),
        config_from: None,
        stage_tools,
        incomplete: true,
    })
}

/// Rebuild the pipelines the loader dropped, flagged incomplete
///
/// # Arguments
///
/// * `manifest` - The converted manifest to add the pipelines to
/// * `document` - The prepared legacy document the pipelines are read from
/// * `issues` - The issues so far, whose entries for these pipelines are no longer skips
fn keep_dropped_pipelines(
    manifest: &mut ToolboxManifestV2,
    document: &Value,
    issues: &mut [ConvertIssue],
) {
    let stages_code = MigrationReason::invalid_value("stages.*").code();
    // look at every pipeline the loader left out because of an unresolved stage
    for entry in issues.iter_mut() {
        if entry.code != stages_code || !entry.skipped {
            continue;
        }
        // the item names the pipeline and its version
        let Some((name, label)) = entry
            .item
            .strip_prefix("pipeline ")
            .and_then(|rest| rest.rsplit_once('@'))
        else {
            continue;
        };
        // find the legacy pipeline and rebuild it
        let Some(rebuilt) = document
            .get("pipelines")
            .and_then(|pipelines| pipelines.get(name))
            .and_then(|labels| labels.get(label))
            .and_then(|legacy| rebuild_incomplete(manifest, legacy))
        else {
            continue;
        };
        manifest
            .pipelines
            .entry(name.to_owned())
            .or_default()
            .versions
            .insert(label.to_owned(), rebuilt);
        // the pipeline is kept, so the issue is no longer a skip
        entry.skipped = false;
    }
}

/// List the tool each stage of the converted legacy pipelines runs
///
/// # Arguments
///
/// * `manifest` - The converted manifest
/// * `legacy` - The names of the pipelines that came from legacy entries
fn stage_mappings(manifest: &ToolboxManifestV2, legacy: &BTreeSet<String>) -> Vec<StageMapping> {
    // every stage of every legacy pipeline version, in name order
    manifest
        .pipelines
        .iter()
        .filter(|(name, _)| legacy.contains(*name))
        .flat_map(|(name, pipeline)| {
            pipeline.versions.values().flat_map(move |version| {
                version
                    .stage_tools
                    .iter()
                    .map(move |(stage, tool)| StageMapping {
                        pipeline: name.clone(),
                        stage: stage.clone(),
                        tool: tool.tool.clone(),
                        version: tool.version.clone(),
                    })
            })
        })
        .collect()
}

/// Make the converted pipelines follow their tools' latest versions
///
/// # Arguments
///
/// * `manifest` - The converted manifest
/// * `legacy` - The names of the pipelines that came from legacy entries
fn follow_latest(manifest: &mut ToolboxManifestV2, legacy: &BTreeSet<String>) {
    // a stage without a version follows its tool's latest
    for (name, pipeline) in &mut manifest.pipelines {
        if !legacy.contains(name) {
            continue;
        }
        for version in pipeline.versions.values_mut() {
            for stage in version.stage_tools.values_mut() {
                stage.version = None;
            }
        }
    }
}

/// Start the summary of a loaded manifest from what loading reported
///
/// # Arguments
///
/// * `loaded` - The loaded manifest
/// * `prepared` - What preparing the document changed
fn start_summary(loaded: &Loaded, prepared: &Prepared) -> ConvertSummary {
    let report = &loaded.report;
    // what loading skipped, noted, and warned about carries over
    let issues = report
        .skipped
        .iter()
        .map(|skipped| issue_of(skipped, None, true))
        .collect();
    // the notes include the ones made while preparing the document
    let mut notes = prepared.notes.clone();
    notes.extend(report.notes.iter().map(|note| ConvertNote {
        code: "note:description".to_owned(),
        item: format!("image {}", note.item),
        message: note.note.clone(),
    }));
    // loading's warnings are plain text
    let warnings = report
        .warnings
        .iter()
        .map(|warning| ConvertNote {
            code: "warning:conversion".to_owned(),
            item: loaded.manifest.name.clone(),
            message: warning.clone(),
        })
        .collect();
    // names that both sections of the input define, and images split across groups
    let mut collisions = report.collisions.clone();
    collisions.extend(prepared.collisions.iter().cloned());
    collisions.sort();
    collisions.dedup();
    // the counts are filled in once the conversion is done
    ConvertSummary {
        name: loaded.manifest.name.clone(),
        source: loaded.source,
        output: None,
        tools: 0,
        tool_versions: 0,
        builds: 0,
        pipelines: 0,
        network_policies: 0,
        mappings: Vec::new(),
        issues,
        warnings,
        notes,
        collisions,
    }
}

/// Convert the bytes of a toolbox manifest of either format
///
/// # Arguments
///
/// * `bytes` - The manifest's JSON
/// * `base_dir` - The directory the manifest was read from, if it was read from disk
/// * `options` - The choices that shape the conversion
/// * `bar` - The progress bar to track fetching remote configs through
pub async fn convert_document(
    bytes: &[u8],
    base_dir: Option<PathBuf>,
    options: &ConvertOptions,
    bar: &Bar,
) -> Result<Converted, Error> {
    // read the document generically so the legacy sections can be adjusted before loading
    let mut document: Value = serde_json::from_slice(bytes)
        .map_err(|err| Error::new(format!("Failed to parse toolbox manifest: {err}")))?;
    let prepared = prepare_document(&mut document, options);
    // load the prepared document from bytes, since loading reads bytes
    let prepared_bytes = serde_json::to_vec(&document)
        .map_err(|err| Error::new(format!("Failed to prepare toolbox manifest: {err}")))?;
    // load it, which projects the legacy part into tools, builds, and pipelines
    let mut loaded = parse_any(&prepared_bytes, base_dir, bar).await?;
    // start the report from what loading reported, and take the manifest out to convert it
    let mut summary = start_summary(&loaded, &prepared);
    let legacy_pipelines = prepared.legacy_pipelines;
    let mut manifest = std::mem::replace(&mut loaded.manifest, ToolboxManifestV2::new(""));
    // a legacy part is converted; a manifest that is already current is left as it is
    if loaded.source != SourceFormat::V2 {
        // build the specs, then check the pipelines against the tools that survived
        build_specs(&mut manifest, &mut summary);
        recheck_pipelines(
            &mut manifest,
            &legacy_pipelines,
            options,
            &mut summary.issues,
        );
        // pipelines the loader dropped come back when asked for
        if options.keep_incomplete_pipelines {
            keep_dropped_pipelines(&mut manifest, &document, &mut summary.issues);
        }
        // stages pin versions unless asked to follow the latest
        if options.follow_latest {
            follow_latest(&mut manifest, &legacy_pipelines);
        }
        // report the tool each stage runs
        summary.mappings = stage_mappings(&manifest, &legacy_pipelines);
    }
    // count what the converted manifest holds
    let counts = manifest.counts();
    summary.tools = counts.tools;
    summary.tool_versions = counts.tool_versions;
    summary.builds = counts.build_versions;
    summary.pipelines = counts.pipelines;
    summary.network_policies = counts.network_policies;
    Ok(Converted { manifest, summary })
}
