//! Resource categorization for toolbox imports
//!
//! Checks which images and pipelines from a toolbox manifest already exist
//! in Thorium, categorizing them for downstream handling (create vs update).

use futures::{StreamExt, stream};
use http::StatusCode;
use std::collections::HashMap;
use thorium::models::{Image, ImageRequest, Pipeline, PipelineRequest};
use thorium::{Error, Thorium};

use super::manifest::{ImageManifest, PipelineManifest};
use crate::handlers::progress::{Bar, BarKind};

/// An image from the manifest categorized by whether it already exists in Thorium
pub struct CategorizedImage {
    /// The manifest name for this image
    pub image_name: String,
    /// The manifest version name for this image
    pub version_name: String,
    /// The image request from the manifest
    pub request: ImageRequest,
    /// The existing image in Thorium, if any
    pub existing: Option<Image>,
}

/// A pipeline from the manifest categorized by whether it already exists in Thorium
pub struct CategorizedPipeline {
    /// The manifest name for this pipeline
    pub pipeline_name: String,
    /// The manifest version name for this pipeline
    pub version_name: String,
    /// The pipeline request from the manifest
    pub request: PipelineRequest,
    /// The existing pipeline in Thorium, if any
    pub existing: Option<Pipeline>,
}

/// Categorize all images in the manifest by checking which ones already exist
/// in Thorium
pub async fn categorize_images(
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
pub async fn categorize_pipelines(
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
