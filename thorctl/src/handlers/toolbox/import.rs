//! Handlers for importing a toolbox to Thorium
//!
//! When images or pipelines already exist in Thorium, the import will detect
//! differences and either open an interactive merge editor (default) or
//! automatically apply all incoming changes (`--force`).

use colored::Colorize;
use futures::{StreamExt, TryStreamExt, stream};
use http::StatusCode;
use std::collections::{HashMap, HashSet};
use thorium::models::{Image, ImageRequest, Pipeline, PipelineRequest, ScrubbedUser};
use thorium::{CtlConf, Error, Thorium};

use super::manifest::{ImageManifest, PipelineManifest};
use super::merge::{self, MergeAction};
use crate::args::toolbox::ImportToolbox;
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::shared;

// ─── Resource Categorization ─────────────────────────────────────────────────

/// An image from the manifest categorized by whether it already exists in Thorium
struct CategorizedImage {
    /// The manifest name for this image
    image_name: String,
    /// The manifest version name for this image
    version_name: String,
    /// The image request from the manifest
    request: ImageRequest,
    /// The existing image in Thorium, if any
    existing: Option<Image>,
}

/// A pipeline from the manifest categorized by whether it already exists in Thorium
struct CategorizedPipeline {
    /// The manifest name for this pipeline
    pipeline_name: String,
    /// The manifest version name for this pipeline
    version_name: String,
    /// The pipeline request from the manifest
    request: PipelineRequest,
    /// The existing pipeline in Thorium, if any
    existing: Option<Pipeline>,
}

/// Categorize all images in the manifest by checking which ones already exist
/// in Thorium
async fn categorize_images(
    thorium: &Thorium,
    images: &HashMap<String, ImageManifest>,
    progress: &Bar,
) -> Result<Vec<CategorizedImage>, Error> {
    let items: Vec<_> = images
        .iter()
        .flat_map(|(image_name, manifest)| {
            manifest
                .versions
                .iter()
                .map(move |(version_name, image)| {
                    (image_name.clone(), version_name.clone(), image.config.clone())
                })
        })
        .collect();
    progress.refresh(
        "Checking existing images",
        BarKind::Bound(items.len() as u64),
    );
    stream::iter(items)
        .map(|(image_name, version_name, request)| {
            let thorium = thorium;
            async move {
                let existing = match thorium
                    .images
                    .get(&request.group, &request.name)
                    .await
                {
                    Ok(image) => Some(image),
                    Err(err)
                        if err
                            .status()
                            .is_some_and(|status| status == StatusCode::NOT_FOUND) =>
                    {
                        None
                    }
                    Err(err) => {
                        return Err(Error::new(format!(
                            "Error checking image '{image_name}:{version_name}': {err}"
                        )));
                    }
                };
                progress.inc(1);
                Ok(CategorizedImage {
                    image_name,
                    version_name,
                    request,
                    existing,
                })
            }
        })
        .buffer_unordered(10)
        .collect::<Vec<Result<_, _>>>()
        .await
        .into_iter()
        .collect()
}

/// Categorize all pipelines in the manifest by checking which ones already exist
/// in Thorium
async fn categorize_pipelines(
    thorium: &Thorium,
    pipelines: &HashMap<String, PipelineManifest>,
    progress: &Bar,
) -> Result<Vec<CategorizedPipeline>, Error> {
    let items: Vec<_> = pipelines
        .iter()
        .flat_map(|(pipeline_name, manifest)| {
            manifest
                .versions
                .iter()
                .map(move |(version_name, pipeline)| {
                    (
                        pipeline_name.clone(),
                        version_name.clone(),
                        pipeline.config.clone(),
                    )
                })
        })
        .collect();
    progress.refresh(
        "Checking existing pipelines",
        BarKind::Bound(items.len() as u64),
    );
    stream::iter(items)
        .map(|(pipeline_name, version_name, request)| {
            let thorium = thorium;
            async move {
                let existing = match thorium
                    .pipelines
                    .get(&request.group, &request.name)
                    .await
                {
                    Ok(pipeline) => Some(pipeline),
                    Err(err)
                        if err
                            .status()
                            .is_some_and(|status| status == StatusCode::NOT_FOUND) =>
                    {
                        None
                    }
                    Err(err) => {
                        return Err(Error::new(format!(
                            "Error checking pipeline '{pipeline_name}:{version_name}': {err}"
                        )));
                    }
                };
                progress.inc(1);
                Ok(CategorizedPipeline {
                    pipeline_name,
                    version_name,
                    request,
                    existing,
                })
            }
        })
        .buffer_unordered(10)
        .collect::<Vec<Result<_, _>>>()
        .await
        .into_iter()
        .collect()
}

// ─── New Resource Import ─────────────────────────────────────────────────────

/// Import new images (ones that don't already exist in Thorium)
async fn import_new_images(
    thorium: &Thorium,
    new_images: Vec<&CategorizedImage>,
    progress: &Bar,
) -> Result<(), Error> {
    if new_images.is_empty() {
        return Ok(());
    }
    progress.refresh(
        "Importing new images",
        BarKind::Bound(new_images.len() as u64),
    );
    stream::iter(new_images)
        .map(Ok::<_, Error>)
        .try_for_each_concurrent(10, |img| async move {
            thorium.images.create(&img.request).await.map_err(|err| {
                Error::new(format!(
                    "Error importing image '{}:{}': {}",
                    img.image_name, img.version_name, err
                ))
            })?;
            progress.inc(1);
            Ok(())
        })
        .await
}

/// Import new pipelines (ones that don't already exist in Thorium)
async fn import_new_pipelines(
    thorium: &Thorium,
    new_pipelines: Vec<&CategorizedPipeline>,
    progress: &Bar,
) -> Result<(), Error> {
    if new_pipelines.is_empty() {
        return Ok(());
    }
    progress.refresh(
        "Importing new pipelines",
        BarKind::Bound(new_pipelines.len() as u64),
    );
    stream::iter(new_pipelines)
        .map(Ok::<_, Error>)
        .try_for_each_concurrent(10, |pipe| async move {
            thorium
                .pipelines
                .create(&pipe.request)
                .await
                .map_err(|err| {
                    Error::new(format!(
                        "Error importing pipeline '{}:{}': {}",
                        pipe.pipeline_name, pipe.version_name, err
                    ))
                })?;
            progress.inc(1);
            Ok(())
        })
        .await
}

// ─── Force Update (--force) ──────────────────────────────────────────────────

/// Force-update all existing images without the editor
async fn force_update_images(
    thorium: &Thorium,
    existing_images: Vec<&CategorizedImage>,
    progress: &Bar,
) -> Result<(), Error> {
    let updates: Vec<_> = existing_images
        .into_iter()
        .filter_map(|img| {
            let existing = img.existing.as_ref()?;
            let update = merge::calculate_image_update(existing.clone(), img.request.clone())?;
            Some((img, update))
        })
        .collect();
    if updates.is_empty() {
        return Ok(());
    }
    progress.refresh(
        "Force-updating images",
        BarKind::Bound(updates.len() as u64),
    );
    stream::iter(updates)
        .map(Ok::<_, Error>)
        .try_for_each_concurrent(10, |(img, update)| async move {
            thorium
                .images
                .update(&img.request.group, &img.request.name, &update)
                .await
                .map_err(|err| {
                    Error::new(format!(
                        "Error force-updating image '{}:{}': {}",
                        img.image_name, img.version_name, err
                    ))
                })?;
            progress.inc(1);
            Ok(())
        })
        .await
}

/// Force-update all existing pipelines without the editor
async fn force_update_pipelines(
    thorium: &Thorium,
    existing_pipelines: Vec<&CategorizedPipeline>,
    progress: &Bar,
) -> Result<(), Error> {
    let updates: Vec<_> = existing_pipelines
        .into_iter()
        .filter_map(|pipe| {
            let existing = pipe.existing.as_ref()?;
            let update =
                merge::calculate_pipeline_update(existing.clone(), pipe.request.clone())?;
            Some((pipe, update))
        })
        .collect();
    if updates.is_empty() {
        return Ok(());
    }
    progress.refresh(
        "Force-updating pipelines",
        BarKind::Bound(updates.len() as u64),
    );
    stream::iter(updates)
        .map(Ok::<_, Error>)
        .try_for_each_concurrent(10, |(pipe, update)| async move {
            thorium
                .pipelines
                .update(&pipe.request.group, &pipe.request.name, &update)
                .await
                .map_err(|err| {
                    Error::new(format!(
                        "Error force-updating pipeline '{}:{}': {}",
                        pipe.pipeline_name, pipe.version_name, err
                    ))
                })?;
            progress.inc(1);
            Ok(())
        })
        .await
}

// ─── Interactive Merge ───────────────────────────────────────────────────────

/// Interactively handle existing images that have changes, prompting the user
/// for each one
async fn interactive_merge_images(
    thorium: &Thorium,
    existing_images: Vec<&CategorizedImage>,
    conf: &CtlConf,
    editor_override: Option<&str>,
    progress: &Bar,
) -> Result<(), Error> {
    // filter to only images with actual changes
    let changed_images: Vec<_> = existing_images
        .into_iter()
        .filter(|img| {
            img.existing
                .as_ref()
                .is_some_and(|existing| existing != &img.request)
        })
        .collect();
    if changed_images.is_empty() {
        return Ok(());
    }
    for img in changed_images {
        let existing = img.existing.as_ref().unwrap();
        // suspend the progress bar for interactive prompts
        let action = progress.suspend(|| {
            merge::prompt_merge_action("Image", &img.request.group, &img.request.name)
        })?;
        match action {
            MergeAction::Edit => {
                // open the editor for this image
                let update = progress.suspend(|| {
                    merge::merge_image_interactive(
                        existing,
                        &img.request,
                        conf,
                        editor_override,
                    )
                })?;
                if let Some(update) = update {
                    thorium
                        .images
                        .update(&img.request.group, &img.request.name, &update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating image '{}:{}': {}",
                                img.image_name, img.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Image".bright_green(),
                        format!("'{}:{}'", img.request.group, img.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                } else {
                    println!(
                        "{} No changes detected for image '{}:{}'",
                        "Skipped:".bright_blue(),
                        img.request.group,
                        img.request.name
                    );
                }
            }
            MergeAction::Skip => {
                progress.info_anonymous(format!(
                    "Skipping image '{}:{}'",
                    img.image_name.bright_yellow(),
                    img.version_name.bright_yellow()
                ));
            }
            MergeAction::Apply => {
                if let Some(update) =
                    merge::calculate_image_update(existing.clone(), img.request.clone())
                {
                    thorium
                        .images
                        .update(&img.request.group, &img.request.name, &update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating image '{}:{}': {}",
                                img.image_name, img.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Image".bright_green(),
                        format!("'{}:{}'", img.request.group, img.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                }
            }
            MergeAction::Quit => {
                println!("Stopping further resource processing.");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Interactively handle existing pipelines that have changes, prompting the user
/// for each one
async fn interactive_merge_pipelines(
    thorium: &Thorium,
    existing_pipelines: Vec<&CategorizedPipeline>,
    conf: &CtlConf,
    editor_override: Option<&str>,
    progress: &Bar,
) -> Result<(), Error> {
    // filter to only pipelines with actual changes
    let changed_pipelines: Vec<_> = existing_pipelines
        .into_iter()
        .filter(|pipe| {
            pipe.existing
                .as_ref()
                .is_some_and(|existing| existing != &pipe.request)
        })
        .collect();
    if changed_pipelines.is_empty() {
        return Ok(());
    }
    for pipe in changed_pipelines {
        let existing = pipe.existing.as_ref().unwrap();
        let action = progress.suspend(|| {
            merge::prompt_merge_action("Pipeline", &pipe.request.group, &pipe.request.name)
        })?;
        match action {
            MergeAction::Edit => {
                let update = progress.suspend(|| {
                    merge::merge_pipeline_interactive(
                        existing,
                        &pipe.request,
                        conf,
                        editor_override,
                    )
                })?;
                if let Some(update) = update {
                    thorium
                        .pipelines
                        .update(&pipe.request.group, &pipe.request.name, &update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating pipeline '{}:{}': {}",
                                pipe.pipeline_name, pipe.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Pipeline".bright_green(),
                        format!("'{}:{}'", pipe.request.group, pipe.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                } else {
                    println!(
                        "{} No changes detected for pipeline '{}:{}'",
                        "Skipped:".bright_blue(),
                        pipe.request.group,
                        pipe.request.name
                    );
                }
            }
            MergeAction::Skip => {
                progress.info_anonymous(format!(
                    "Skipping pipeline '{}:{}'",
                    pipe.pipeline_name.bright_yellow(),
                    pipe.version_name.bright_yellow()
                ));
            }
            MergeAction::Apply => {
                if let Some(update) =
                    merge::calculate_pipeline_update(existing.clone(), pipe.request.clone())
                {
                    thorium
                        .pipelines
                        .update(&pipe.request.group, &pipe.request.name, &update)
                        .await
                        .map_err(|err| {
                            Error::new(format!(
                                "Error updating pipeline '{}:{}': {}",
                                pipe.pipeline_name, pipe.version_name, err
                            ))
                        })?;
                    println!(
                        "{} {} {}",
                        "Pipeline".bright_green(),
                        format!("'{}:{}'", pipe.request.group, pipe.request.name).yellow(),
                        "updated successfully!".bright_green()
                    );
                }
            }
            MergeAction::Quit => {
                println!("Stopping further resource processing.");
                return Ok(());
            }
        }
    }
    Ok(())
}

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
    let images = categorize_images(&thorium, &manifest.images, &progress).await?;
    let pipelines = categorize_pipelines(&thorium, &manifest.pipelines, &progress).await?;
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
    // import new images
    import_new_images(&thorium, new_images, &progress).await?;
    // import new pipelines
    import_new_pipelines(&thorium, new_pipelines, &progress).await?;
    // handle existing resources
    if cmd.force {
        // force-update all existing resources without the editor
        force_update_images(&thorium, existing_images, &progress).await?;
        force_update_pipelines(&thorium, existing_pipelines, &progress).await?;
    } else {
        // interactively merge existing resources
        interactive_merge_images(
            &thorium,
            existing_images,
            &conf,
            cmd.editor.as_deref(),
            &progress,
        )
        .await?;
        interactive_merge_pipelines(
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
