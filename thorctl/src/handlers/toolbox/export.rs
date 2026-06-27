//! Export Thorium images and pipelines into a toolbox directory structure

use colored::Colorize;
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::Path;
use thorium::models::{
    Image, ImageRequest, ImageVersion, NetworkPolicyRequest, Pipeline, PipelineRequest,
};
use thorium::{CtlConf, Error, Thorium};

use super::init::{generate_image_manifest, generate_pipeline_manifest, render_config_toml};
use crate::handlers::imports::editor::{resolve_editor, review_config_in_editor};
use super::manifest::{self, ToolboxManifest};
use super::{build, collisions, policies, shared};
use crate::args::Args;
use crate::args::toolbox::{BuildToolbox, ExportToolbox, ResourceSpec};
use crate::handlers::container;
use crate::handlers::exports::{DiskConflictResolver, WriteOutcome};
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
        for img in group_images {
            seen_images.insert((img.group.clone(), img.name.clone()));
            images.push(img);
        }
        pipelines.extend(group_pipelines);
    }

    // Specific pipelines: fetch concurrently, then auto-resolve referenced images
    if !cmd.pipelines.is_empty() {
        let specs = cmd
            .pipelines
            .iter()
            .map(|s| ResourceSpec::parse(s, cmd.group.as_deref()).map_err(Error::new))
            .collect::<Result<Vec<_>, _>>()?;
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
        // collect the (deduped) image identities each pipeline references
        let mut referenced: Vec<(String, String)> = Vec::new();
        for pipeline in fetched {
            let pipeline = pipeline?;
            for image_name in pipeline.order.iter().flatten() {
                let key = (pipeline.group.clone(), image_name.clone());
                if seen_images.insert(key.clone()) {
                    referenced.push(key);
                }
            }
            pipelines.push(pipeline);
        }
        // fetch the referenced images concurrently
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
        for image in fetched_images {
            images.push(image?);
        }
    }

    // Specific standalone images: dedup, then fetch concurrently
    let standalone = cmd
        .images
        .iter()
        .map(|s| ResourceSpec::parse(s, cmd.group.as_deref()).map_err(Error::new))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
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
    for image in fetched_standalone {
        images.push(image?);
    }

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
    /// Whether the toolbox bundles image tarballs (driven by `--with-images`)
    bundled_images: bool,
    /// The toolbox-wide default base-image config, preserved from a reused `--config`
    base_image: Option<build::BaseImage>,
}

/// Resolve the toolbox-wide settings for an export
///
/// `--config` reuses an existing toolbox's `config.toml` (name, registry,
/// registries, image_path_prefix); otherwise the `--name`/`--registry` flags are
/// used. Bundling is always driven by the `--with-images` export action, not
/// inherited from a config's `bundled_images`, so the written `config.toml` never
/// claims tarballs that weren't actually exported.
///
/// # Arguments
///
/// * `cmd` - The export command
fn resolve_settings(cmd: &ExportToolbox) -> Result<ToolboxSettings, Error> {
    if let Some(config_path) = &cmd.config {
        let config = build::load_config(config_path)?;
        Ok(ToolboxSettings {
            name: config.name,
            registry: config.registry,
            registries: config.registries,
            image_path_prefix: config.image_path_prefix,
            bundled_images: cmd.with_images,
            base_image: config.base_image,
        })
    } else {
        Ok(ToolboxSettings {
            name: cmd.name.clone(),
            registry: cmd.registry.clone(),
            registries: Vec::new(),
            image_path_prefix: None,
            bundled_images: cmd.with_images,
            base_image: None,
        })
    }
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
        let version = version_label(&image.version);
        let config = ImageRequest::from(image.clone());
        // bundle the definitions of the policies this image references
        let network_policies = config
            .network_policies
            .iter()
            .filter_map(|name| policies.get(name).cloned())
            .collect();
        let entry = manifest::ImageVersion {
            build_path: "./".to_string(),
            config_from: None,
            config: Some(config),
            network_policies_from: Vec::new(),
            network_policies,
        };
        image_versions.insert((image.group.clone(), image.name.clone()), version.clone());
        image_entries.insert(
            format!("{}/{}", image.group, image.name),
            manifest::ImageManifest {
                versions: HashMap::from([(version, entry)]),
            },
        );
    }
    let mut pipeline_entries: HashMap<String, manifest::PipelineManifest> = HashMap::new();
    for pipeline in pipelines {
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
        let entry = manifest::PipelineVersion {
            description: pipeline.description.clone().unwrap_or_default(),
            images: images_map,
            config_from: None,
            config: Some(PipelineRequest::from(pipeline.clone())),
        };
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

/// Write a resolved image entry to the toolbox directory, resolving on-disk
/// conflicts; returns [`WriteOutcome::Quit`] if the user asked to stop
///
/// # Arguments
///
/// * `output` - The toolbox output directory
/// * `config` - The resolved image request (its `name` is the on-disk name)
/// * `version` - The toolbox version label to record
/// * `network_policies` - The policy definitions this image references
/// * `review` - Open the config in an editor for review before writing
/// * `editor` - The editor command used when `review` is set
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar
async fn write_image_entry(
    output: &Path,
    config: &ImageRequest,
    version: &str,
    network_policies: &[NetworkPolicyRequest],
    review: bool,
    editor: &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<WriteOutcome, Error> {
    let name = &config.name;
    let image_dir = output.join("images").join(name);
    // canonical (sorted-key) JSON so reordered map fields don't churn the file
    let config_json = crate::utils::canonical_json(config)
        .map_err(|e| Error::new(format!("Failed to serialize image '{name}': {e}")))?;
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
    if resolver
        .write_yaml::<ImageRequest>(&image_dir.join(format!("{name}.json")), &final_json, progress)
        .await?
        == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }

    // write the definition of every network policy this image references so an
    // import can create them in instances that lack them
    let mut policy_files: Vec<String> = Vec::new();
    for policy in network_policies {
        let file_name = format!("{}.policy.json", policy.name);
        let policy_json = serde_json::to_string_pretty(policy).map_err(|e| {
            Error::new(format!("Failed to serialize network policy '{}': {e}", policy.name))
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
    // record the real registry url so a rebuild of this (build = false) export keeps
    // the path the image actually lives at instead of deriving one from image_name
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

    // mirror the description into description.md so the toolbox repo carries the
    // tool docs as markdown (toolbox build reads it back on the way in)
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
/// * `output` - The toolbox output directory
/// * `config` - The resolved pipeline request (its `name` is the on-disk name)
/// * `description` - The pipeline description to mirror to description.md
/// * `image_versions` - The (image name, version) pairs for the manifest's image map
/// * `review` - Open the config in an editor for review before writing
/// * `editor` - The editor command used when `review` is set
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar
async fn write_pipeline_entry(
    output: &Path,
    config: &PipelineRequest,
    description: &str,
    image_versions: &[(String, String)],
    review: bool,
    editor: &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<WriteOutcome, Error> {
    let name = &config.name;
    let pipeline_dir = output.join("pipelines").join(name);
    // canonical (sorted-key) JSON so reordered map fields don't churn the file
    let config_json = crate::utils::canonical_json(config)
        .map_err(|e| Error::new(format!("Failed to serialize pipeline '{name}': {e}")))?;
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

    let manifest = generate_pipeline_manifest(name, image_versions);
    if resolver
        .write_toml::<build::ManifestToml>(&pipeline_dir.join("manifest.toml"), &manifest, progress)
        .await?
        == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }

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
    let (images, pipelines) = resolve_resources(&thorium, cmd, args.workers).await?;
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
    for (group, policy_name, image_name) in wanted {
        match index.get(&(group.clone(), policy_name.clone())) {
            Some(policy) => {
                policies.insert(policy_name, NetworkPolicyRequest::from(policy));
            }
            // a dangling reference in the source instance isn't fatal to the
            // export, but the toolbox will be missing that definition
            None => progress.warning(format!(
                "Network policy '{policy_name}' referenced by image '{image_name}' not found in group '{group}'",
            )),
        }
    }

    // resolve the toolbox-wide settings: from --config (reuse an existing toolbox's
    // config.toml) or the --name/--registry flags. Bundling is driven by --with-images
    // (the export action), not inherited from a config's bundled_images.
    let settings = resolve_settings(cmd)?;

    // build an in-memory manifest and run it through the SAME validation and
    // collision-resolution flow as `toolbox import`, so duplicates/collisions are
    // resolved identically (de-dupe, rename + cascade, or skip) before anything
    // touches disk
    let can_prompt = !cmd.skip_conflicts && IsTerminal::is_terminal(&std::io::stdin());
    let mut manifest = build_manifest(&settings, &images, &pipelines, &policies);
    let sources = manifest.capture_source_groups();
    if let Some(group) = &cmd.group_override {
        progress.info_anonymous(format!(
            "Overriding all image/pipeline export groups to '{}'",
            group.bright_yellow()
        ));
        manifest = manifest.override_group(group);
    }
    shared::warn_dropped(&manifest.validate_structural(), &progress);
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    collisions::resolve_collisions(&mut manifest, &sources, can_prompt, &progress)?;
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);

    // write the resolved manifest to disk, resolving on-disk conflicts
    let mut resolver = DiskConflictResolver::new(cmd.overwrite, can_prompt, editor.to_string());
    let mut stopped = false;
    // (image name, container url) tarballs to bundle after the config pass; the
    // config writes stay sequential (the resolver prompts), but the heavy container
    // pull/save is run bounded-parallel below
    let mut bundle_jobs: Vec<(String, Option<String>)> = Vec::new();
    'images: for image_manifest in manifest.images.values() {
        for (version, entry) in &image_manifest.versions {
            let Some(config) = &entry.config else {
                continue;
            };
            let outcome = write_image_entry(
                &cmd.output,
                config,
                version,
                &entry.network_policies,
                cmd.review,
                editor,
                &mut resolver,
                &progress,
            )
            .await?;
            if outcome == WriteOutcome::Quit {
                stopped = true;
                break 'images;
            }
            // queue the container image file itself for offline bundling
            if cmd.with_images {
                bundle_jobs.push((config.name.clone(), config.image.clone()));
            }
        }
    }
    if !stopped {
        'pipelines: for pipeline_manifest in manifest.pipelines.values() {
            for entry in pipeline_manifest.versions.values() {
                let Some(config) = &entry.config else {
                    continue;
                };
                // the resolved image map carries the (possibly renamed) names paired
                // with the versions we exported them under
                let mut image_versions: Vec<(String, String)> = entry
                    .images
                    .iter()
                    .map(|(name, image)| (name.clone(), image.version.clone()))
                    .collect();
                image_versions.sort();
                let outcome = write_pipeline_entry(
                    &cmd.output,
                    config,
                    &entry.description,
                    &image_versions,
                    cmd.review,
                    editor,
                    &mut resolver,
                    &progress,
                )
                .await?;
                if outcome == WriteOutcome::Quit {
                    stopped = true;
                    break 'pipelines;
                }
            }
        }
    }
    if stopped {
        progress.refresh("Export stopped early", BarKind::Timer);
        progress.finish();
        return Ok(());
    }

    // bundle the queued container images in parallel (container pull/save here capture
    // their output rather than streaming it, so concurrency is safe). Bounded by
    // --workers; every job runs to completion before the first error is returned so
    // a cancelled job can't leave a partial archive behind.
    if !bundle_jobs.is_empty() {
        let workers = std::cmp::min(args.workers, bundle_jobs.len()).max(1);
        progress.refresh("Bundling images", BarKind::Bound(bundle_jobs.len() as u64));
        let results: Vec<Result<(), Error>> = stream::iter(bundle_jobs)
            .map(|(name, url)| {
                let progress = &progress;
                async move {
                    let outcome = bundle_image(&cmd.output, &name, url.as_deref(), progress).await;
                    progress.inc(1);
                    outcome
                }
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        results.into_iter().collect::<Result<(), Error>>()?;
    }

    // Write config.toml from the resolved settings. The real image urls are preserved
    // in each image config (see exported_image_path), so the registry here only matters
    // for tools a user later marks buildable; bundled toolboxes record that fact so
    // import knows to push the bundled images to a registry.
    let config_toml = render_config_toml(
        &settings.name,
        settings.registry.as_deref(),
        &settings.registries,
        settings.image_path_prefix.as_deref(),
        settings.bundled_images,
        settings.base_image.as_ref(),
    );
    let config_outcome = resolver
        .write_toml::<build::ToolboxConfig>(&cmd.output.join("config.toml"), &config_toml, &progress)
        .await?;
    // honor a quit at this final prompt the same way the per-resource writes above
    // do, so toolbox.json isn't built against a config the user declined to write
    if config_outcome == WriteOutcome::Quit {
        progress.refresh("Export stopped early", BarKind::Timer);
        progress.finish();
        return Ok(());
    }
    // Auto-build toolbox.json, preserving the real image urls captured from Thorium.
    // build walks the tree with synchronous std::fs, so run it off the async runtime.
    let build_cmd = BuildToolbox {
        config: cmd.output.join("config.toml"),
        use_image_path: false,
        output: cmd.output.join("toolbox.json"),
        path: cmd.output.clone(),
        // an export records each image's real published url, so no tag suffix is applied
        tag_suffix: None,
    };
    tokio::task::spawn_blocking(move || build::build(&build_cmd))
        .await
        .map_err(|err| Error::new(format!("Toolbox build task failed: {err}")))??;

    progress.finish();
    println!(
        "\n{} Toolbox exported to '{}'",
        "Done!".bright_green(),
        cmd.output.display()
    );
    Ok(())
}

/// Download and save an image's container image file into the toolbox bundle
///
/// Writes `<output>/images/<name>/<name>.tar.gz`. Images without a container url are skipped.
///
/// # Arguments
///
/// * `output` - The toolbox output directory
/// * `name` - The exported image name (its on-disk directory)
/// * `url` - The image's container url, if any
/// * `progress` - The progress bar to route the skip warning through
async fn bundle_image(
    output: &Path,
    name: &str,
    url: Option<&str>,
    progress: &Bar,
) -> Result<(), Error> {
    let Some(url) = url else {
        // route through the bar so the warning respects --quiet like every other one
        progress.warning(format!(
            "Image '{}' has no container image url; skipping bundle",
            name.bright_cyan(),
        ));
        return Ok(());
    };
    let bar = Bar::new(name, "Bundling image", BarKind::Timer);
    container::pull(url, &bar).await?;
    let tar = output
        .join("images")
        .join(name)
        .join(format!("{name}.tar.gz"));
    container::save(url, &tar, &bar).await?;
    bar.finish_and_clear();
    Ok(())
}
