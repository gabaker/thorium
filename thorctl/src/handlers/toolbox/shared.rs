//! Functionality shared between toolbox handlers

use colored::Colorize;
use futures::StreamExt;
use http::header::CONTENT_LENGTH;
use std::path::Path;
use thorium::Error;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::args::toolbox::ManifestLocation;
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::manifest::{DroppedItems, ToolboxManifest};

/// Warn through the progress bar about every image/pipeline version a validation
/// pass dropped, so skipped resources are visible before anything is applied
///
/// Shared by `toolbox import` (before the confirmation screen) and `toolbox
/// export` (before writing to disk).
///
/// # Arguments
///
/// * `dropped` - The report returned by a `validate_*` pass
/// * `progress` - The progress bar to log warnings through
pub fn warn_dropped(dropped: &DroppedItems, progress: &Bar) {
    // use `warning` (not `info_anonymous`) so dropped resources still reach the
    // user in quiet mode or when output isn't a tty (it falls back to stderr)
    for (name, reason) in &dropped.images {
        progress.warning(format!(
            "Skipping invalid image '{}': {}",
            name.bright_yellow(),
            reason
        ));
    }
    for (name, reasons) in &dropped.pipelines {
        progress.warning(format!(
            "Skipping invalid pipeline '{}': {}",
            name.bright_yellow(),
            reasons.join("; ")
        ));
    }
}

/// Get a [`ToolboxManifest`] from a [`ManifestLocation`]
///
/// # Arguments
///
/// * `location` - The location the manifest is found at
///
/// # Returns
///
/// Returns the [`ToolboxManifest`] along with a [`Bar`] used to track download/reading progress
pub async fn get_manifest(location: &ManifestLocation) -> Result<(ToolboxManifest, Bar), Error> {
    // get the toolbox manifest by URL or file path
    match location {
        ManifestLocation::Url(manifest_url) => {
            // create the progress bar
            let progress = Bar::new("", "Downloading manifest...", BarKind::UnboundIO);
            let manifest = get_manifest_from_url(manifest_url, &progress).await?;
            Ok((manifest, progress))
        }
        ManifestLocation::Path(manifest_path) => {
            // create the progress bar
            let progress = Bar::new("", "Reading manifest file...", BarKind::UnboundIO);
            let manifest = get_manifest_from_path(manifest_path, &progress).await?;
            Ok((manifest, progress))
        }
    }
}

/// Read and parse the manifest file at the given URL
///
/// # Arguments
///
/// * `url` - The manifest URL
/// * `progress` - The progress bar
async fn get_manifest_from_url(url: &Url, progress: &Bar) -> Result<ToolboxManifest, Error> {
    // get the manifest file from the URL
    let resp = reqwest::get(url.clone())
        .await
        .map_err(|err| Error::new(format!("Error downloading toolbox manifest: {err}")))?;
    // check if the response was an error
    match resp.error_for_status() {
        Ok(resp) => {
            // try to get the content length for the progress bar
            if let Some(content_length) = resp.content_length().or(resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|content_length_header| content_length_header.to_str().ok())
                .and_then(|content_length_str| content_length_str.parse::<u64>().ok()))
            {
                progress.refresh("Downloading manifest...", BarKind::IO(content_length));
            } else {
                progress.refresh("Downloading manifest...", BarKind::UnboundIO);
            }
            // get the manifest file as bytes;
            // we use serde_json here instead of reqwest's JSON capabilities for
            // better error logging
            let mut manifest_bytes = Vec::new();
            let mut manifest_bytes_stream = resp.bytes_stream();
            while let Some(bytes) = manifest_bytes_stream.next().await {
                let bytes = bytes.map_err(|err| {
                    Error::new(format!(
                        "Error downloading toolbox manifest response body: {err}"
                    ))
                })?;
                progress.inc(bytes.len() as u64);
                manifest_bytes.extend_from_slice(&bytes);
            }
            // parse the manifest data
            serde_json::from_slice(&manifest_bytes)
                .map_err(|err| Error::new(format!("Malformed toolbox manifest: {err}")))
        }
        Err(err) => Err(Error::new(format!(
            "Error downloading toolbox manifest: {err}"
        ))),
    }
}

/// Read and parse the manifest file at the given path
///
/// # Arguments
///
/// * `path` - The manifest file path
/// * `progress` - The progress bar
async fn get_manifest_from_path(path: &Path, progress: &Bar) -> Result<ToolboxManifest, Error> {
    // open the manifest file at the path
    let mut manifest_file = tokio::fs::File::open(path).await.map_err(|err| {
        Error::new(format!(
            "Error opening manifest file '{}': {}",
            path.display(),
            err
        ))
    })?;
    // try to get the file's length
    match manifest_file
        .metadata()
        .await
        .ok()
        .map(|metadata| metadata.len())
    {
        Some(file_len) => progress.refresh("Reading manifest file...", BarKind::IO(file_len)),
        None => progress.refresh("Reading manifest file...", BarKind::UnboundIO),
    }
    // read the file
    let mut manifest_bytes = Vec::new();
    loop {
        let bytes_read = manifest_file
            .read_buf(&mut manifest_bytes)
            .await
            .map_err(|err| {
                Error::new(format!(
                    "Error reading manifest file '{}': {}",
                    path.display(),
                    err
                ))
            })?;
        if bytes_read == 0 {
            break;
        }
        progress.inc(bytes_read as u64);
    }
    // parse the manifest file
    serde_json::from_slice(&manifest_bytes)
        .map_err(|err| Error::new(format!("Malformed toolbox manifest file: {err}")))
}

/// Fetch a JSON config from a URL and deserialize it
///
/// # Arguments
///
/// * `url` - The URL to fetch the JSON config from
async fn fetch_json_config<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, Error> {
    // fetch the config document from the URL
    let resp = reqwest::get(url)
        .await
        .map_err(|e| Error::new(format!("Failed to fetch config from '{url}': {e}")))?;
    // turn a non-success status into an error
    let resp = resp
        .error_for_status()
        .map_err(|e| Error::new(format!("Failed to fetch config from '{url}': {e}")))?;
    // read the full response body
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::new(format!("Failed to read config response from '{url}': {e}")))?;
    // deserialize the body into the requested type
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::new(format!("Failed to parse config from '{url}': {e}")))
}

/// Resolve any URL-based configs and network policies in the manifest by fetching them
///
/// Only versions that carry a `config_from` URL (and no inline `config`) have their
/// config fetched; likewise every `network_policies_from` URL is fetched and folded
/// into the version's `network_policies`. Versions with neither an inline config nor a
/// `config_from` are left with `config` as `None` — they are not resolved here and are
/// dropped later by structural validation.
///
/// # Arguments
///
/// * `manifest` - The manifest whose URL-based configs/policies are fetched in place
/// * `progress` - The progress bar tracking the fetches
pub async fn resolve_manifest_configs(
    manifest: &mut ToolboxManifest,
    progress: &Bar,
) -> Result<(), Error> {
    use thorium::models::{ImageRequest, NetworkPolicyRequest, PipelineRequest};
    // count how many remote fetches we'll make so the progress bar can be bounded
    let mut url_count = 0u64;
    for image_manifest in manifest.images.values() {
        for version in image_manifest.versions.values() {
            if version.config_from.is_some() && version.config.is_none() {
                url_count += 1;
            }
            url_count += version.network_policies_from.len() as u64;
        }
    }
    for pipeline_manifest in manifest.pipelines.values() {
        for version in pipeline_manifest.versions.values() {
            if version.config_from.is_some() && version.config.is_none() {
                url_count += 1;
            }
        }
    }
    // nothing to fetch, so return without touching the progress bar
    if url_count == 0 {
        return Ok(());
    }
    // switch the bar to a bounded mode now that we know the total
    progress.refresh("Fetching remote configs", BarKind::Bound(url_count));
    // fetch each image version's URL-based config and network policies in place
    for image_manifest in manifest.images.values_mut() {
        for version in image_manifest.versions.values_mut() {
            // fetch the config only when it's URL-sourced and not already inline
            if let Some(url) = &version.config_from
                && version.config.is_none()
            {
                let config: ImageRequest = fetch_json_config(url).await?;
                version.config = Some(config);
                progress.inc(1);
            }
            // fetch any URL-based network policy definitions alongside configs
            for url in version.network_policies_from.drain(..) {
                let policy: NetworkPolicyRequest = fetch_json_config(&url).await?;
                version.network_policies.push(policy);
                progress.inc(1);
            }
        }
    }
    // fetch each pipeline version's URL-based config in place
    for pipeline_manifest in manifest.pipelines.values_mut() {
        for version in pipeline_manifest.versions.values_mut() {
            // fetch the config only when it's URL-sourced and not already inline
            if let Some(url) = &version.config_from
                && version.config.is_none()
            {
                let config: PipelineRequest = fetch_json_config(url).await?;
                version.config = Some(config);
                progress.inc(1);
            }
        }
    }
    Ok(())
}
