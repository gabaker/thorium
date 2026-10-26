//! Functionality shared between toolbox handlers

use futures::StreamExt;
use http::header::CONTENT_LENGTH;
use std::path::Path;
use thorium::Error;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::manifest::ToolboxManifest;
use crate::utils;

/// Download the manifest at the given URL
///
/// # Arguments
///
/// * `url` - The manifest URL
/// * `progress` - The progress bar
pub async fn download_manifest_bytes(url: &Url, progress: &Bar) -> Result<Vec<u8>, Error> {
    // issue the GET; a transport-level failure (DNS, connection) surfaces here before
    // any status code is known
    let resp = reqwest::get(url.clone())
        .await
        .map_err(|err| Error::new(format!("Failed to download toolbox manifest: {err}")))?;
    // turn a non-2xx status (404, 500, ...) into an error so we don't try to parse an
    // error page body as a manifest
    match resp.error_for_status() {
        Ok(resp) => {
            // bound the bar when the size is known: prefer reqwest's parsed length,
            // falling back to parsing the raw Content-Length header ourselves, since
            // some responses (e.g. chunked) leave `content_length()` as None
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
            // stream the body chunk by chunk into a buffer, advancing the bar as bytes
            // arrive; the bytes are collected (rather than handed to reqwest's `.json()`)
            // so the caller can run serde_json itself and emit a more descriptive parse error
            let mut manifest_bytes = Vec::new();
            let mut manifest_bytes_stream = resp.bytes_stream();
            while let Some(bytes) = manifest_bytes_stream.next().await {
                let bytes = bytes.map_err(|err| {
                    Error::new(format!(
                        "Failed to download toolbox manifest response body: {err}"
                    ))
                })?;
                progress.inc(bytes.len() as u64);
                manifest_bytes.extend_from_slice(&bytes);
            }
            // hand back the fully buffered body for the caller to parse
            Ok(manifest_bytes)
        }
        Err(err) => Err(Error::new(format!(
            "Failed to download toolbox manifest: {err}"
        ))),
    }
}

/// Read the manifest file at the given path
///
/// # Arguments
///
/// * `path` - The manifest file path
/// * `progress` - The progress bar
pub async fn read_manifest_bytes(path: &Path, progress: &Bar) -> Result<Vec<u8>, Error> {
    // a directory (e.g. a toolbox repo checkout) isn't a manifest; point the user at the
    // toolbox.json inside it, or at `toolbox build` to generate one, instead of a raw IO error
    if path.is_dir() {
        let candidate = path.join("toolbox.json");
        let hint = if candidate.is_file() {
            format!("pass the manifest file instead: '{}'", candidate.display())
        } else {
            "run `thorctl toolbox build` to generate a toolbox.json and pass that file".to_string()
        };
        return Err(Error::new(format!(
            "Failed to read toolbox manifest '{}': it is a directory; {hint}",
            path.display()
        )));
    }
    // open the file up front so a missing/unreadable path fails before the bar advances
    let mut manifest_file = tokio::fs::File::open(path).await.map_err(|err| {
        Error::new(format!(
            "Failed to open manifest file '{}': {}",
            path.display(),
            err
        ))
    })?;
    // bound the bar with the file size when stat succeeds; a stat failure is non-fatal
    // (`.ok()` swallows it) and simply leaves the bar unbounded rather than aborting
    match manifest_file
        .metadata()
        .await
        .ok()
        .map(|metadata| metadata.len())
    {
        Some(file_len) => progress.refresh("Reading manifest file...", BarKind::IO(file_len)),
        None => progress.refresh("Reading manifest file...", BarKind::UnboundIO),
    }
    // read the whole file into a buffer, advancing the bar per read; a `read_buf` of 0
    // bytes signals EOF and ends the loop
    let mut manifest_bytes = Vec::new();
    loop {
        let bytes_read = manifest_file
            .read_buf(&mut manifest_bytes)
            .await
            .map_err(|err| {
                Error::new(format!(
                    "Failed to read manifest file '{}': {}",
                    path.display(),
                    err
                ))
            })?;
        if bytes_read == 0 {
            break;
        }
        progress.inc(bytes_read as u64);
    }
    // hand back the fully buffered file for the caller to parse
    Ok(manifest_bytes)
}

/// Fetch a JSON config from a URL and deserialize it
///
/// # Arguments
///
/// * `url` - The URL to fetch the JSON config from
pub async fn fetch_json_config<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, Error> {
    // issue the GET; transport-level failures surface here before any status is known
    let resp = reqwest::get(url)
        .await
        .map_err(|e| Error::new(format!("Failed to fetch config from '{url}': {e}")))?;
    // reject a non-2xx status so an error page body is never parsed as a config
    let resp = resp
        .error_for_status()
        .map_err(|e| Error::new(format!("Failed to fetch config from '{url}': {e}")))?;
    // buffer the whole body; configs are small, so there's no streaming/progress here
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::new(format!("Failed to read config response from '{url}': {e}")))?;
    // deserialize into the caller-chosen request type (ImageRequest / PipelineRequest /
    // NetworkPolicyRequest), keeping the source url in the error for diagnosability
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::new(format!("Failed to parse config from '{url}': {e}")))
}

/// Fill in a fetched config's description from the one its entry carries
///
/// A `description.md` can't be written into a config fetched from a URL at build time, so the
/// entry carries it and it is applied once the config is fetched. A description the fetched
/// config already has is kept.
///
/// # Arguments
///
/// * `description` - The fetched config's description
/// * `carried` - The description the entry carries, which may be empty
pub(super) fn fill_description(description: &mut Option<String>, carried: &str) {
    // only a config with no description (missing, null, or empty) takes the carried one
    if description.as_deref().is_none_or(str::is_empty) && !carried.is_empty() {
        *description = Some(carried.to_owned());
    }
}

/// Resolve any URL-based configs and network policies in the manifest by fetching them
///
/// Only versions that carry a `config_from` URL (and no inline `config`) have their
/// config fetched; likewise every `network_policies_from` URL is fetched and folded
/// into the version's `network_policies`. Versions with neither an inline config nor a
/// `config_from` are left with `config` as `None` — they are not resolved here and are
/// dropped later by structural validation.
///
/// Every URL is attempted even after a failure, and all failures are then reported
/// together in one error naming each resource and URL. Callers run this before making
/// any change to Thorium, so a failed fetch aborts the command with nothing applied.
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
    // pre-count every remote fetch so the bar can be bounded; this counting pass must
    // mirror the fetching passes below exactly, or the bar total will drift from reality
    let mut url_count = 0u64;
    for image_manifest in manifest.images.values() {
        for version in image_manifest.versions.values() {
            // an image config is fetched only when sourced from a URL and not already
            // inline; an inline `config` takes precedence and skips the fetch
            if version.config_from.is_some() && version.config.is_none() {
                url_count += 1;
            }
            // every `network_policies_from` entry is a URL that will be fetched, so each
            // counts toward the total regardless of inline config
            url_count += version.network_policies_from.len() as u64;
        }
    }
    for pipeline_manifest in manifest.pipelines.values() {
        for version in pipeline_manifest.versions.values() {
            // pipelines have no network policies, so only the URL-sourced config counts
            if version.config_from.is_some() && version.config.is_none() {
                url_count += 1;
            }
        }
    }
    // short-circuit when there's nothing remote to fetch, leaving the bar in whatever
    // (unbounded) state the caller set so we don't flash an empty bounded bar
    if url_count == 0 {
        return Ok(());
    }
    // now that the exact total is known, switch the bar to bounded so it shows real progress
    progress.refresh("Fetching remote configs", BarKind::Bound(url_count));
    // every fetch failure, labelled with the resource it belongs to, reported together below
    let mut failures: Vec<String> = Vec::new();
    // resolve each image version's URL-sourced config and network policies in place
    for (image_name, image_manifest) in &mut manifest.images {
        for (version_name, version) in &mut image_manifest.versions {
            // fetch the config only when it's URL-sourced and not already inline, matching
            // the counting pass; embed the result so downstream validation sees a config
            if let Some(url) = &version.config_from
                && version.config.is_none()
            {
                match fetch_json_config::<ImageRequest>(url).await {
                    Ok(mut config) => {
                        // the description.md the entry carries fills in a missing description
                        if let Some(carried) = &version.description {
                            fill_description(&mut config.description, carried);
                        }
                        version.config = Some(config);
                    }
                    Err(err) => failures.push(format!(
                        "image '{}' config_from: {err}",
                        utils::entry_id(None, image_name, version_name)
                    )),
                }
                progress.inc(1);
            }
            // drain each policy URL into the resolved `network_policies` list; draining
            // (rather than iterating) moves the urls out so they aren't re-fetched and the
            // resolved manifest no longer carries unresolved `network_policies_from` urls
            for url in version.network_policies_from.drain(..) {
                match fetch_json_config::<NetworkPolicyRequest>(&url).await {
                    Ok(policy) => version.network_policies.push(policy),
                    Err(err) => failures.push(format!(
                        "image '{}' network_policies_from: {err}",
                        utils::entry_id(None, image_name, version_name)
                    )),
                }
                progress.inc(1);
            }
        }
    }
    // resolve each pipeline version's URL-sourced config in place (pipelines carry no policies)
    for (pipeline_name, pipeline_manifest) in &mut manifest.pipelines {
        for (version_name, version) in &mut pipeline_manifest.versions {
            // fetch the config only when it's URL-sourced and not already inline
            if let Some(url) = &version.config_from
                && version.config.is_none()
            {
                match fetch_json_config::<PipelineRequest>(url).await {
                    Ok(mut config) => {
                        // the description the entry carries fills in a missing description
                        fill_description(&mut config.description, &version.description);
                        version.config = Some(config);
                    }
                    Err(err) => failures.push(format!(
                        "pipeline '{}' config_from: {err}",
                        utils::entry_id(None, pipeline_name, version_name)
                    )),
                }
                progress.inc(1);
            }
        }
    }
    // abort with every failed fetch listed, in a stable order, so all unreachable urls
    // can be fixed at once instead of one per run
    if !failures.is_empty() {
        failures.sort();
        return Err(Error::new(format!(
            "Failed to fetch {} remote config(s); nothing was changed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fill_description;

    /// A carried description fills a missing, null, or empty description and never replaces one
    #[test]
    fn fill_description_only_fills_a_missing_description() {
        // a description the config has stays
        let mut own = Some("own".to_owned());
        fill_description(&mut own, "carried");
        assert_eq!(own.as_deref(), Some("own"));
        // none and empty take the carried one
        for start in [None, Some(String::new())] {
            let mut description = start;
            fill_description(&mut description, "carried");
            assert_eq!(description.as_deref(), Some("carried"));
        }
        // nothing carried leaves the description as it is
        let mut none = None;
        fill_description(&mut none, "");
        assert_eq!(none, None);
        let mut empty = Some(String::new());
        fill_description(&mut empty, "");
        assert_eq!(empty.as_deref(), Some(""));
    }
}
