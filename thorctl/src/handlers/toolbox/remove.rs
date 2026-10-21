//! Remove a previously imported toolbox's resources from Thorium
//!
//! The manifest is the source of truth for what to remove. It is prepared exactly like a
//! non-interactive import (structural validation, group override, group coherence, and
//! collision resolution), so only resources an import would have created are targeted;
//! every surviving pipeline and image is deleted from the target instance if present.
//! Pipelines are deleted before the images they reference, and images still used by
//! pipelines outside the toolbox are left in place (and reported) before anything is
//! deleted. Groups and network policies are never deleted.

use colored::Colorize;
use http::StatusCode;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;
use thorium::{CtlConf, Error, Thorium};

use super::manifest::ToolboxManifest;
use super::{collisions, shared};
use crate::args::toolbox::{ManifestLocation, RemoveToolbox};
use crate::handlers::imports::categorize::{self, CategorizedImage, CategorizedPipeline};
use crate::handlers::imports::kind::{ImageKind, ImportKind, PipelineKind};
use crate::handlers::progress::{Bar, BarKind};
use crate::utils;

/// A resource's `(group, name)` identity in Thorium
type Identity = (String, String);

/// An existing resource this removal will try to delete
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// The group the resource lives in
    group: String,
    /// The resource's name
    name: String,
    /// Whether the live resource differs from the toolbox's definition (it was changed
    /// after import, or it wasn't created by the toolbox at all)
    modified: bool,
}

/// Collapse categorized entries into the unique existing targets and the unique
/// identities that don't exist in the instance
///
/// Deleting the same identity twice would error on the second call, so targets are
/// de-duplicated. Both lists are sorted so the listing and the delete order are the
/// same on every run.
///
/// # Arguments
///
/// * `entries` - `(group, name, changed)` for every flattened manifest entry, where
///   `changed` is `None` when the resource doesn't exist and otherwise whether the
///   live resource differs from the entry
fn collect_targets<'a, I>(entries: I) -> (Vec<Target>, Vec<Identity>)
where
    I: IntoIterator<Item = (&'a str, &'a str, Option<bool>)>,
{
    // existing identities mapped to whether any entry for them differs from the live copy
    let mut existing: BTreeMap<Identity, bool> = BTreeMap::new();
    // identities the instance doesn't have
    let mut missing: BTreeSet<Identity> = BTreeSet::new();
    for (group, name, changed) in entries {
        // the owned identity is the key for both collections
        let key = (group.to_string(), name.to_string());
        match changed {
            // an existing identity is a target once, flagged if any entry differs
            Some(changed) => *existing.entry(key).or_default() |= changed,
            // an absent identity is only reported
            None => {
                missing.insert(key);
            }
        }
    }
    // turn the existing identities into sorted targets
    let targets = existing
        .into_iter()
        .map(|((group, name), modified)| Target {
            group,
            name,
            modified,
        })
        .collect();
    (targets, missing.into_iter().collect())
}

/// The pipelines outside this removal that still use an image
///
/// The API refuses to delete an image while any pipeline uses it, so an image whose
/// users aren't all being deleted here can't be removed.
///
/// # Arguments
///
/// * `used_by` - The names of the pipelines that use the image (in the image's group)
/// * `group` - The image's group
/// * `pipeline_targets` - The pipelines this removal deletes
fn blocking_pipelines(used_by: &[String], group: &str, pipeline_targets: &[Target]) -> Vec<String> {
    // keep every user that isn't one of the pipelines being deleted from the same group
    let mut blocking: Vec<String> = used_by
        .iter()
        .filter(|user| {
            !pipeline_targets
                .iter()
                .any(|pipe| pipe.group == group && &pipe.name == *user)
        })
        .cloned()
        .collect();
    // sort for deterministic output
    blocking.sort_unstable();
    blocking
}

/// The `(kind, group, name)` identities of every configured image and pipeline in a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest to collect identities from
fn identities(manifest: &ToolboxManifest) -> BTreeSet<(&'static str, String, String)> {
    // every configured image version contributes its identity
    let images = manifest
        .images
        .values()
        .flat_map(|image| image.versions.values())
        .filter_map(|version| version.config.as_ref())
        .map(|config| ("Image", config.group.clone(), config.name.clone()));
    // every configured pipeline version contributes its identity
    let pipelines = manifest
        .pipelines
        .values()
        .flat_map(|pipeline| pipeline.versions.values())
        .filter_map(|version| version.config.as_ref())
        .map(|config| ("Pipeline", config.group.clone(), config.name.clone()));
    images.chain(pipelines).collect()
}

/// Print a per-resource result, even when the progress bar is hidden
///
/// A hidden bar (no TTY, CI, piped output) drops its printed lines, which would leave a
/// non-interactive removal with no record of what it deleted, so this falls back to stdout.
///
/// # Arguments
///
/// * `progress` - The progress bar to print through when it is visible
/// * `msg` - The message to print
fn report(progress: &Bar, msg: &str) {
    // a visible bar prints above itself so the line isn't overwritten
    if progress.is_visible() {
        progress.info_anonymous(msg);
    } else {
        // otherwise print directly so the line is never lost
        println!("{msg}");
    }
}

/// Finish the progress bar with a final banner, even when the bar is hidden
///
/// # Arguments
///
/// * `progress` - The progress bar to finish
/// * `msg` - The final banner
fn finish(progress: &Bar, msg: &'static str) {
    // a visible bar shows the banner as its final message
    if progress.is_visible() {
        progress.refresh(msg, BarKind::Timer);
        progress.finish();
    } else {
        // otherwise print the banner directly
        println!("{msg}");
    }
}

/// Format a target for the listing, noting when it differs from the toolbox
///
/// # Arguments
///
/// * `target` - The target to format
fn describe_target(target: &Target) -> String {
    if target.modified {
        format!(
            "  {} {}",
            utils::resource_id(&target.group, &target.name),
            "(differs from the toolbox definition)".bright_yellow()
        )
    } else {
        format!("  {}", utils::resource_id(&target.group, &target.name))
    }
}

/// What a removal will delete, leave in place, and skip
struct RemovalPlan {
    /// The pipelines to delete
    pipelines: Vec<Target>,
    /// The images to delete
    images: Vec<Target>,
    /// Images that can't be deleted and the outside pipelines still using them
    blocked: Vec<(Target, Vec<String>)>,
    /// Pipelines the manifest names that don't exist in the instance
    missing_pipelines: Vec<Identity>,
    /// Images the manifest names that don't exist in the instance
    missing_images: Vec<Identity>,
}

impl RemovalPlan {
    /// Whether the plan deletes nothing at all
    fn deletes_nothing(&self) -> bool {
        self.pipelines.is_empty() && self.images.is_empty()
    }

    /// Print what the removal will delete, leave in place, and skip
    fn print(&self) {
        // pipelines first because removal deletes them first; headers are skipped for
        // empty sections so the listing isn't cluttered
        if !self.pipelines.is_empty() {
            println!("{}", "Pipelines to delete:".bright_red());
            for pipe in &self.pipelines {
                println!("{}", describe_target(pipe));
            }
        }
        // then images, which are deleted after the pipelines that may reference them
        if !self.images.is_empty() {
            println!("{}", "Images to delete:".bright_red());
            for img in &self.images {
                println!("{}", describe_target(img));
            }
        }
        // images the API would refuse to delete are listed so the user knows up front
        if !self.blocked.is_empty() {
            println!(
                "{}",
                "Images left in place (still used by pipelines outside this toolbox):"
                    .bright_yellow()
            );
            for (img, users) in &self.blocked {
                println!(
                    "  {} (used by: [{}])",
                    utils::resource_id(&img.group, &img.name),
                    users.join(", ")
                );
            }
        }
        // everything the manifest names but the instance doesn't have is a no-op
        if !self.missing_pipelines.is_empty() || !self.missing_images.is_empty() {
            println!("{}", "Not found (skipped):".bright_blue());
            for (group, name) in &self.missing_pipelines {
                println!("  pipeline {}", utils::resource_id(group, name));
            }
            for (group, name) in &self.missing_images {
                println!("  image {}", utils::resource_id(group, name));
            }
        }
        // removal never touches groups or policies, so say so where the user decides
        println!("Groups and network policies are never removed.");
    }
}

/// Load and prepare the manifest exactly like a non-interactive import would
///
/// Network policy URLs are not fetched because removal never touches policies, so a
/// dead policy URL can't block a removal. Collisions are resolved non-interactively
/// (skip + warn); an identity that is dropped here isn't removed, and a warning says so,
/// because a prior interactive import may have kept or renamed it.
///
/// # Arguments
///
/// * `cmd` - The toolbox remove command that was run
async fn prepare_manifest(cmd: &RemoveToolbox) -> Result<(ToolboxManifest, Bar), Error> {
    // a toolbox directory must be built first; say so instead of failing to read it
    if let ManifestLocation::Path(path) = &cmd.manifest
        && path.is_dir()
    {
        return Err(Error::new(format!(
            "'{}' is a directory; pass its toolbox.json (run `thorctl toolbox build` to \
             generate one)",
            path.display()
        )));
    }
    // load the manifest the same way import does (path or URL)
    let (mut manifest, progress) =
        shared::get_manifest_named(&cmd.manifest, "toolbox remove").await?;
    // removal never touches policies, so don't fetch their URLs
    manifest
        .images
        .values_mut()
        .flat_map(|image| image.versions.values_mut())
        .for_each(|version| version.network_policies_from.clear());
    // resolve URL-backed configs, which are needed to learn each resource's group and name
    shared::resolve_manifest_configs(&mut manifest, &progress)
        .await
        .map_err(|err| {
            Error::new(format!(
                "Failed to resolve the toolbox's remote configs, which are needed to identify \
                 what to remove: {err}"
            ))
        })?;
    // drop intrinsically-invalid versions, which an import would never have created
    shared::warn_dropped(&manifest.validate_structural(), &progress);
    // snapshot the pre-override groups so collision resolution matches import's
    let sources = manifest.capture_source_groups();
    // apply the same group override an import would have used
    if let Some(group_override) = &cmd.group_override {
        manifest = manifest.override_group(group_override);
    }
    // drop pipelines whose images aren't in their final group, as import does
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // remember what survived so far to report what collision resolution drops
    let before = identities(&manifest);
    // resolve collisions non-interactively, like a non-interactive import
    collisions::resolve_collisions(&mut manifest, &sources, false, &progress)?;
    // report every identity collision resolution dropped, since it may still exist
    for (kind, group, name) in before.difference(&identities(&manifest)) {
        progress.warning(format!(
            "{kind} '{}' was not removed because its toolbox definition collides \
             (or depends on a colliding image); a prior interactive import may have kept or \
             renamed it, so check for it and remove it manually if needed",
            utils::resource_id(group, name)
        ));
    }
    // re-check coherence after resolution, as import does
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    Ok((manifest, progress))
}

/// Build the removal plan from the categorized pipelines and images
///
/// # Arguments
///
/// * `pipelines` - The categorized pipelines the manifest names
/// * `images` - The categorized images the manifest names
fn build_plan(pipelines: &[CategorizedPipeline], images: &[CategorizedImage]) -> RemovalPlan {
    // collapse pipelines into unique targets, flagging those that differ from the toolbox
    let (pipeline_targets, missing_pipelines) = collect_targets(pipelines.iter().map(|pipe| {
        (
            pipe.request.group.as_str(),
            pipe.request.name.as_str(),
            pipe.existing
                .as_ref()
                .map(|existing| PipelineKind::changed(existing, &pipe.request)),
        )
    }));
    // collapse images the same way
    let (image_targets, missing_images) = collect_targets(images.iter().map(|img| {
        (
            img.request.group.as_str(),
            img.request.name.as_str(),
            img.existing
                .as_ref()
                .map(|existing| ImageKind::changed(existing, &img.request)),
        )
    }));
    // index which pipelines use each existing image
    let used_by: HashMap<Identity, &[String]> = images
        .iter()
        .filter_map(|img| {
            img.existing.as_ref().map(|existing| {
                (
                    (existing.group.clone(), existing.name.clone()),
                    existing.used_by.as_slice(),
                )
            })
        })
        .collect();
    // split the images into deletable ones and those still used outside this removal
    let mut deletable = Vec::new();
    let mut blocked = Vec::new();
    for target in image_targets {
        // find the pipelines outside this removal that still use the image
        let users = used_by
            .get(&(target.group.clone(), target.name.clone()))
            .map_or_else(Vec::new, |used_by| {
                blocking_pipelines(used_by, &target.group, &pipeline_targets)
            });
        if users.is_empty() {
            deletable.push(target);
        } else {
            blocked.push((target, users));
        }
    }
    RemovalPlan {
        pipelines: pipeline_targets,
        images: deletable,
        blocked,
        missing_pipelines,
        missing_images,
    }
}

/// Confirm the removal with the user
///
/// # Arguments
///
/// * `conf` - The Thorctl config (used to display the API URL)
fn confirm_remove(conf: &CtlConf) -> Result<bool, Error> {
    // blank line separates the listing from the prompt for readability
    println!();
    // default to No so a stray Enter never deletes anything; the prompt names the
    // API url so the user can confirm they are pointed at the right instance
    let response = dialoguer::Confirm::new()
        .with_prompt(format!(
            "Delete the resources listed above from '{}'?",
            conf.keys.api.bright_green()
        ))
        .default(false)
        .interact()?;
    Ok(response)
}

/// Delete every pipeline and then every image in a removal plan
///
/// One failure never aborts the rest: each failure is warned about and its label is
/// returned so the caller can report everything at the end.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `plan` - The removal plan to carry out
/// * `progress` - The progress bar to report through
async fn delete_targets(thorium: &Thorium, plan: &RemovalPlan, progress: &Bar) -> Vec<String> {
    // labels of the resources whose deletion failed
    let mut failures: Vec<String> = Vec::new();
    // delete pipelines first so no image deletion can orphan one
    progress.refresh(
        "Deleting pipelines",
        BarKind::Bound(plan.pipelines.len() as u64),
    );
    for Target { group, name, .. } in &plan.pipelines {
        // name the pipeline the same way in every message below
        let id = utils::resource_id(group, name);
        // a missing resource is treated as already-removed, not a failure, so a
        // re-run never aborts the rest of the removal
        match thorium.pipelines.delete(group, name).await {
            Ok(_) => report(progress, &format!("Deleted pipeline '{id}'")),
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => {
                report(progress, &format!("Pipeline '{id}' already removed"));
            }
            // log the failure and keep going so the remaining resources still get deleted
            Err(err) => {
                progress.warning(format!("Failed to delete pipeline '{id}': {err}"));
                failures.push(format!("pipeline {id}"));
            }
        }
        // advance the bar whether the delete succeeded, 404'd, or failed, since
        // every outcome is one fully-handled target
        progress.inc(1);
    }
    // images are deleted only after all pipelines; size the bar to the deletable images
    progress.refresh("Deleting images", BarKind::Bound(plan.images.len() as u64));
    for Target { group, name, .. } in &plan.images {
        // name the image the same way in every message below
        let id = utils::resource_id(group, name);
        // as with pipelines, a 404 means the image is already gone and counts as success
        match thorium.images.delete(group, name).await {
            Ok(_) => report(progress, &format!("Deleted image '{id}'")),
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => {
                report(progress, &format!("Image '{id}' already removed"));
            }
            // log the failure and keep going; a pipeline that still references this image
            // (e.g. its own delete failed above) is the likely cause and is reported too
            Err(err) => {
                progress.warning(format!("Failed to delete image '{id}': {err}"));
                failures.push(format!("image {id}"));
            }
        }
        // advance the bar for every handled image target, regardless of outcome
        progress.inc(1);
    }
    failures
}

/// Remove a toolbox's pipelines and images from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The toolbox remove command that was run
pub async fn remove(thorium: Thorium, conf: CtlConf, cmd: &RemoveToolbox) -> Result<(), Error> {
    // load the manifest and prepare it the way a non-interactive import would
    let (manifest, progress) = prepare_manifest(cmd).await?;
    // categorize against the instance so each entry carries its live resource, if any
    let images = categorize::categorize_images(
        &thorium,
        super::import::flatten_manifest_images(&manifest),
        &progress,
    )
    .await?;
    let pipelines = categorize::categorize_pipelines(
        &thorium,
        super::import::flatten_manifest_pipelines(&manifest),
        &progress,
    )
    .await?;
    // decide what to delete, what to leave in place, and what's already absent
    let plan = build_plan(&pipelines, &images);
    // images the API would refuse to delete, labeled for the final error
    let blocked: Vec<String> = plan
        .blocked
        .iter()
        .map(|(img, _)| format!("image {}", utils::resource_id(&img.group, &img.name)))
        .collect();
    // nothing deletable and nothing blocked means this toolbox is already gone
    if plan.deletes_nothing() && blocked.is_empty() {
        progress.finish_and_clear();
        println!("Nothing to remove: no resources from this toolbox exist in the instance");
        return Ok(());
    }
    // always list the plan before acting, so non-interactive runs leave a record too
    progress.suspend(|| plan.print());
    // a dry run stops after the listing
    if cmd.dry_run {
        progress.finish_and_clear();
        println!("Dry run: nothing was deleted");
        return Ok(());
    }
    // deleting is irreversible, so confirm exactly what will be removed
    if !cmd.skip_confirm && !plan.deletes_nothing() {
        // dialoguer reads stdin and draws on stderr, so both must be terminals to prompt
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            progress.finish_and_clear();
            return Err(Error::new(
                "No terminal available to confirm this removal; pass --skip-confirm to \
                 proceed non-interactively, or --dry-run to preview it",
            ));
        }
        // ask the user to confirm the listed removal
        let confirmed = progress.suspend(|| confirm_remove(&conf))?;
        // a declined prompt deletes nothing, and says so
        if !confirmed {
            progress.finish_and_clear();
            println!("Removal cancelled; nothing was deleted");
            return Ok(());
        }
    }
    // delete everything in the plan, collecting the labels of anything that failed
    let failures = delete_targets(&thorium, &plan, &progress).await;
    // anything that failed or was left in place means the toolbox wasn't fully removed,
    // so report it all once and exit non-zero
    if !failures.is_empty() || !blocked.is_empty() {
        finish(&progress, "Removal finished with errors");
        // describe each kind of problem separately
        let mut problems = Vec::new();
        if !failures.is_empty() {
            problems.push(format!(
                "failed to delete {} resource(s): {}",
                failures.len(),
                failures.join(", ")
            ));
        }
        if !blocked.is_empty() {
            problems.push(format!(
                "left {} image(s) in place because pipelines outside this toolbox still use \
                 them: {}",
                blocked.len(),
                blocked.join(", ")
            ));
        }
        return Err(Error::new(format!(
            "Toolbox was not fully removed: {}",
            problems.join("; ")
        )));
    }
    // every target deleted (or already absent) with no failures, so report success
    finish(&progress, "Removal complete!");
    Ok(())
}

/// Unit tests for the removal helpers that have no instance dependency
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a target for the assertions below
    ///
    /// # Arguments
    ///
    /// * `group` - The target's group
    /// * `name` - The target's name
    /// * `modified` - Whether the target differs from the toolbox
    fn target(group: &str, name: &str, modified: bool) -> Target {
        Target {
            group: group.to_string(),
            name: name.to_string(),
            modified,
        }
    }

    /// Existing identities are de-duplicated and sorted, a differing entry flags its
    /// target, and absent identities are only reported as missing
    #[test]
    fn collects_sorted_unique_targets() {
        let entries = vec![
            ("static", "yara", Some(false)),
            // the same identity twice (two versions); one differs, so the target is flagged
            ("static", "exiftool", Some(false)),
            ("static", "exiftool", Some(true)),
            // not present in the instance, listed twice but reported once
            ("static", "ghost", None),
            ("static", "ghost", None),
        ];
        let (targets, missing) = collect_targets(entries);
        // targets are unique and sorted by (group, name)
        assert_eq!(
            targets,
            vec![
                target("static", "exiftool", true),
                target("static", "yara", false)
            ]
        );
        // the absent identity is reported once and never targeted
        assert_eq!(missing, vec![("static".to_string(), "ghost".to_string())]);
    }

    /// Pipelines being deleted in the same group don't block an image; others do
    #[test]
    fn blocking_pipelines_excludes_targets() {
        let used_by = vec![
            "scan".to_string(),
            "custom".to_string(),
            "other".to_string(),
        ];
        // `scan` is deleted in the image's group, `other` only in a different group
        let pipeline_targets = vec![target("g", "scan", false), target("h", "other", false)];
        let blocking = blocking_pipelines(&used_by, "g", &pipeline_targets);
        assert_eq!(blocking, vec!["custom".to_string(), "other".to_string()]);
        // an image whose users are all being deleted isn't blocked
        assert_eq!(
            blocking_pipelines(&["scan".to_string()], "g", &pipeline_targets),
            Vec::<String>::new()
        );
    }
}
