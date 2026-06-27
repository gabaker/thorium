//! Diff an on-disk toolbox against what a Thorium instance has imported
//!
//! Renders what an import of the toolbox would change in git-diff style:
//! resources only in the toolbox are new-file diffs, resources only in the
//! instance's groups are deletions, and changed resources show unified hunks.
//! Comparison happens on the normalized request forms (the same shapes the
//! merge editor uses), so server-only fields like creators, bans, and
//! timestamps never show up as drift.

use colored::Colorize;
use similar::TextDiff;
use std::collections::HashSet;
use thorium::models::{ImageRequest, PipelineRequest};
use thorium::{Error, Thorium};

use super::manifest::ToolboxManifest;
use super::{build, import, policies, shared};
use crate::args::toolbox::{BuildToolbox, DiffToolbox, ManifestLocation};
use crate::handlers::imports::categorize;
use crate::handlers::imports::merge::{MergeableImage, MergeablePipeline};
use crate::utils;
use crate::handlers::progress::Bar;
use crate::utils::images::list_all_images;
use crate::utils::pipelines::list_all_pipelines;

/// Running totals for the trailing summary line
#[derive(Default)]
struct DiffStats {
    /// Resources that exist on both sides but differ
    changed: usize,
    /// Resources only present in the toolbox
    only_toolbox: usize,
    /// Resources in the toolbox's groups that the toolbox doesn't name
    only_instance: usize,
    /// Resources that match exactly
    unchanged: usize,
}

impl DiffStats {
    /// Whether any difference was found
    fn any(&self) -> bool {
        self.changed > 0 || self.only_toolbox > 0 || self.only_instance > 0
    }
}

/// Print one resource's diff with git-style file headers
///
/// # Arguments
///
/// * `old_label` - The old/left-side header (or "/dev/null" for new resources)
/// * `new_label` - The new/right-side header (or "/dev/null" for deletions)
/// * `old_text` - The old/left-side rendered text (image/pipeline/policy)
/// * `new_text` - The new/right-side rendered text
fn print_diff(old_label: &str, new_label: &str, old_text: &str, new_text: &str) {
    let diff = TextDiff::from_lines(old_text, new_text);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string();
    for line in unified.lines() {
        // colorize like git: headers bold, adds green, removes red, hunks cyan
        if line.starts_with("---") || line.starts_with("+++") {
            println!("{}", line.bold());
        } else if line.starts_with('@') {
            println!("{}", line.cyan());
        } else if line.starts_with('+') {
            println!("{}", line.green());
        } else if line.starts_with('-') {
            println!("{}", line.red());
        } else {
            println!("{line}");
        }
    }
    println!();
}

/// Render one resource's diff against the instance, updating the running stats
///
/// Shared by the image and pipeline passes: a resource absent from the instance
/// is a new-file diff, an identical one is silent, and a differing one prints a
/// unified hunk. The caller supplies the already-serialized YAML and the labels
/// so this stays agnostic to the resource kind.
///
/// # Arguments
///
/// * `stats` - The running diff totals to update
/// * `instance_label` - The instance-side header for a changed resource
/// * `toolbox_path` - The toolbox-side header (used for new and changed alike)
/// * `old_text` - The instance-side YAML, or `None` if it only exists in the toolbox
/// * `new_text` - The toolbox-side YAML
fn render_diff_entry(
    stats: &mut DiffStats,
    instance_label: &str,
    toolbox_path: &str,
    old_text: Option<&str>,
    new_text: &str,
) {
    match old_text {
        // only in the toolbox: render as a new file
        None => {
            stats.only_toolbox += 1;
            print_diff("/dev/null", toolbox_path, "", new_text);
        }
        // present on both sides and identical: nothing to show
        Some(old) if old == new_text => stats.unchanged += 1,
        // present on both sides but changed: show the hunk
        Some(old) => {
            stats.changed += 1;
            print_diff(instance_label, toolbox_path, old, new_text);
        }
    }
}

/// Serialize an image request to the normalized YAML used for comparison
///
/// Uses a canonical (sorted-key) form so reordered map fields (e.g. `env`) don't
/// show up as drift.
///
/// # Arguments
///
/// * `request` - The image request to serialize
fn image_yaml(request: &ImageRequest) -> Result<String, Error> {
    utils::canonical_yaml(&MergeableImage::from(request.clone()))
        .map_err(|err| Error::new(format!("Failed to serialize image for diff: {err}")))
}

/// Serialize a pipeline request to the normalized YAML used for comparison
///
/// Uses a canonical (sorted-key) form so reordered map fields (e.g. `triggers`)
/// don't show up as drift.
///
/// # Arguments
///
/// * `request` - The pipeline request to serialize
fn pipeline_yaml(request: &PipelineRequest) -> Result<String, Error> {
    utils::canonical_yaml(&MergeablePipeline::from(request.clone()))
        .map_err(|err| Error::new(format!("Failed to serialize pipeline for diff: {err}")))
}

/// Whether a group-listing error just means the group isn't visible to us
///
/// A 404 (group absent) or 403 (not a member) means the group simply holds no
/// resources we can see, which is safely treated as empty. Any other status (auth,
/// transient 5xx) is a genuine failure the caller should surface instead of
/// silently under-reporting instance-only drift.
///
/// # Arguments
///
/// * `err` - The error returned while listing a group's images or pipelines
fn is_invisible_group(err: &Error) -> bool {
    matches!(err.status().map(|status| status.as_u16()), Some(403 | 404))
}

/// Load the toolbox manifest from a file, URL, or repo directory
///
/// Directories are built in-memory with image urls preserved so the diff
/// reflects the configs exactly as they sit on disk.
///
/// # Arguments
///
/// * `location` - Where the toolbox lives
async fn load_manifest(location: &ManifestLocation) -> Result<(ToolboxManifest, Bar), Error> {
    // a directory is a toolbox repo checkout; build it in memory
    if let ManifestLocation::Path(path) = location
        && path.is_dir()
    {
        let config = path.join("config.toml");
        if !config.exists() {
            return Err(Error::new(format!(
                "'{}' has no config.toml; point at a toolbox repo root or a toolbox.json",
                path.display()
            )));
        }
        let build_cmd = BuildToolbox {
            config,
            use_image_path: false,
            // never written; build_in_memory ignores the output path
            output: path.join("toolbox.json"),
            path: path.clone(),
            // diff compares the on-disk toolbox as-is, so no tag suffix is applied
            tag_suffix: None,
        };
        // build walks the tree with synchronous std::fs, so run it off the async runtime
        let value = tokio::task::spawn_blocking(move || build::build_in_memory(&build_cmd))
            .await
            .map_err(|err| Error::new(format!("Toolbox build task failed: {err}")))??;
        let manifest: ToolboxManifest = serde_json::from_value(value)
            .map_err(|err| Error::new(format!("Failed to assemble toolbox manifest: {err}")))?;
        let progress = Bar::new(
            "",
            "Diffing toolbox",
            crate::handlers::progress::BarKind::Timer,
        );
        return Ok((manifest, progress));
    }
    shared::get_manifest(location).await
}

/// Diff a toolbox against the instance and print the result
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The toolbox diff command that was run
pub async fn diff(thorium: Thorium, cmd: &DiffToolbox) -> Result<bool, Error> {
    let (mut manifest, progress) = load_manifest(&cmd.manifest).await?;
    shared::resolve_manifest_configs(&mut manifest, &progress).await?;
    // compare against the groups an import (with the same flags) would target
    if let Some(group_override) = &cmd.group_override {
        manifest = manifest.override_group(group_override);
    }
    // categorize everything against the live instance
    let images = categorize::categorize_images(
        &thorium,
        import::flatten_manifest_images(&manifest),
        &progress,
    )
    .await?;
    let pipelines = categorize::categorize_pipelines(
        &thorium,
        import::flatten_manifest_pipelines(&manifest),
        &progress,
    )
    .await?;
    let policy_plan = policies::categorize_policies(
        &thorium,
        policies::collect_policies(&manifest, cmd.group_override.as_deref(), &progress),
        // diff is read-only: always categorize as warn-only so it never plans a mutation
        false,
        &progress,
    )
    .await?;
    // the groups the toolbox targets, where we'll look for resources it doesn't name
    let groups = manifest.groups();
    // index the toolbox's image identities so instance-only images can be detected
    let toolbox_images: HashSet<(String, String)> = images
        .iter()
        .map(|img| (img.request.group.clone(), img.request.name.clone()))
        .collect();
    // index the toolbox's pipeline identities for the same instance-only detection
    let toolbox_pipelines: HashSet<(String, String)> = pipelines
        .iter()
        .map(|pipe| (pipe.request.group.clone(), pipe.request.name.clone()))
        .collect();
    // list every group's images and pipelines concurrently; a group we can't see
    // (absent or non-member) can't hold extra resources, so its listing is treated
    // as empty, but a genuine failure is surfaced rather than hiding drift
    let thorium_ref = &thorium;
    let progress_ref = &progress;
    let group_listings = futures::future::join_all(groups.iter().map(|group| async move {
        // list this group's images, warning on a real failure before treating it empty
        let images = match list_all_images(thorium_ref, group).await {
            Ok(images) => images,
            Err(err) if is_invisible_group(&err) => Vec::new(),
            Err(err) => {
                progress_ref.warning(format!(
                    "Could not list images in group '{group}': {err} (treating as empty)"
                ));
                Vec::new()
            }
        };
        // list this group's pipelines with the same visible-vs-genuine error handling
        let pipelines = match list_all_pipelines(thorium_ref, group).await {
            Ok(pipelines) => pipelines,
            Err(err) if is_invisible_group(&err) => Vec::new(),
            Err(err) => {
                progress_ref.warning(format!(
                    "Could not list pipelines in group '{group}': {err} (treating as empty)"
                ));
                Vec::new()
            }
        };
        (images, pipelines)
    }))
    .await;
    let mut instance_only_images = Vec::new();
    let mut instance_only_pipelines = Vec::new();
    for (group_images, group_pipelines) in group_listings {
        for image in group_images {
            if !toolbox_images.contains(&(image.group.clone(), image.name.clone())) {
                instance_only_images.push(image);
            }
        }
        for pipeline in group_pipelines {
            if !toolbox_pipelines.contains(&(pipeline.group.clone(), pipeline.name.clone())) {
                instance_only_pipelines.push(pipeline);
            }
        }
    }
    progress.finish_and_clear();
    // render all the diffs
    let mut stats = DiffStats::default();
    for img in &images {
        let label = format!("{}/{}", img.request.group, img.request.name);
        let new_text = image_yaml(&img.request)?;
        // normalize the live image through its request form so server-only
        // fields don't show as drift
        let old_text = match &img.existing {
            Some(existing) => Some(image_yaml(&ImageRequest::from(existing.clone()))?),
            None => None,
        };
        render_diff_entry(
            &mut stats,
            &format!("instance/{label} (image)"),
            &format!("toolbox/images/{}", img.name),
            old_text.as_deref(),
            &new_text,
        );
    }
    for pipe in &pipelines {
        let label = format!("{}/{}", pipe.request.group, pipe.request.name);
        let new_text = pipeline_yaml(&pipe.request)?;
        let old_text = match &pipe.existing {
            Some(existing) => Some(pipeline_yaml(&PipelineRequest::from(existing.clone()))?),
            None => None,
        };
        render_diff_entry(
            &mut stats,
            &format!("instance/{label} (pipeline)"),
            &format!("toolbox/pipelines/{}", pipe.name),
            old_text.as_deref(),
            &new_text,
        );
    }
    // policies: new ones render as new files, mismatches as summary lines
    // (the instance side is authoritative and never updated by imports)
    for policy in &policy_plan.new {
        stats.only_toolbox += 1;
        let new_text = utils::canonical_yaml(policy)
            .map_err(|err| Error::new(format!("Failed to serialize policy for diff: {err}")))?;
        print_diff(
            "/dev/null",
            &format!("toolbox/network-policies/{}", policy.name),
            "",
            &new_text,
        );
    }
    for mismatch in &policy_plan.mismatched {
        stats.changed += 1;
        // attribute the difference to the instance's existing policy, splitting rule/flag
        // drift from a coverage gap so the suggested action matches each case
        let name = &mismatch.name;
        if !mismatch.drift.is_empty() {
            println!(
                "{} network policy '{name}' already exists in the instance with a different \
                 definition (differs: [{}]); use --update-network-policy on import to overwrite it\n",
                "note:".cyan(),
                mismatch.drift.join(", ")
            );
        }
        if !mismatch.missing_groups.is_empty() {
            println!(
                "{} network policy '{name}' exists in the instance but not in group(s) {:?}; \
                 use --update-network-policy on import to add those group(s)\n",
                "note:".cyan(),
                mismatch.missing_groups
            );
        }
    }
    stats.unchanged += policy_plan.unchanged;
    // resources only in the instance are listed compactly: a partial toolbox
    // (one pipeline out of a big group) would otherwise drown the real diff
    // in full-body deletions
    for image in &instance_only_images {
        stats.only_instance += 1;
        println!(
            "{} instance/{}/{} (image) — not in this toolbox",
            "only in instance:".yellow(),
            image.group,
            image.name
        );
    }
    for pipeline in &instance_only_pipelines {
        stats.only_instance += 1;
        println!(
            "{} instance/{}/{} (pipeline) — not in this toolbox",
            "only in instance:".yellow(),
            pipeline.group,
            pipeline.name
        );
    }
    if stats.only_instance > 0 {
        println!();
    }
    // trailing summary like git's diffstat footer
    println!(
        "{} changed, {} only in toolbox, {} only in instance, {} unchanged",
        stats.changed, stats.only_toolbox, stats.only_instance, stats.unchanged
    );
    // git diff --exit-code semantics: tell the caller whether to exit non-zero on
    // drift. The actual process exit happens at the dispatch boundary so this
    // function returns normally and its resources drop first.
    Ok(cmd.exit_code && stats.any())
}
