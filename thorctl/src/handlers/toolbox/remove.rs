//! Remove a previously imported toolbox's resources from Thorium
//!
//! The manifest is the source of truth for what to remove: every pipeline and
//! image it names is deleted from the target instance if present. Pipelines
//! are deleted before the images they reference so the API never sees an
//! image deletion that would orphan a pipeline. Groups are never deleted.

use colored::Colorize;
use http::StatusCode;
use std::collections::{HashMap, HashSet};
use thorium::{CtlConf, Error, Thorium};

use super::shared;
use crate::args::toolbox::RemoveToolbox;
use crate::handlers::imports::categorize::{self, CategorizedImage, CategorizedPipeline};
use crate::handlers::progress::{Bar, BarKind};

/// A resource's `(group, name)` identity in Thorium
type Identity = (String, String);

/// Collapse categorized entries into the unique `(group, name)` identities that
/// exist and should be deleted, and flag the identities the manifest defines more
/// than once
///
/// Deleting the same identity twice would error on the second call, so existing
/// targets are de-duplicated (first-seen order). A duplicated identity also means
/// an import could have resolved the collision by renaming the extras to names
/// this manifest doesn't carry — so those copies can't be targeted for removal
/// and the caller should warn about them.
///
/// # Arguments
///
/// * `entries` - `(group, name, exists)` for every flattened manifest entry
fn dedup_targets<'a, I>(entries: I) -> (Vec<Identity>, HashSet<Identity>)
where
    I: IntoIterator<Item = (&'a str, &'a str, bool)>,
{
    let mut counts: HashMap<Identity, usize> = HashMap::new();
    let mut targets: Vec<Identity> = Vec::new();
    let mut seen: HashSet<Identity> = HashSet::new();
    for (group, name, exists) in entries {
        let key = (group.to_string(), name.to_string());
        *counts.entry(key.clone()).or_default() += 1;
        // keep one delete target per existing identity, in first-seen order
        if exists && seen.insert(key.clone()) {
            targets.push(key);
        }
    }
    let duplicates = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(key, _)| key)
        .collect();
    (targets, duplicates)
}

/// Confirm the removal with the user, listing what exists and what doesn't
///
/// # Arguments
///
/// * `conf` - The Thorctl config (used to display the API URL)
/// * `pipelines` - The categorized pipelines named by the manifest
/// * `images` - The categorized images named by the manifest
fn confirm_remove(
    conf: &CtlConf,
    pipelines: &[CategorizedPipeline],
    images: &[CategorizedImage],
) -> Result<bool, Error> {
    let found_pipelines: Vec<_> = pipelines
        .iter()
        .filter(|pipe| pipe.existing.is_some())
        .collect();
    let found_images: Vec<_> = images.iter().filter(|img| img.existing.is_some()).collect();
    if !found_pipelines.is_empty() {
        println!("{}", "Pipelines to delete:".bright_red());
        for pipe in &found_pipelines {
            println!("  {}:{}", pipe.request.group, pipe.request.name);
        }
    }
    if !found_images.is_empty() {
        println!("{}", "Images to delete:".bright_red());
        for img in &found_images {
            println!("  {}:{}", img.request.group, img.request.name);
        }
    }
    let missing: Vec<String> = pipelines
        .iter()
        .filter(|pipe| pipe.existing.is_none())
        .map(|pipe| format!("pipeline {}:{}", pipe.request.group, pipe.request.name))
        .chain(
            images
                .iter()
                .filter(|img| img.existing.is_none())
                .map(|img| format!("image {}:{}", img.request.group, img.request.name)),
        )
        .collect();
    if !missing.is_empty() {
        println!("{}", "Not found (skipped):".bright_blue());
        for line in missing {
            println!("  {line}");
        }
    }
    println!();
    let response = dialoguer::Confirm::new()
        .with_prompt(format!(
            "Delete the resources listed above from '{}'?",
            conf.keys.api.bright_green()
        ))
        .default(false)
        .interact()?;
    Ok(response)
}

/// Remove a toolbox's pipelines and images from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The toolbox remove command that was run
pub async fn remove(thorium: Thorium, conf: CtlConf, cmd: &RemoveToolbox) -> Result<(), Error> {
    // load the manifest exactly like import does, including remote configs
    let location = &cmd.manifest;
    let (mut manifest, progress) = shared::get_manifest(location).await?;
    shared::resolve_manifest_configs(&mut manifest, &progress).await?;
    // removal targets the same groups an import (with the same flags) would hit
    if let Some(group_override) = &cmd.group_override {
        manifest = manifest.override_group(group_override);
    }
    // categorize so we only try to delete what actually exists
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
    // collapse duplicate (group, name) targets so we never delete the same
    // resource twice (the second call would error), and flag identities the
    // toolbox defines more than once
    let (pipeline_targets, pipeline_dups) = dedup_targets(
        pipelines.iter().map(|pipe| {
            (
                pipe.request.group.as_str(),
                pipe.request.name.as_str(),
                pipe.existing.is_some(),
            )
        }),
    );
    let (image_targets, image_dups) = dedup_targets(images.iter().map(|img| {
        (
            img.request.group.as_str(),
            img.request.name.as_str(),
            img.existing.is_some(),
        )
    }));
    // a duplicated identity means an import may have renamed the extras to names
    // we can't derive from this manifest, so those copies may still remain
    warn_duplicates("Pipeline", pipeline_dups, &progress);
    warn_duplicates("Image", image_dups, &progress);
    if pipeline_targets.is_empty() && image_targets.is_empty() {
        progress.finish();
        println!("Nothing to remove: no resources from this toolbox exist in the instance");
        return Ok(());
    }
    // deleting is irreversible, so confirm exactly what will be removed
    if !cmd.skip_confirm {
        // fail clearly (not with a raw dialoguer error) when we can't prompt
        crate::utils::require_confirm_terminal("--skip-confirm (-y)")?;
        let confirmed = progress.suspend(|| confirm_remove(&conf, &pipelines, &images))?;
        if !confirmed {
            return Ok(());
        }
    }
    // labels of resources whose deletion failed; collected so one failure doesn't abort
    // the rest — we make as much progress as possible and report everything at the end
    let mut failures: Vec<String> = Vec::new();
    // delete pipelines first so no image deletion can orphan one
    progress.refresh(
        "Deleting pipelines",
        BarKind::Bound(pipeline_targets.len() as u64),
    );
    for (group, name) in &pipeline_targets {
        // a missing resource is treated as already-removed, not a failure, so a
        // collision or a re-run never aborts the rest of the removal
        match thorium.pipelines.delete(group, name).await {
            Ok(_) => progress.info_anonymous(format!("Deleted pipeline '{group}:{name}'")),
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => {
                progress.info_anonymous(format!("Pipeline '{group}:{name}' already removed"));
            }
            // log the failure and keep going so the remaining resources still get deleted
            Err(err) => {
                progress.warning(format!("Failed to delete pipeline '{group}:{name}': {err}"));
                failures.push(format!("pipeline {group}:{name}"));
            }
        }
        progress.inc(1);
    }
    progress.refresh(
        "Deleting images",
        BarKind::Bound(image_targets.len() as u64),
    );
    for (group, name) in &image_targets {
        match thorium.images.delete(group, name).await {
            Ok(_) => progress.info_anonymous(format!("Deleted image '{group}:{name}'")),
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => {
                progress.info_anonymous(format!("Image '{group}:{name}' already removed"));
            }
            // log the failure and keep going; a pipeline that still references this image
            // (e.g. its own delete failed above) is the likely cause and is reported too
            Err(err) => {
                progress.warning(format!("Failed to delete image '{group}:{name}': {err}"));
                failures.push(format!("image {group}:{name}"));
            }
        }
        progress.inc(1);
    }
    // every target was attempted; if any failed, surface them all and exit non-zero so a
    // partial removal isn't silently reported as a success
    if !failures.is_empty() {
        progress.refresh("Removal finished with errors", BarKind::Timer);
        progress.finish();
        return Err(Error::new(format!(
            "Failed to delete {} resource(s): {}",
            failures.len(),
            failures.join(", ")
        )));
    }
    progress.refresh("Removal complete!", BarKind::Timer);
    progress.finish();
    Ok(())
}

/// Warn that duplicated `(group, name)` identities may have left renamed copies
/// behind that this removal can't target
///
/// # Arguments
///
/// * `kind` - "Image" or "Pipeline", used to start the message
/// * `duplicates` - The duplicated identities
/// * `progress` - The progress bar to warn through
fn warn_duplicates(kind: &str, duplicates: HashSet<Identity>, progress: &Bar) {
    let mut duplicates: Vec<Identity> = duplicates.into_iter().collect();
    duplicates.sort();
    for (group, name) in duplicates {
        progress.warning(format!(
            "{kind} '{}:{}' is defined more than once in the toolbox; if a prior import \
             renamed the duplicates, those renamed copies were not removed",
            group.bright_yellow(),
            name.bright_yellow(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Existing identities are de-duplicated (kept once, in order) and repeated
    /// identities are flagged; non-existent entries are never delete targets
    #[test]
    fn dedups_existing_and_flags_duplicates() {
        let entries = vec![
            ("static", "exiftool", true),
            ("static", "exiftool", true), // same identity again -> a duplicate
            ("static", "yara", true),
            ("static", "ghost", false), // not in the instance
        ];
        let (targets, duplicates) = dedup_targets(entries);
        assert_eq!(
            targets,
            vec![
                ("static".to_string(), "exiftool".to_string()),
                ("static".to_string(), "yara".to_string()),
            ]
        );
        assert!(duplicates.contains(&("static".to_string(), "exiftool".to_string())));
        assert!(!duplicates.contains(&("static".to_string(), "yara".to_string())));
        // ghost doesn't exist, so it's neither a delete target nor a duplicate
        assert!(!targets.contains(&("static".to_string(), "ghost".to_string())));
    }
}
