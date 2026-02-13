//! Resource creation and force-update operations
//!
//! Handles importing new images/pipelines and force-updating existing ones
//! without interactive merge resolution.

use futures::{StreamExt, TryStreamExt, stream};
use thorium::{Error, Thorium};

use super::categorize::{CategorizedImage, CategorizedPipeline};
use super::update;
use crate::handlers::progress::{Bar, BarKind};

/// Import new images (ones that don't already exist in Thorium)
pub async fn import_new_images(
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
pub async fn import_new_pipelines(
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

/// Force-update all existing images without the editor
pub async fn force_update_images(
    thorium: &Thorium,
    existing_images: Vec<&CategorizedImage>,
    progress: &Bar,
) -> Result<(), Error> {
    let updates: Vec<_> = existing_images
        .into_iter()
        .filter_map(|img| {
            let existing = img.existing.as_ref()?;
            let update_req = update::calculate_image_update(existing.clone(), img.request.clone())?;
            Some((img, update_req))
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
        .try_for_each_concurrent(10, |(img, update_req)| async move {
            thorium
                .images
                .update(&img.request.group, &img.request.name, &update_req)
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
pub async fn force_update_pipelines(
    thorium: &Thorium,
    existing_pipelines: Vec<&CategorizedPipeline>,
    progress: &Bar,
) -> Result<(), Error> {
    let updates: Vec<_> = existing_pipelines
        .into_iter()
        .filter_map(|pipe| {
            let existing = pipe.existing.as_ref()?;
            let update_req =
                update::calculate_pipeline_update(existing.clone(), pipe.request.clone())?;
            Some((pipe, update_req))
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
        .try_for_each_concurrent(10, |(pipe, update_req)| async move {
            thorium
                .pipelines
                .update(&pipe.request.group, &pipe.request.name, &update_req)
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
