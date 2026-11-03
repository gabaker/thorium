//! Functionality shared between toolbox handlers

use futures::StreamExt;
use http::header::CONTENT_LENGTH;
use std::path::Path;
use thorium::Error;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::handlers::progress::{self, Bar, BarKind};
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

/// Apply the description an entry carries to the config fetched for it
///
/// A `description.md` can't be written into a config fetched from a URL at build time, so the
/// entry carries it and it is applied once the config is fetched. Like `description.md` with an
/// inline config, the carried description wins over the fetched config's own, with a warning when
/// it replaces a differing non-empty one. An entry that carries nothing leaves the fetched
/// description as it is.
///
/// Returns whether a differing non-empty description was replaced (and warned about).
///
/// # Arguments
///
/// * `name` - The entry the config belongs to, for the warning
/// * `description` - The fetched config's description
/// * `carried` - The description the entry carries, which may be empty
pub(super) fn apply_carried_description(
    name: &str,
    description: &mut Option<String>,
    carried: &str,
) -> bool {
    // an entry that carries no description leaves the fetched one alone
    if carried.is_empty() {
        return false;
    }
    // warn when replacing a differing description so the conflict is visible
    let replaced = description
        .as_deref()
        .is_some_and(|own| !own.is_empty() && own != carried);
    if replaced {
        progress::warn(format!(
            "{name}: the description its toolbox entry carries overrides the differing \
             description in its fetched config"
        ));
    }
    *description = Some(carried.to_owned());
    replaced
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
                        // the description.md the entry carries replaces the fetched description
                        if let Some(carried) = &version.description {
                            let id = utils::entry_id(None, image_name, version_name);
                            apply_carried_description(&id, &mut config.description, carried);
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
                        // the description the entry carries replaces the fetched description
                        let id = utils::entry_id(None, pipeline_name, version_name);
                        apply_carried_description(
                            &id,
                            &mut config.description,
                            &version.description,
                        );
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
    use super::{apply_carried_description, resolve_manifest_configs};
    use crate::handlers::progress::{Bar, BarKind};
    use crate::handlers::toolbox::manifest::ToolboxManifest;
    use thorium::models::{ImageRequest, PipelineRequest};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one image config and one pipeline config over HTTP, each described as "fetched"
    ///
    /// Returns the base URL the configs are served under (`/image.json` and `/pipeline.json`).
    async fn serve_described_configs() -> String {
        // the two configs, each with a description of its own
        let mut image = ImageRequest::new("g", "scan");
        image.description = Some("fetched".to_owned());
        let mut pipeline = PipelineRequest::new("g", "scan", serde_json::json!(["scan"]));
        pipeline.description = Some("fetched".to_owned());
        let image = serde_json::to_string(&image).unwrap();
        let pipeline = serde_json::to_string(&pipeline).unwrap();
        // answer every request with the config its path names
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // serve requests until the test ends
            while let Ok((mut stream, _)) = listener.accept().await {
                // read the request line to learn which config was asked for
                let mut buf = vec![0u8; 4096];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]);
                let body = if request.starts_with("GET /image.json") {
                    &image
                } else {
                    &pipeline
                };
                // reply with that config and close the connection
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    /// A description.md carried by an entry whose config is fetched from a URL replaces the
    /// fetched config's own description, for images and pipelines
    #[tokio::test]
    async fn description_md_overrides_a_fetched_config_description() {
        // a toolbox whose image and pipeline configs are URLs and whose entries carry description.md
        let base = serve_described_configs().await;
        let mut manifest: ToolboxManifest = serde_json::from_value(serde_json::json!({
            "name": "test",
            "images": {"scan": {"1": {
                "config_from": format!("{base}/image.json"), "description": "from md"
            }}},
            "pipelines": {"scan": {"1": {
                "config_from": format!("{base}/pipeline.json"), "description": "from md"
            }}},
        }))
        .unwrap();
        // fetch the configs
        let bar = Bar::new_or_quiet("test", "test", BarKind::UnboundIO, true);
        resolve_manifest_configs(&mut manifest, &bar).await.unwrap();
        // both fetched configs take the carried description.md
        let image = manifest.images["scan"].versions["1"]
            .config
            .as_ref()
            .unwrap();
        assert_eq!(image.description.as_deref(), Some("from md"));
        let pipeline = manifest.pipelines["scan"].versions["1"]
            .config
            .as_ref()
            .unwrap();
        assert_eq!(pipeline.description.as_deref(), Some("from md"));
    }

    /// A carried description fills a missing, null, or empty description, replaces a differing one
    /// with a warning, and leaves the description alone when nothing is carried
    #[test]
    fn carried_description_overrides_the_fetched_description() {
        // a differing description the fetched config has is replaced and warned about
        let mut own = Some("own".to_owned());
        assert!(apply_carried_description("tool", &mut own, "carried"));
        assert_eq!(own.as_deref(), Some("carried"));
        // a matching description is kept without a warning
        let mut same = Some("carried".to_owned());
        assert!(!apply_carried_description("tool", &mut same, "carried"));
        assert_eq!(same.as_deref(), Some("carried"));
        // none and empty take the carried one without a warning
        for start in [None, Some(String::new())] {
            let mut description = start;
            assert!(!apply_carried_description(
                "tool",
                &mut description,
                "carried"
            ));
            assert_eq!(description.as_deref(), Some("carried"));
        }
        // nothing carried leaves the description as it is
        let mut kept = Some("own".to_owned());
        assert!(!apply_carried_description("tool", &mut kept, ""));
        assert_eq!(kept.as_deref(), Some("own"));
        let mut none = None;
        assert!(!apply_carried_description("tool", &mut none, ""));
        assert_eq!(none, None);
        let mut empty = Some(String::new());
        assert!(!apply_carried_description("tool", &mut empty, ""));
        assert_eq!(empty.as_deref(), Some(""));
    }
}
