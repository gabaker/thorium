//! Main entry point for toolbox imports
//!
//! Orchestrates the import workflow: loading the manifest, categorizing
//! resources, confirming with the user, and delegating to the appropriate
//! creation or merge handlers.

use colored::Colorize;
use std::collections::HashSet;
use thorium::models::ScrubbedUser;
use thorium::{CtlConf, Error, Thorium};

use super::{categorize, create, merge, shared};
use super::categorize::{CategorizedImage, CategorizedPipeline};
use crate::args::toolbox::ImportToolbox;
use crate::handlers::progress::BarKind;

// ─── Confirmation ────────────────────────────────────────────────────────────

/// Confirm the import with the user, showing what will be created and what
/// already exists
fn confirm_import(
    conf: &CtlConf,
    new_images: &[&CategorizedImage],
    existing_images: &[&CategorizedImage],
    new_pipelines: &[&CategorizedPipeline],
    existing_pipelines: &[&CategorizedPipeline],
    missing_groups: &[String],
    current_user: &ScrubbedUser,
    force: bool,
) -> Result<bool, Error> {
    if !new_images.is_empty() {
        println!("{}", "New Images:".bright_green());
        for img in new_images {
            println!(
                "  {}:{} (group: {})",
                img.image_name, img.version_name, img.request.group
            );
        }
    }
    if !existing_images.is_empty() {
        let label = if force {
            "Existing Images (will be force-updated):".bright_yellow()
        } else {
            "Existing Images (will prompt for action):".bright_yellow()
        };
        println!("{label}");
        for img in existing_images {
            let changed = img
                .existing
                .as_ref()
                .is_some_and(|existing| existing != &img.request);
            let status = if changed {
                "changed".bright_yellow()
            } else {
                "unchanged".bright_blue()
            };
            println!(
                "  {}:{} (group: {}) [{}]",
                img.image_name, img.version_name, img.request.group, status
            );
        }
    }
    if !new_pipelines.is_empty() {
        println!("{}", "New Pipelines:".bright_green());
        for pipe in new_pipelines {
            println!(
                "  {}:{} (group: {})",
                pipe.pipeline_name, pipe.version_name, pipe.request.group
            );
        }
    }
    if !existing_pipelines.is_empty() {
        let label = if force {
            "Existing Pipelines (will be force-updated):".bright_yellow()
        } else {
            "Existing Pipelines (will prompt for action):".bright_yellow()
        };
        println!("{label}");
        for pipe in existing_pipelines {
            let changed = pipe
                .existing
                .as_ref()
                .is_some_and(|existing| existing != &pipe.request);
            let status = if changed {
                "changed".bright_yellow()
            } else {
                "unchanged".bright_blue()
            };
            println!(
                "  {}:{} (group: {}) [{}]",
                pipe.pipeline_name, pipe.version_name, pipe.request.group, status
            );
        }
    }
    if !missing_groups.is_empty() {
        println!("{}", "New Groups:".bright_green());
        for group in missing_groups {
            println!("  {group}");
        }
    }
    println!();
    let response = dialoguer::Confirm::new()
        .with_prompt(format!(
            "Import the above items to Thorium instance at '{}' as user '{}'?",
            conf.keys.api.bright_green(),
            current_user.username.bright_green()
        ))
        .interact()?;
    Ok(response)
}

// ─── Main Import Entry Point ─────────────────────────────────────────────────

/// Import a toolbox into Thorium by the given manifest file.
///
/// When images or pipelines already exist, the user is prompted interactively
/// to Edit (merge editor), Skip, Apply (accept incoming), or Quit for each
/// changed resource. Use `--force` to skip the editor and auto-apply all changes.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The toolbox import command that was run
pub async fn import(thorium: Thorium, conf: CtlConf, cmd: &ImportToolbox) -> Result<(), Error> {
    // get the manifest from the location along with a progress bar
    let (mut manifest, progress) = shared::get_manifest(&cmd.manifest).await?;
    // validate the manifest
    if let Err(err) = manifest.validate() {
        return Err(Error::new(format!(
            "Invalid toolbox manifest: {}",
            err.msg().unwrap_or_else(|| "Unknown error".to_string())
        )));
    }
    // get all the groups the manifest expects to exist, overriding them if we're set to
    let manifest_groups = if let Some(group_override) = &cmd.group_override {
        progress.info_anonymous(format!(
            "Overriding all image/pipeline import groups to '{}'",
            group_override.bright_yellow()
        ));
        manifest = manifest.override_group(group_override);
        if let Err(err) = manifest.validate() {
            return Err(Error::new(format!(
                "Invalid toolbox manifest after group override '{}': {}",
                group_override.bright_yellow(),
                err.msg().unwrap_or_else(|| "Unknown error".to_string())
            )));
        }
        HashSet::from([group_override.to_string()])
    } else {
        manifest.groups()
    };
    // categorize images and pipelines by checking what already exists in Thorium
    let images = categorize::categorize_images(&thorium, &manifest.images, &progress).await?;
    let pipelines =
        categorize::categorize_pipelines(&thorium, &manifest.pipelines, &progress).await?;
    // partition into new vs existing
    let (new_images, existing_images): (Vec<_>, Vec<_>) = images
        .iter()
        .partition(|img| img.existing.is_none());
    let (new_pipelines, existing_pipelines): (Vec<_>, Vec<_>) = pipelines
        .iter()
        .partition(|pipe| pipe.existing.is_none());
    // check which groups are missing
    let missing_groups = shared::get_missing_groups(&thorium, manifest_groups.clone())
        .await
        .map_err(|err| Error::new(format!("Error retrieving missing groups: {err}")))?;
    // confirm with the user that it's okay to import
    if !cmd.skip_confirm {
        let current_user = thorium
            .users
            .info()
            .await
            .map_err(|err| Error::new(format!("Error getting current user info: {err}")))?;
        let confirmed = progress.suspend(|| {
            confirm_import(
                &conf,
                &new_images,
                &existing_images,
                &new_pipelines,
                &existing_pipelines,
                &missing_groups,
                &current_user,
                cmd.force,
            )
        })?;
        if !confirmed {
            return Ok(());
        }
    }
    // create any missing groups
    if !missing_groups.is_empty() {
        progress.refresh(
            "Creating groups",
            BarKind::Bound(missing_groups.len() as u64),
        );
        shared::create_groups(&thorium, missing_groups, &progress).await?;
    }
    // import new resources
    create::import_new_images(&thorium, new_images, &progress).await?;
    create::import_new_pipelines(&thorium, new_pipelines, &progress).await?;
    // handle existing resources
    if cmd.force {
        // force-update all existing resources without the editor
        create::force_update_images(&thorium, existing_images, &progress).await?;
        create::force_update_pipelines(&thorium, existing_pipelines, &progress).await?;
    } else {
        // interactively merge existing resources
        merge::interactive_merge_images(
            &thorium,
            existing_images,
            &conf,
            cmd.editor.as_deref(),
            &progress,
        )
        .await?;
        merge::interactive_merge_pipelines(
            &thorium,
            existing_pipelines,
            &conf,
            cmd.editor.as_deref(),
            &progress,
        )
        .await?;
    }
    progress.refresh("Import complete!", BarKind::Timer);
    progress.finish();
    Ok(())
}
