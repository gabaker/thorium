//! Tests for converting toolboxes to the current format

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thorium::models::{Image, ImageRequest, Resources};

use super::legacy::Converted;
use super::{ConvertOptions, convert_document};
use crate::handlers::progress::{Bar, BarKind};

mod acceptance;
mod command;
mod mapping;

/// The real toolbox this repository ships, in the legacy format
pub(super) const REAL_TOOLBOX: &str = include_str!("../../../../../../tools/toolbox.json");

/// The real toolbox as converted, committed beside it
pub(super) const REAL_TOOLBOX_V2: &str = include_str!("../../../../../../tools/toolbox.v2.json");

/// A small legacy toolbox with an image that can't convert and a pipeline that depends on it
pub(super) const SMALL_LEGACY: &str =
    include_str!("../../../../../tests/data/toolbox/small.legacy.json");

/// The directory the golden files live in
pub(super) fn data_dir() -> PathBuf {
    // the goldens sit with the other test data of this crate
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/toolbox")
}

/// Compare text with a golden file, or rewrite the file when `THORCTL_UPDATE_GOLDEN` is set
///
/// # Arguments
///
/// * `name` - The golden file's name
/// * `actual` - The text to compare
pub(super) fn assert_golden(name: &str, actual: &str) {
    let path = data_dir().join(name);
    // an update run records the output instead of checking it
    if std::env::var_os("THORCTL_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    // otherwise the output must match the recorded file exactly
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read golden {}: {err}", path.display()));
    assert_eq!(actual, expected, "golden {name} differs");
}

/// A scratch directory that is removed when it is dropped
pub(super) struct Scratch {
    /// The directory
    pub path: PathBuf,
}

impl Scratch {
    /// Create an empty scratch directory with a unique name
    pub fn new() -> Self {
        // a random name keeps parallel tests apart
        let path = std::env::temp_dir().join(format!("thorctl-convert-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch { path }
    }

    /// Get the path of a file in the directory
    ///
    /// # Arguments
    ///
    /// * `name` - The file's name
    pub fn file(&self, name: &str) -> PathBuf {
        // files sit directly in the directory
        self.path.join(name)
    }

    /// Write a file into the directory and return its path
    ///
    /// # Arguments
    ///
    /// * `name` - The file's name
    /// * `contents` - What to write
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        // write the file and hand back where it is
        let path = self.file(name);
        std::fs::write(&path, contents).expect("write scratch file");
        path
    }
}

impl Drop for Scratch {
    /// Remove the directory and everything in it
    fn drop(&mut self) {
        // a leftover temp directory is harmless, so ignore failures
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Build a progress bar that draws nothing
pub(super) fn quiet() -> Bar {
    // a quiet bar has nothing to draw, so tests stay silent
    Bar::new_or_quiet("test", "test", BarKind::UnboundIO, true)
}

/// Build the legacy config of an image
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
/// * `image` - The container image to run, if any
pub(super) fn request(group: &str, name: &str, image: Option<&str>) -> ImageRequest {
    // start from the request the API builds by default
    let request = ImageRequest::new(group, name);
    match image {
        Some(image) => request.image(image),
        None => request,
    }
}

/// Build one legacy image entry (one label of an image)
///
/// # Arguments
///
/// * `config` - The image's config
pub(super) fn entry(config: &ImageRequest) -> Value {
    // an entry carries its config next to the build information
    json!({
        "build_image": true,
        "build_path": format!("images/{}", config.name),
        "dir": format!("images/{}", config.name),
        "image_tags": [format!("ghcr.io/example/{}:latest", config.name)],
        "config": serde_json::to_value(config).expect("serialize request"),
    })
}

/// Build one legacy pipeline version
///
/// # Arguments
///
/// * `group` - The pipeline's group
/// * `name` - The pipeline's name
/// * `stages` - Each stage with the version of its image the pipeline pins
pub(super) fn pipeline(group: &str, name: &str, stages: &[(&str, &str)]) -> Value {
    // all the stages run in one order group
    let order: Vec<&str> = stages.iter().map(|(stage, _)| *stage).collect();
    let images: serde_json::Map<String, Value> = stages
        .iter()
        .map(|(stage, version)| ((*stage).to_owned(), json!({ "version": version })))
        .collect();
    json!({ "latest": {
        "config": {
            "description": null, "group": group, "name": name,
            "order": [order], "sla": 100, "triggers": {}
        },
        "description": "",
        "dir": format!("pipelines/{group}/{name}"),
        "images": images,
    }})
}

/// Build a legacy toolbox from its images and pipelines
#[allow(clippy::needless_pass_by_value)]
///
/// # Arguments
///
/// * `images` - The `images` section: image names to labels to entries
/// * `pipelines` - The `pipelines` section
pub(super) fn toolbox(images: Value, pipelines: Value) -> Value {
    // a legacy toolbox holds exactly these sections
    json!({ "name": "test toolbox", "images": images, "pipelines": pipelines })
}

/// Convert a toolbox document with the given options
///
/// # Arguments
///
/// * `document` - The toolbox, as JSON
/// * `options` - The choices that shape the conversion
pub(super) async fn convert_with(document: &Value, options: &ConvertOptions) -> Converted {
    // convert with no base directory and a quiet bar
    convert_document(document.to_string().as_bytes(), None, options, &quiet())
        .await
        .expect("convert")
}

/// Convert a toolbox document with the default options
///
/// # Arguments
///
/// * `document` - The toolbox, as JSON
pub(super) async fn convert_default(document: &Value) -> Converted {
    // no options change the conversion
    convert_with(document, &ConvertOptions::default()).await
}

/// Get the codes of a conversion's issues, in the order they were found
///
/// # Arguments
///
/// * `converted` - The conversion
pub(super) fn issue_codes(converted: &Converted) -> Vec<String> {
    // each issue's code
    converted
        .summary
        .issues
        .iter()
        .map(|issue| issue.code.clone())
        .collect()
}

/// Store an image request the way the API does, with a creator and the default runtime
///
/// This is the stored form the migration of existing images converts, so a spec built from the
/// request can be compared with it.
///
/// # Arguments
///
/// * `request` - The request to store
pub(super) fn stored(request: &ImageRequest) -> Image {
    // every field of the stored image comes from the request or takes the API's default
    Image {
        group: request.group.clone(),
        name: request.name.clone(),
        version: request.version.clone(),
        image: request.image.clone(),
        creator: "io".to_owned(),
        lifetime: request.lifetime.clone(),
        timeout: request.timeout,
        resources: Resources::from(request.resources.clone()),
        spawn_limit: request.spawn_limit.clone(),
        scaler: request.scaler,
        runtime: 600.0,
        volumes: request.volumes.clone(),
        env: request.env.clone(),
        args: request.args.clone(),
        modifiers: request.modifiers.clone(),
        description: request.description.clone(),
        security_context: request.security_context.clone().unwrap_or_default(),
        used_by: Vec::default(),
        collect_logs: request.collect_logs,
        generator: request.generator,
        dependencies: request.dependencies.clone(),
        display_type: request.display_type,
        output_collection: request.output_collection.clone(),
        child_filters: request.child_filters.clone(),
        clean_up: request.clean_up.clone(),
        kvm: request.kvm.clone(),
        bans: HashMap::default(),
        network_policies: request.network_policies.clone(),
    }
}
