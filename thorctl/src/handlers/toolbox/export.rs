//! Export Thorium images and pipelines into a toolbox directory structure

use colored::Colorize;
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use thorium::models::{
    Image, ImageRequest, ImageVersion, NetworkPolicyRequest, Pipeline, PipelineRequest,
};
use thorium::{CtlConf, Error, Thorium};

use super::init::{generate_image_manifest, generate_pipeline_manifest, render_config_toml};
use super::manifest::{self, ToolboxManifest};
use super::{build, collisions, policies, shared};
use crate::args::Args;
use crate::args::toolbox::{BuildToolbox, ExportToolbox, ResourceSpec};
use crate::handlers::container;
use crate::handlers::exports::{DiskConflictResolver, WriteOutcome};
use crate::handlers::imports::editor::{resolve_editor, review_config_in_editor};
use crate::handlers::progress::{Bar, BarKind};
use crate::utils::images::list_all_images;
use crate::utils::pipelines::list_all_pipelines;

/// Render an image's version as a toolbox version label, defaulting to "latest"
///
/// # Arguments
///
/// * `version` - The image version to render, or `None` for the default label
fn version_label(version: &Option<ImageVersion>) -> String {
    match version {
        Some(ImageVersion::SemVer(v)) => v.to_string(),
        Some(ImageVersion::Custom(s)) => s.clone(),
        None => "latest".to_string(),
    }
}

// ─── Resource Resolution ─────────────────────────────────────────────────────

/// Resolves the images and pipelines an export run should write
///
/// Supports a full-group export (every image and pipeline in a group) and a targeted
/// export of named pipelines/images, deduplicating so a pipeline-referenced image and
/// a standalone `--images` selection never fetch the same image twice.
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to fetch resources
/// * `cmd` - The export args (group and/or named pipelines/images)
/// * `workers` - The number of concurrent fetches to run
async fn resolve_resources(
    thorium: &Thorium,
    cmd: &ExportToolbox,
    workers: usize,
) -> Result<(Vec<Image>, Vec<Pipeline>), Error> {
    // buffer_unordered with 0 never polls anything, so clamp to at least one worker
    let workers = workers.max(1);
    let mut images: Vec<Image> = Vec::new();
    let mut pipelines: Vec<Pipeline> = Vec::new();
    // tracks which image identities we've already queued so a pipeline-referenced
    // image and a standalone --image (or two pipelines) don't fetch it twice
    let mut seen_images: HashSet<(String, String)> = HashSet::new();
    // Full group export: list images and pipelines concurrently
    if let Some(group) = &cmd.group
        && cmd.pipelines.is_empty()
        && cmd.images.is_empty()
    {
        let (group_images, group_pipelines) = futures::try_join!(
            list_all_images(thorium, group),
            list_all_pipelines(thorium, group)
        )?;
        // seed the dedup set with every group image so a later --pipeline that references
        // one of them doesn't re-fetch it
        for img in group_images {
            seen_images.insert((img.group.clone(), img.name.clone()));
            images.push(img);
        }
        pipelines.extend(group_pipelines);
    }
    // Specific pipelines: fetch concurrently, then auto-resolve referenced images
    if !cmd.pipelines.is_empty() {
        // parse each "group/name" (falling back to --group) up front so a malformed spec
        // fails before any network calls are issued
        let specs = cmd
            .pipelines
            .iter()
            .map(|s| ResourceSpec::parse(s, cmd.group.as_deref()).map_err(Error::new))
            .collect::<Result<Vec<_>, _>>()?;
        // fetch the named pipelines bounded-parallel; each result keeps the error context
        // (group:name) so a failure points at the offending spec
        let fetched: Vec<Result<Pipeline, Error>> = stream::iter(specs)
            .map(|spec| async move {
                thorium
                    .pipelines
                    .get(&spec.group, &spec.name)
                    .await
                    .map_err(|e| {
                        Error::new(format!(
                            "Failed to get pipeline '{}:{}': {e}",
                            spec.group, spec.name
                        ))
                    })
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        // walk every fetched pipeline's order and record each referenced image identity
        // exactly once, scoping the lookup to the pipeline's own group
        let mut referenced: Vec<(String, String)> = Vec::new();
        for pipeline in fetched {
            // propagate the first fetch error here rather than during the concurrent stream
            // so partial successes are still surfaced as a single failure
            let pipeline = pipeline?;
            for image_name in pipeline.order.iter().flatten() {
                let key = (pipeline.group.clone(), image_name.clone());
                // insert returns false when the identity was already queued, skipping the
                // duplicate fetch
                if seen_images.insert(key.clone()) {
                    referenced.push(key);
                }
            }
            pipelines.push(pipeline);
        }
        // pull in every image a pipeline depends on so the exported toolbox is self-contained
        let fetched_images: Vec<Result<Image, Error>> = stream::iter(referenced)
            .map(|(group, name)| async move {
                thorium.images.get(&group, &name).await.map_err(|e| {
                    Error::new(format!(
                        "Failed to get image '{group}:{name}' (referenced by a pipeline): {e}"
                    ))
                })
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        // surface any referenced-image fetch failure as a hard error: a pipeline can't be
        // exported usefully without the images it runs
        for image in fetched_images {
            images.push(image?);
        }
    }
    // Specific standalone images: dedup against everything already queued, then fetch concurrently
    let standalone = cmd
        .images
        .iter()
        .map(|s| ResourceSpec::parse(s, cmd.group.as_deref()).map_err(Error::new))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        // keep only identities not already queued by a group export or a pipeline reference
        .filter_map(|spec| {
            let key = (spec.group, spec.name);
            seen_images.insert(key.clone()).then_some(key)
        })
        .collect::<Vec<_>>();
    let fetched_standalone: Vec<Result<Image, Error>> = stream::iter(standalone)
        .map(|(group, name)| async move {
            thorium
                .images
                .get(&group, &name)
                .await
                .map_err(|e| Error::new(format!("Failed to get image '{group}:{name}': {e}")))
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    // a standalone image the user explicitly named that can't be fetched is fatal
    for image in fetched_standalone {
        images.push(image?);
    }
    // nothing was selected by any of the three paths: fail loudly rather than write an empty toolbox
    if images.is_empty() && pipelines.is_empty() {
        return Err(Error::new(
            "No resources to export. Specify --group, --pipeline, or --image.",
        ));
    }
    Ok((images, pipelines))
}

// ─── Toolbox-wide settings ───────────────────────────────────────────────────

/// The toolbox-wide settings written into the exported `config.toml`
///
/// Sourced either from an existing `config.toml` (`--config`) or the
/// `--name`/`--registry` flags.
struct ToolboxSettings {
    /// Human-readable toolbox name
    name: String,
    /// Primary container registry, unset when the toolbox declares no central one
    registry: Option<String>,
    /// Extra registries to additionally tag images for
    registries: Vec<String>,
    /// Default registry base path bundled images push under on import
    image_path_prefix: Option<String>,
    /// Directory (relative to the output root) image tool dirs are written under; `None` = `images`
    export_image_path: Option<String>,
    /// Directory (relative to the output root) pipeline tool dirs are written under; `None` = `pipelines`
    export_pipeline_path: Option<String>,
    /// Whether the toolbox bundles image tarballs (driven by `--with-images`)
    bundled_images: bool,
    /// The toolbox-wide default base-image config, preserved from a reused `--config`
    base_image: Option<build::BaseImage>,
}

/// Collect the per-resource `=destpath` placements from the `--images`/`--pipelines` selections
///
/// Returns a map of resource name to its destination directory (relative to the toolbox root).
/// Keyed by name (the on-disk tool-directory leaf); a resource without an explicit `=dest` is
/// absent and falls back to the configured/default layout. Whole-group exports and auto-pulled
/// dependency images aren't named here, so they are never overridden. Specs that fail to parse are
/// skipped — `resolve_resources` re-parses and surfaces the error.
///
/// # Arguments
///
/// * `cmd` - The export command
fn collect_dest_overrides(cmd: &ExportToolbox) -> HashMap<String, String> {
    let mut overrides = HashMap::new();
    for spec in cmd.images.iter().chain(cmd.pipelines.iter()) {
        if let Ok(parsed) = ResourceSpec::parse(spec, cmd.group.as_deref())
            && let Some(dest) = parsed.dest
        {
            overrides.insert(parsed.name, dest);
        }
    }
    overrides
}

/// Index an existing `toolbox.json` at the output for append reconciliation
///
/// Returns `(images, pipelines)` maps keyed by `(group, name, version)` — images map to their
/// `(canonical-config JSON, dir)` and pipelines to their canonical-config JSON. Empty when there is
/// no existing `toolbox.json` (a fresh export) or it can't be read/parsed (in which case `build`'s
/// duplicate-manifest check still guards). Lets the write loops skip unchanged resources (config +
/// bundle) and catch a cross-directory duplicate identity up front.
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `progress` - The progress bar, for the "found existing toolbox" notice
#[allow(clippy::type_complexity)]
async fn load_existing_index(
    cmd: &ExportToolbox,
    progress: &Bar,
) -> (
    HashMap<(String, String, String), (String, String)>,
    HashMap<(String, String, String), String>,
) {
    let mut images = HashMap::new();
    let mut pipelines = HashMap::new();
    let path = cmd.output.join("toolbox.json");
    // no existing toolbox.json → fresh export, nothing to reconcile (zero added cost)
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return (images, pipelines);
    };
    // an unparsable existing toolbox.json is skipped; build's duplicate check still guards
    let Ok(existing) = serde_json::from_slice::<ToolboxManifest>(&bytes) else {
        return (images, pipelines);
    };
    progress.info_anonymous(format!(
        "Found existing toolbox at '{}'; reconciling against it",
        path.display()
    ));
    // index each embedded image config by identity, keeping its canonical JSON (for the unchanged
    // comparison) and recorded dir (for the cross-directory duplicate check)
    for image in existing.images.values() {
        for (version, entry) in &image.versions {
            if let Some(config) = &entry.config
                && let Ok(json) = crate::utils::canonical_json(config)
            {
                images.insert(
                    (config.group.clone(), config.name.clone(), version.clone()),
                    (json, entry.dir.clone()),
                );
            }
        }
    }
    // pipelines have no recorded dir, so they only get the unchanged comparison
    for pipeline in existing.pipelines.values() {
        for (version, entry) in &pipeline.versions {
            if let Some(config) = &entry.config
                && let Ok(json) = crate::utils::canonical_json(config)
            {
                pipelines.insert(
                    (config.group.clone(), config.name.clone(), version.clone()),
                    json,
                );
            }
        }
    }
    (images, pipelines)
}

/// The `config.toml` at the export output root, if one already exists
///
/// Its presence makes the export an append into an existing toolbox: the file becomes the settings
/// source and is preserved unless `--overwrite-config`.
///
/// # Arguments
///
/// * `cmd` - The export command
fn existing_config_path(cmd: &ExportToolbox) -> Option<PathBuf> {
    let path = cmd.output.join("config.toml");
    path.exists().then_some(path)
}

/// Build [`ToolboxSettings`] from a loaded `config.toml`, with bundling driven by the run
///
/// # Arguments
///
/// * `config` - The loaded toolbox config
/// * `with_images` - Whether this run bundles images (drives `bundled_images`, never the config's claim)
fn settings_from_config(config: build::ToolboxConfig, with_images: bool) -> ToolboxSettings {
    ToolboxSettings {
        name: config.name,
        registry: config.registry,
        registries: config.registries,
        image_path_prefix: config.image_path_prefix,
        export_image_path: config.export_image_path,
        export_pipeline_path: config.export_pipeline_path,
        // bundling reflects what this run actually exports, never the reused config's claim
        bundled_images: with_images,
        base_image: config.base_image,
    }
}

/// Resolve the toolbox-wide settings for an export
///
/// Priority: an explicit `--config` (seed from another toolbox); else an existing `config.toml` at
/// the output root (append — its settings are reused and the file is preserved unless
/// `--overwrite-config`); else the `--name`/`--registry` flags (a new toolbox). Bundling is always
/// driven by the `--with-images` export action, not inherited from a config's `bundled_images`.
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `existing_config` - The output's `config.toml` if it already exists (the append source)
/// * `progress` - The progress bar, for the "using existing config" notice and mismatch warnings
fn resolve_settings(
    cmd: &ExportToolbox,
    existing_config: Option<&Path>,
    progress: &Bar,
) -> Result<ToolboxSettings, Error> {
    // (1) an explicit --config seeds settings from another toolbox
    if let Some(config_path) = &cmd.config {
        return Ok(settings_from_config(
            build::load_config(config_path)?,
            cmd.with_images,
        ));
    }
    // (2) append: an existing config.toml at the output is the settings source
    if let Some(path) = existing_config {
        progress.info_anonymous(format!(
            "Using existing config.toml at '{}' for toolbox settings",
            path.display()
        ));
        let config = build::load_config(path)?;
        // warn when a run flag implies a setting the preserved config contradicts; the existing
        // config wins unless --overwrite-config, so the flag is otherwise silently ignored
        if !cmd.overwrite_config {
            if cmd.with_images && !config.bundled_images {
                progress.warning(
                    "--with-images is set but the existing config.toml has bundled_images = false; \
                     the existing setting is kept (pass --overwrite-config to update it)",
                );
            }
            if let Some(registry) = &cmd.registry
                && config.registry.as_deref() != Some(registry.as_str())
            {
                progress.warning(format!(
                    "--registry '{registry}' differs from the existing config.toml; the existing \
                     registry is kept (pass --overwrite-config to update it)"
                ));
            }
        }
        return Ok(settings_from_config(config, cmd.with_images));
    }
    // (3) new toolbox: derive from flags; registries/prefix/layout/base_image have no flag
    Ok(ToolboxSettings {
        name: cmd.name.clone(),
        registry: cmd.registry.clone(),
        registries: Vec::new(),
        image_path_prefix: None,
        export_image_path: None,
        export_pipeline_path: None,
        bundled_images: cmd.with_images,
        base_image: None,
    })
}

// ─── Manifest assembly ───────────────────────────────────────────────────────

/// Build an in-memory toolbox manifest from the resolved Thorium resources
///
/// Entries are keyed by `<group>/<name>` so that same-named resources from
/// different groups stay distinct until collision resolution runs (the manifest's
/// own maps are otherwise keyed by name). Each image carries the network policy
/// definitions it references so they can be written alongside it.
///
/// # Arguments
///
/// * `settings` - The resolved toolbox-wide settings (name/registry/bundling/etc.)
/// * `images` - The resolved Thorium images
/// * `pipelines` - The resolved Thorium pipelines
/// * `policies` - Fetched network policy definitions keyed by name
fn build_manifest(
    settings: &ToolboxSettings,
    images: &[Image],
    pipelines: &[Pipeline],
    policies: &HashMap<String, NetworkPolicyRequest>,
) -> ToolboxManifest {
    let mut image_entries: HashMap<String, manifest::ImageManifest> = HashMap::new();
    // map (group, name) -> exported version label so each pipeline's image map pins
    // the version we actually exported (rather than a name-keyed guess); built in the
    // same pass as the entries so the version label is only computed once per image
    let mut image_versions: HashMap<(String, String), String> = HashMap::new();
    for image in images {
        // the version label both keys this image's manifest entry and pins it in any pipeline
        // image map below, so compute it once here
        let version = version_label(&image.version);
        // the on-disk config is the image's Thorium request form
        let config = ImageRequest::from(image.clone());
        // bundle the definitions of the policies this image references, scoping each copy to the
        // group this toolbox exports the image in rather than the policy's full instance-wide
        // group set (so the exported toolbox doesn't reference groups it doesn't carry)
        let network_policies = config
            .network_policies
            .iter()
            .filter_map(|name| policies.get(name).cloned())
            .map(|mut policy| {
                policy.groups = vec![image.group.clone()];
                policy
            })
            .collect();
        // build_path is "./" because the manifest sits in the tool's own dir; config is embedded
        // inline (not config_from) and the bundled policies travel as definitions (not _from refs).
        // dir is empty here: this in-memory manifest only drives validation/collision/reconcile —
        // the authoritative per-image dir is computed by the auto-build that walks the written tree.
        let entry = manifest::ImageVersion {
            dir: String::new(),
            build_path: "./".to_string(),
            config_from: None,
            config: Some(config),
            network_policies_from: Vec::new(),
            network_policies,
        };
        // remember this image's exported version so pipelines can pin the exact version exported
        image_versions.insert((image.group.clone(), image.name.clone()), version.clone());
        // key the entry by <group>/<name> to keep same-named images from different groups
        // distinct until collision resolution collapses/renames them
        image_entries.insert(
            format!("{}/{}", image.group, image.name),
            manifest::ImageManifest {
                versions: HashMap::from([(version, entry)]),
            },
        );
    }
    let mut pipeline_entries: HashMap<String, manifest::PipelineManifest> = HashMap::new();
    for pipeline in pipelines {
        // flatten the (possibly staged) order into the unique image names this pipeline runs,
        // pinning each to the version we actually exported it under (falling back to "latest"
        // when the image wasn't part of this export, e.g. it lives outside the selection)
        let images_map = pipeline
            .order
            .iter()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|name| {
                let version = image_versions
                    .get(&(pipeline.group.clone(), name.clone()))
                    .cloned()
                    .unwrap_or_else(|| "latest".to_string());
                (name, manifest::PipelineImage { version })
            })
            .collect();
        // pipelines carry no version axis here, so every entry is keyed "latest"
        let entry = manifest::PipelineVersion {
            description: pipeline.description.clone().unwrap_or_default(),
            images: images_map,
            config_from: None,
            config: Some(PipelineRequest::from(pipeline.clone())),
        };
        // key by <group>/<name> for the same group-distinctness reason as images above
        pipeline_entries.insert(
            format!("{}/{}", pipeline.group, pipeline.name),
            manifest::PipelineManifest {
                versions: HashMap::from([("latest".to_string(), entry)]),
            },
        );
    }
    ToolboxManifest {
        name: settings.name.clone(),
        registry: settings.registry.clone(),
        images: image_entries,
        pipelines: pipeline_entries,
        bundled_images: settings.bundled_images,
        image_path_prefix: settings.image_path_prefix.clone(),
    }
}

// ─── File Writing ────────────────────────────────────────────────────────────

/// The directory an exported pipeline's files are written to: `<output>/<layout>/<name>`, where
/// `<layout>` is the toolbox's configured `export_pipeline_path` or the default `pipelines`
///
/// # Arguments
///
/// * `output` - The toolbox output root
/// * `settings` - The resolved toolbox settings carrying the configured layout
/// * `name` - The pipeline's name (its tool-directory leaf)
fn pipeline_dest_dir(output: &Path, settings: &ToolboxSettings, name: &str) -> PathBuf {
    output
        .join(
            settings
                .export_pipeline_path
                .as_deref()
                .unwrap_or("pipelines"),
        )
        .join(name)
}

/// Write a resolved image entry to the toolbox directory, resolving on-disk
/// conflicts; returns [`WriteOutcome::Quit`] if the user asked to stop
///
/// # Arguments
///
/// * `image_dir` - The tool directory to write this image's files into (resolved by the caller)
/// * `config` - The resolved image request (its `name` is the on-disk file stem)
/// * `version` - The toolbox version label to record
/// * `network_policies` - The policy definitions this image references
/// * `review` - Open the config in an editor for review before writing
/// * `editor` - The editor command used when `review` is set
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar
#[allow(clippy::too_many_arguments)]
async fn write_image_entry(
    image_dir: &Path,
    config: &ImageRequest,
    version: &str,
    network_policies: &[NetworkPolicyRequest],
    review: bool,
    editor: &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<WriteOutcome, Error> {
    // the config's own name is the json file stem; image_dir is the caller-resolved tool directory
    let name = &config.name;
    // canonical (sorted-key) JSON so reordered map fields don't churn the file
    let config_json = crate::utils::canonical_json(config)
        .map_err(|e| Error::new(format!("Failed to serialize image '{name}': {e}")))?;
    // let the user hand-edit the config first when --review is set; otherwise write it verbatim
    let final_json = if review {
        // suspend the spinner while the editor owns the terminal
        progress
            .suspend_async(review_config_in_editor::<ImageRequest>(
                &config_json,
                &format!("export-image-{name}"),
                editor,
                crate::handlers::imports::merge::IMAGE_FIELD_ORDER,
            ))
            .await?
    } else {
        config_json
    };
    // short-circuit the whole export if the resolver prompt returns Quit at any write
    if resolver
        .write_yaml::<ImageRequest>(
            &image_dir.join(format!("{name}.json")),
            &final_json,
            progress,
        )
        .await?
        == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    // write the definition of every network policy this image references so an
    // import can create them in instances that lack them
    let mut policy_files: Vec<String> = Vec::new();
    for policy in network_policies {
        // name each policy file <policy>.policy.json so the manifest can list it under
        // network_policies_from and the importer recognizes it by suffix
        let file_name = format!("{}.policy.json", policy.name);
        // policy files are pretty-printed (not canonical) to stay human-readable on disk
        let policy_json = serde_json::to_string_pretty(policy).map_err(|e| {
            Error::new(format!(
                "Failed to serialize network policy '{}': {e}",
                policy.name
            ))
        })?;
        if resolver
            .write_yaml::<NetworkPolicyRequest>(&image_dir.join(&file_name), &policy_json, progress)
            .await?
            == WriteOutcome::Quit
        {
            return Ok(WriteOutcome::Quit);
        }
        policy_files.push(file_name);
    }
    // sort so regenerated manifests don't churn on set iteration order
    policy_files.sort_unstable();
    // record the real registry url so a rebuild of this (build = false) export keeps the path the
    // image actually lives at instead of deriving one. image_name is set to the tool name (the
    // second arg): it's irrelevant while the image is pinned via exported_image_path, and only
    // matters if the user later flips build = true and opts into --use-image-path.
    let manifest = generate_image_manifest(
        name,
        name,
        version,
        true,
        &policy_files,
        config.image.as_deref(),
    );
    if resolver
        .write_toml::<build::ManifestToml>(&image_dir.join("manifest.toml"), &manifest, progress)
        .await?
        == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    // mirror a non-empty description into description.md so the toolbox repo carries the tool
    // docs as markdown (toolbox build reads it back and treats it as the source of truth); an
    // empty/absent description writes no file so build leaves the inline value untouched
    if let Some(description) = config
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
        && resolver
            .write_text(&image_dir.join("description.md"), description, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    Ok(WriteOutcome::Written)
}

/// Write a resolved pipeline entry to the toolbox directory, resolving on-disk
/// conflicts; returns [`WriteOutcome::Quit`] if the user asked to stop
///
/// # Arguments
///
/// * `pipeline_dir` - The tool directory to write this pipeline's files into (resolved by the caller)
/// * `config` - The resolved pipeline request (its `name` is the on-disk file stem)
/// * `description` - The pipeline description to mirror to description.md
/// * `image_versions` - The (image name, version) pairs for the manifest's image map
/// * `review` - Open the config in an editor for review before writing
/// * `editor` - The editor command used when `review` is set
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar
#[allow(clippy::too_many_arguments)]
async fn write_pipeline_entry(
    pipeline_dir: &Path,
    config: &PipelineRequest,
    description: &str,
    image_versions: &[(String, String)],
    review: bool,
    editor: &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<WriteOutcome, Error> {
    // the config's own name is the json file stem; pipeline_dir is the caller-resolved tool directory
    let name = &config.name;
    // canonical (sorted-key) JSON so reordered map fields don't churn the file
    let config_json = crate::utils::canonical_json(config)
        .map_err(|e| Error::new(format!("Failed to serialize pipeline '{name}': {e}")))?;
    // let the user hand-edit the config first when --review is set; otherwise write it verbatim
    let final_json = if review {
        // suspend the spinner while the editor owns the terminal
        progress
            .suspend_async(review_config_in_editor::<PipelineRequest>(
                &config_json,
                &format!("export-pipeline-{name}"),
                editor,
                crate::handlers::imports::merge::PIPELINE_FIELD_ORDER,
            ))
            .await?
    } else {
        config_json
    };
    // short-circuit the whole export if the resolver prompt returns Quit at any write
    if resolver
        .write_yaml::<PipelineRequest>(
            &pipeline_dir.join(format!("{name}.json")),
            &final_json,
            progress,
        )
        .await?
        == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    // generate the per-tool manifest pinning each image to its exported version
    let manifest = generate_pipeline_manifest(name, image_versions);
    if resolver
        .write_toml::<build::ManifestToml>(&pipeline_dir.join("manifest.toml"), &manifest, progress)
        .await?
        == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    // mirror a non-empty description to description.md (build treats it as the source of truth);
    // an empty description writes no file so the inline config value stands
    if !description.is_empty()
        && resolver
            .write_text(&pipeline_dir.join("description.md"), description, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    Ok(WriteOutcome::Written)
}

// ─── Main Export ─────────────────────────────────────────────────────────────

/// Exports the selected images and pipelines into an on-disk toolbox directory
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to fetch resources
/// * `cmd` - The export args (target directory and resource selection)
/// * `args` - The top-level thorctl args (worker count)
/// * `conf` - The Thorctl config (editor resolution)
pub async fn export(
    thorium: Thorium,
    cmd: &ExportToolbox,
    args: &Args,
    conf: &CtlConf,
) -> Result<(), Error> {
    // when --group is combined with named --pipelines/--images it's only the default group for
    // those names, not a full-group export; say so to avoid the "why didn't it export the whole
    // group?" surprise
    if cmd.group.is_some() && (!cmd.pipelines.is_empty() || !cmd.images.is_empty()) {
        println!(
            "Note: --group is used only as the default group for the named --pipelines/--images; \
             omit them to export the whole group"
        );
    }
    // resolve every image/pipeline to export (group export and/or named resources, deduped)
    let (images, pipelines) = resolve_resources(&thorium, cmd, args.workers).await?;
    // resolve the editor up front so --review uses a consistent command across all configs
    let editor = resolve_editor(None, conf);
    let progress = Bar::new("toolbox export", "Exporting", BarKind::Timer);
    println!(
        "Exporting {} images and {} pipelines to '{}'{}",
        images.len().to_string().bright_green(),
        pipelines.len().to_string().bright_green(),
        cmd.output.display().to_string().bright_cyan(),
        if cmd.with_images {
            " (bundling container images)".bright_yellow().to_string()
        } else {
            String::new()
        },
    );
    // gather the unique (group, policy name) pairs referenced across all images, keeping
    // one referencing image per pair for error context
    let mut wanted: Vec<(String, String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for image in &images {
        for policy_name in &image.network_policies {
            // first image to reference a given (group, policy) wins as the error-context name
            if seen.insert((image.group.clone(), policy_name.clone())) {
                wanted.push((image.group.clone(), policy_name.clone(), image.name.clone()));
            }
        }
    }
    // fetch existing policies scoped to just the exported images' groups so a name that
    // is ambiguous across the whole instance still resolves uniquely within its group
    // (a global get-by-name 400s on cross-group ambiguity)
    let groups: Vec<String> = images
        .iter()
        .map(|image| image.group.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let index = policies::fetch_existing_in_groups(&thorium, &groups).await?;
    let mut policies: HashMap<String, NetworkPolicyRequest> = HashMap::new();
    // collect references that resolved to no definition; an export that omits a policy it
    // references produces a structurally-incomplete toolbox, so these drive a non-zero exit
    // (and an aggregated end-of-run summary) rather than being lost as mid-stream warnings
    let mut dangling_policies: Vec<String> = Vec::new();
    for (group, policy_name, image_name) in wanted {
        // look each referenced policy up by its (group, name) identity within the fetched index
        match index.get(&(group.clone(), policy_name.clone())) {
            // found: stash its request form keyed by name for build_manifest to attach
            Some(policy) => {
                policies.insert(policy_name, NetworkPolicyRequest::from(policy));
            }
            // a dangling reference in the source instance isn't fatal to the export, but the
            // toolbox will be missing that definition, so record it and warn
            None => {
                progress.warning(format!(
                    "Network policy '{policy_name}' (referenced by image '{image_name}' in group \
                     '{group}') was not found; the exported toolbox won't include its definition, so \
                     an import will rely on the target instance already having it",
                ));
                dangling_policies.push(format!("{policy_name} (group '{group}')"));
            }
        }
    }
    // detect an existing config.toml at the output root: it makes this an append into an existing
    // toolbox (its settings are the source, and it is preserved unless --overwrite-config)
    let existing_config = existing_config_path(cmd);
    // resolve the toolbox-wide settings — explicit --config seed, else an existing config.toml at
    // the output (append), else the --name/--registry flags. Bundling is driven by --with-images.
    let settings = resolve_settings(cmd, existing_config.as_deref(), &progress)?;
    // prompts are only possible interactively (not --skip-conflicts) AND on a real terminal;
    // this gates both collision resolution and the on-disk conflict resolver below
    let can_prompt = !cmd.skip_conflicts && IsTerminal::is_terminal(&std::io::stdin());
    // --review opens an editor per config, which needs a real terminal; ignore it (with a
    // warning) when there's no TTY so a headless run doesn't hang waiting on an editor
    let review = if cmd.review && !IsTerminal::is_terminal(&std::io::stdin()) {
        progress.warning("--review needs a terminal; skipping the editor review for each config");
        false
    } else {
        cmd.review
    };
    // build an in-memory manifest and run it through the SAME validation and
    // collision-resolution flow as `toolbox import`, so duplicates/collisions are
    // resolved identically (de-dupe, rename + cascade, or skip) before anything
    // touches disk
    let mut manifest = build_manifest(&settings, &images, &pipelines, &policies);
    // snapshot each resource's original group BEFORE any override so collision resolution can
    // tell which members truly collided versus were collapsed into one group by --group-override
    let sources = manifest.capture_source_groups();
    if let Some(group) = &cmd.group_override {
        progress.info_anonymous(format!(
            "Overriding all image/pipeline export groups to '{}'",
            group.bright_yellow()
        ));
        manifest = manifest.override_group(group);
        // override_group only rewrites image/pipeline config groups; keep each bundled policy's
        // groups consistent with the overridden image groups on disk
        for image in manifest.images.values_mut() {
            for version in image.versions.values_mut() {
                for policy in &mut version.network_policies {
                    policy.groups = vec![group.clone()];
                }
            }
        }
    }
    // drop image versions with no config and pipelines that are structurally broken (warning each)
    shared::warn_dropped(&manifest.validate_structural(), &progress);
    // drop pipelines whose order references images not present in their group (warning each)
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // collapse byte-identical duplicates and rename/skip true (group, name) collisions
    collisions::resolve_collisions(&mut manifest, &sources, can_prompt, &progress)?;
    // re-check coherence: collision renames/repointing can re-break a pipeline's group view
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // map each explicitly-named resource (by tool name) to its optional `=destpath`, so the write
    // loops can place a single resource at a chosen directory instead of the configured layout.
    // Keyed by name because that is the on-disk tool directory leaf and names are unique after
    // collision resolution; whole-group/auto-pulled resources aren't named here so they use the
    // configured/default layout.
    let dest_overrides = collect_dest_overrides(cmd);
    // index an existing toolbox at the output (append reconciliation): lets the write loops skip
    // unchanged resources and their bundle work, and refuse to write a second copy of an identity
    // that already lives at a different directory (a build-duplicate). Empty for a fresh export.
    let (existing_images, existing_pipelines) = load_existing_index(cmd, &progress).await;
    // write the resolved manifest to disk, resolving on-disk conflicts
    let mut resolver = DiskConflictResolver::new(cmd.overwrite, can_prompt, editor.to_string());
    let mut stopped = false;
    // (image name, container url, tool dir) tarballs to bundle after the config pass; the
    // config writes stay sequential (the resolver prompts), but the heavy container
    // pull/save is run bounded-parallel below. The tool dir is carried so the tarball lands
    // beside the image's manifest (the configured layout), matching where import looks for it.
    let mut bundle_jobs: Vec<(String, Option<String>, PathBuf)> = Vec::new();
    'images: for image_manifest in manifest.images.values() {
        for (version, entry) in &image_manifest.versions {
            // an export always embeds a config; skip defensively so a configless entry
            // (shouldn't occur here) doesn't panic on unwrap
            let Some(config) = &entry.config else {
                continue;
            };
            // resolve the tool directory (relative to the output root): an explicit per-resource
            // `=destpath` wins, else the toolbox's configured image layout (default `images/<name>`)
            let target_rel = match dest_overrides.get(config.name.as_str()) {
                Some(dest) => dest.clone(),
                None => format!(
                    "{}/{}",
                    settings.export_image_path.as_deref().unwrap_or("images"),
                    config.name
                ),
            };
            let image_dir = cmd.output.join(&target_rel);
            // append reconciliation against an existing toolbox: skip an unchanged image (its config
            // and its bundle work), and refuse to write a second copy of an identity that already
            // lives at a different directory (which would fail `build`)
            let key = (config.group.clone(), config.name.clone(), version.clone());
            if let Some((existing_json, existing_dir)) = existing_images.get(&key) {
                let current_json = crate::utils::canonical_json(config)?;
                if &current_json == existing_json {
                    progress.info_anonymous(format!(
                        "Unchanged: image '{}:{version}' already in the toolbox; skipping",
                        config.name
                    ));
                    continue;
                }
                if !existing_dir.is_empty() && existing_dir != &target_rel {
                    progress.warning(format!(
                        "image '{}:{version}' already exists in the toolbox at '{existing_dir}' with \
                         a different config; not writing a second copy at '{target_rel}' (it would \
                         fail build) — update the existing copy or export to its directory",
                        config.name
                    ));
                    continue;
                }
            }
            let outcome = write_image_entry(
                &image_dir,
                config,
                version,
                &entry.network_policies,
                review,
                editor,
                &mut resolver,
                &progress,
            )
            .await?;
            // a Quit at any prompt stops the whole export; flag it and bail out of both loops
            if outcome == WriteOutcome::Quit {
                stopped = true;
                break 'images;
            }
            // defer the heavy container pull/save to a bounded-parallel pass after all the
            // sequential (prompt-driven) config writes complete
            if cmd.with_images {
                bundle_jobs.push((config.name.clone(), config.image.clone(), image_dir));
            }
        }
    }
    // skip the pipeline pass entirely if the image pass was quit
    if !stopped {
        'pipelines: for pipeline_manifest in manifest.pipelines.values() {
            for (version, entry) in &pipeline_manifest.versions {
                // pipelines, like images, always carry a config in an export; skip defensively
                let Some(config) = &entry.config else {
                    continue;
                };
                // append reconciliation: skip a pipeline whose config is unchanged from the existing
                // toolbox (pipelines have no bundle/dir, so only the unchanged comparison applies)
                let key = (config.group.clone(), config.name.clone(), version.clone());
                if let Some(existing_json) = existing_pipelines.get(&key) {
                    let current_json = crate::utils::canonical_json(config)?;
                    if &current_json == existing_json {
                        progress.info_anonymous(format!(
                            "Unchanged: pipeline '{}:{version}' already in the toolbox; skipping",
                            config.name
                        ));
                        continue;
                    }
                }
                // the resolved image map carries the (possibly renamed) names paired
                // with the versions we exported them under
                let mut image_versions: Vec<(String, String)> = entry
                    .images
                    .iter()
                    .map(|(name, image)| (name.clone(), image.version.clone()))
                    .collect();
                image_versions.sort();
                // resolve the tool directory: an explicit per-resource `=destpath` wins, else the
                // toolbox's configured pipeline layout (default `pipelines/<name>`)
                let pipeline_dir = match dest_overrides.get(config.name.as_str()) {
                    Some(dest) => cmd.output.join(dest),
                    None => pipeline_dest_dir(&cmd.output, &settings, &config.name),
                };
                let outcome = write_pipeline_entry(
                    &pipeline_dir,
                    config,
                    &entry.description,
                    &image_versions,
                    review,
                    editor,
                    &mut resolver,
                    &progress,
                )
                .await?;
                // a Quit here stops the export before the bundling and config.toml passes
                if outcome == WriteOutcome::Quit {
                    stopped = true;
                    break 'pipelines;
                }
            }
        }
    }
    // a user quit leaves a partial repo with no config.toml/toolbox.json; report and exit cleanly
    if stopped {
        progress.refresh("Export stopped early", BarKind::Timer);
        progress.finish();
        return Ok(());
    }
    // bundle the queued container images in parallel (container pull/save here capture
    // their output rather than streaming it, so concurrency is safe). Bounded by --workers.
    // Bundling is best-effort: a failed pull/save warns and is collected so one bad image
    // doesn't abort the export (save cleans up its own partial archive on failure), but the
    // collected failures drive a non-zero exit at the end so a scripted export can't mistake
    // an incomplete bundle for a complete one.
    let mut bundle_failures: Vec<String> = Vec::new();
    if !bundle_jobs.is_empty() {
        // never spawn more workers than jobs, and never zero (buffer_unordered(0) stalls)
        let workers = std::cmp::min(args.workers, bundle_jobs.len()).max(1);
        progress.refresh("Bundling images", BarKind::Bound(bundle_jobs.len() as u64));
        // pull+save each container concurrently, pairing every result with its image name so a
        // failure can be reported even though the stream completes out of order
        let results: Vec<(String, Result<(), Error>)> = stream::iter(bundle_jobs)
            .map(|(name, url, dir)| {
                let progress = &progress;
                async move {
                    // save the tarball into the same tool directory the manifest was written to,
                    // so import (which reads the recorded per-image dir) finds it
                    let outcome = bundle_image(&dir, &name, url.as_deref(), progress).await;
                    progress.inc(1);
                    (name, outcome)
                }
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        // warn (don't abort) on each failed bundle so the rest of the export still completes
        for (name, outcome) in results {
            if let Err(err) = outcome {
                progress.warning(format!(
                    "Could not bundle image '{name}': {err}; it will be missing from the toolbox, \
                     so importing '{name}' will fail to find its tarball"
                ));
                bundle_failures.push(name);
            }
        }
        // one summary warning so a multi-image failure is visible at a glance, not just per-image
        if !bundle_failures.is_empty() {
            progress.warning(format!(
                "{} image(s) could not be bundled ({}); the toolbox is marked bundled but is \
                 incomplete — re-export or push those images manually",
                bundle_failures.len(),
                bundle_failures.join(", ")
            ));
        }
    }
    // config.toml is the toolbox's sticky identity: an existing one is preserved (its settings were
    // the source above) unless --overwrite-config, so an append never clobbers the toolbox's
    // settings. A fresh export creates it (announced). The real image urls live in each image config
    // (see exported_image_path), so the registry here only matters for tools later marked buildable.
    let config_path = cmd.output.join("config.toml");
    if existing_config.is_some() && !cmd.overwrite_config {
        progress.info_anonymous(format!(
            "Keeping existing config.toml at '{}' (pass --overwrite-config to replace it)",
            config_path.display()
        ));
    } else {
        if existing_config.is_none() {
            progress.info_anonymous(format!(
                "No config.toml at '{}'; creating one",
                config_path.display()
            ));
        }
        let config_toml = render_config_toml(
            &settings.name,
            settings.registry.as_deref(),
            &settings.registries,
            settings.image_path_prefix.as_deref(),
            settings.export_image_path.as_deref(),
            settings.export_pipeline_path.as_deref(),
            settings.bundled_images,
            settings.base_image.as_ref(),
        );
        // written directly (not via the per-file resolver) because the sticky-config rule, not the
        // resolver's overwrite/skip behavior, governs config.toml
        tokio::fs::write(&config_path, config_toml)
            .await
            .map_err(|e| Error::new(format!("Failed to write '{}': {e}", config_path.display())))?;
    }
    // Auto-build toolbox.json, preserving the real image urls captured from Thorium.
    // build walks the tree with synchronous std::fs, so run it off the async runtime.
    let build_cmd = BuildToolbox {
        config: cmd.output.join("config.toml"),
        // leaf comes from the tool name, not image_name: an export pins urls via
        // exported_image_path, so the repo-path leaf is irrelevant here
        use_image_path: false,
        output: Some(cmd.output.join("toolbox.json")),
        path: Some(cmd.output.clone()),
        // an export records each image's real published url, so no tag suffix is applied
        tag_suffix: None,
    };
    // run the synchronous filesystem walk on a blocking thread; the outer ? unwraps the join
    // result and the inner ? the build result
    tokio::task::spawn_blocking(move || build::build(&build_cmd))
        .await
        .map_err(|err| Error::new(format!("Toolbox build task failed: {err}")))??;
    progress.finish();
    // a missing bundled tarball or an omitted (dangling) policy means the written toolbox is
    // not fully self-contained; report that plainly and exit non-zero so a scripted
    // export -> import handoff doesn't treat an incomplete toolbox as a success
    if bundle_failures.is_empty() && dangling_policies.is_empty() {
        println!(
            "\n{} Toolbox exported to '{}'. Import it with: thorctl toolbox import {}",
            "Done!".bright_green(),
            cmd.output.display(),
            cmd.output.join("toolbox.json").display()
        );
        return Ok(());
    }
    // build a human summary of what made the toolbox incomplete
    let mut problems: Vec<String> = Vec::new();
    if !bundle_failures.is_empty() {
        problems.push(format!(
            "{} image tarball(s) missing ({})",
            bundle_failures.len(),
            bundle_failures.join(", ")
        ));
    }
    if !dangling_policies.is_empty() {
        problems.push(format!(
            "{} referenced network polic(y/ies) not found and omitted ({})",
            dangling_policies.len(),
            dangling_policies.join(", ")
        ));
    }
    println!(
        "\n{} Toolbox written to '{}', but it is INCOMPLETE: {}. Resolve these and re-export \
         before importing.",
        "Warning:".bright_yellow(),
        cmd.output.display(),
        problems.join("; ")
    );
    Err(Error::new(format!(
        "export incomplete: {}",
        problems.join("; ")
    )))
}

/// Download and save an image's container image file into the toolbox bundle
///
/// Writes `<dir>/<name>.tar.gz` (the image's tool directory, beside its manifest). Images
/// without a container url are skipped.
///
/// # Arguments
///
/// * `dir` - The image's tool directory (where its manifest was written)
/// * `name` - The exported image name (the tarball's file stem)
/// * `url` - The image's container url, if any
/// * `progress` - The progress bar to route the skip warning through
async fn bundle_image(
    dir: &Path,
    name: &str,
    url: Option<&str>,
    progress: &Bar,
) -> Result<(), Error> {
    let Some(url) = url else {
        // route through the bar so the warning respects --quiet like every other one; the
        // toolbox is still marked bundled, so importing this image will fail to find its tarball
        progress.warning(format!(
            "Image '{}' has no container image url; skipping its tarball — importing it from this \
             bundled toolbox will fail to find '{}'",
            name.bright_cyan(),
            dir.join(format!("{name}.tar.gz")).display(),
        ));
        return Ok(());
    };
    // dedicated sub-bar so concurrent bundles each show their own pull/save progress
    let bar = Bar::new(name, "Bundling image", BarKind::Timer);
    // pull the container locally first so save has a local image to export
    container::pull(url, &bar).await?;
    // save the tarball into the image's tool directory (where its manifest was written); import
    // resolves this exact location from the per-image `dir` recorded in toolbox.json
    let tar = dir.join(format!("{name}.tar.gz"));
    container::save(url, &tar, &bar).await?;
    bar.finish_and_clear();
    Ok(())
}
