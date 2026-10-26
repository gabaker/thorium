//! The toolbox manifest structure

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thorium::models::{ImageRequest, NetworkPolicyRequest, PipelineRequest};

/// A toolbox manifest – a description of pipelines and images that
/// can be imported into Thorium
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolboxManifest {
    /// The name of this toolbox
    pub name: String,
    /// The registry the images can be found at, if the toolbox declares a central one
    ///
    /// Optional: a toolbox documenting tools whose images live in various external
    /// registries leaves this unset and relies on each image config's own `image` url.
    /// Informational on import (the per-image `image` urls are authoritative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// A map of pipeline names to their details
    pub pipelines: HashMap<String, PipelineManifest>,
    /// A map of image names to their details
    pub images: HashMap<String, ImageManifest>,
    /// Whether this toolbox bundles container image tarballs alongside its configs
    ///
    /// When true, an import must load each image's `<name>.tar.gz` tarball (found in the
    /// image version's `dir`, or `images/<name>/` when `dir` is empty), push it to a target
    /// registry, and rewrite the image's url before creating it in Thorium.
    #[serde(default)]
    pub bundled_images: bool,
    /// The default registry base path that bundled images are pushed under on import
    ///
    /// Used as the fallback when `--image-path-prefix` is not given. Bundled images are
    /// pushed to `<image_path_prefix>/<group>/<name>:<tag>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path_prefix: Option<String>,
}

/// A pipeline entry in a toolbox manifest: its versions keyed by version label
#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineManifest {
    /// A map of pipeline versions to their details
    #[serde(flatten)]
    pub versions: HashMap<String, PipelineVersion>,
}

/// Details for a specific pipeline version
#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineVersion {
    /// The tool directory (where this pipeline's `manifest.toml` lives), relative to the
    /// `toolbox.json`'s location. Lets `export` find where a pipeline already lives and update it in
    /// place; empty for older toolboxes that predate the field (the default layout is used then).
    #[serde(default)]
    pub dir: String,
    /// A description of the pipeline for the purpose of the toolbox, not for
    /// Thorium itself
    #[serde(default)]
    pub description: String,
    /// A map of image names to their info for the pipeline
    #[serde(default)]
    pub images: HashMap<String, PipelineImage>,
    /// URL to fetch the pipeline config from (alternative to inline config)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_from: Option<String>,
    /// The pipeline's Thorium configuration (inline or resolved from `config_from`)
    #[serde(default)]
    pub config: Option<PipelineRequest>,
}

/// A pipeline's reference to an image version it runs
#[derive(Debug, Serialize, Deserialize)]
pub struct PipelineImage {
    /// The version of the image this pipeline expects
    pub version: String,
}

/// An image entry in a toolbox manifest: its versions keyed by version label
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageManifest {
    /// A map of image versions to their details
    #[serde(flatten)]
    pub versions: HashMap<String, ImageVersion>,
}

/// Details for a specific image version
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageVersion {
    /// The tool directory (where this image's `manifest.toml` and any bundled tarball live),
    /// relative to the `toolbox.json`'s location. Used to find a bundled image's tarball; empty
    /// for older toolboxes that predate the field, in which case the bundled-image lookup falls
    /// back to the default `images/<name>` layout.
    #[serde(default)]
    pub dir: String,
    /// The image's build path relative to the toolbox manifest's location
    #[serde(default)]
    pub build_path: String,
    /// URL to fetch the image config from (alternative to inline config)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_from: Option<String>,
    /// The image's Thorium configuration (inline or resolved from `config_from`)
    #[serde(default)]
    pub config: Option<ImageRequest>,
    /// The description from the image's `description.md`, carried when the config is a
    /// `config_from` URL that build can't inject it into; it fills in the fetched config's
    /// description when that has none
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// URLs to fetch network policy definitions from (resolved into
    /// `network_policies` before import, like `config_from`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies_from: Vec<String>,
    /// The network policies this image references, bundled so an import can
    /// create them in the target instance when missing
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies: Vec<NetworkPolicyRequest>,
}
