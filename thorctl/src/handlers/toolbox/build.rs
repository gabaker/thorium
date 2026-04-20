//! Builds a toolbox manifest from a directory of image and pipeline manifests

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use thorium::Error;
use walkdir::WalkDir;

use crate::args::toolbox::BuildToolbox;

// ─── TOML Input Models ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ToolboxConfig {
    name: String,
    registry: String,
    #[serde(default)]
    registries: Vec<String>,
}

#[derive(Deserialize)]
struct ManifestToml {
    name: String,
    #[serde(rename = "type")]
    manifest_type: ManifestType,
    #[serde(default = "default_version")]
    version: String,
    image_name: Option<String>,
    build_path: Option<String>,
    #[serde(default = "default_true")]
    build: bool,
    #[serde(default = "default_true")]
    allow_base_override: bool,
    base_image_token: Option<String>,
    description: Option<String>,
    #[serde(default)]
    images: Option<HashMap<String, PipelineImageToml>>,
    config_from: Option<String>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ManifestType {
    Image,
    Pipeline,
}

#[derive(Deserialize)]
struct PipelineImageToml {
    version: String,
}

fn default_version() -> String {
    "latest".to_string()
}

fn default_true() -> bool {
    true
}

// ─── JSON Output Models ────────────────────────────────────────────────────

#[derive(Serialize)]
struct BuildOutput {
    pipelines: HashMap<String, HashMap<String, BuildPipelineVersion>>,
    images: HashMap<String, HashMap<String, BuildImageVersion>>,
    name: String,
    registry: String,
    registries: Vec<String>,
}

#[derive(Serialize)]
struct BuildImageVersion {
    build_path: String,
    build_image: bool,
    image_tags: Vec<String>,
    allow_base_override: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_image_token: Option<String>,
    config: serde_json::Value,
}

#[derive(Serialize)]
struct BuildPipelineVersion {
    description: String,
    images: HashMap<String, PipelineImageOutput>,
    config: serde_json::Value,
}

#[derive(Serialize)]
struct PipelineImageOutput {
    version: String,
}

// ─── Build Logic ───────────────────────────────────────────────────────────

fn load_json_config(root: &Path, config_from: Option<&str>) -> Result<serde_json::Value, Error> {
    match config_from {
        Some(config_from) => {
            let config_path = root.join(config_from);
            let config_bytes = std::fs::read(&config_path).map_err(|e| {
                Error::new(format!(
                    "Failed to read config '{}': {e}",
                    config_path.display()
                ))
            })?;
            serde_json::from_slice(&config_bytes).map_err(|e| {
                Error::new(format!(
                    "Failed to parse config '{}': {e}",
                    config_path.display()
                ))
            })
        }
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
    }
}

fn build_image_version(
    manifest: &ManifestToml,
    root: &Path,
    registries: &[String],
    override_path: bool,
) -> Result<BuildImageVersion, Error> {
    let image_name = manifest.image_name.as_deref().unwrap_or("");
    let name = &manifest.name;
    let version = &manifest.version;

    if version.is_empty() {
        eprintln!(" No image version found for {name}");
    }
    if image_name.is_empty() {
        eprintln!(" No image name found for {name}");
    }

    let mut image_tags = Vec::new();
    if !image_name.is_empty() {
        for registry in registries {
            let tag = if override_path {
                format!("{registry}/{name}:{version}")
            } else {
                format!("{registry}/{image_name}:{version}")
            };
            if !image_tags.contains(&tag) {
                image_tags.push(tag);
            }
        }
    }

    let build_path_str = manifest.build_path.as_deref().unwrap_or("./");
    let image_build_path = if build_path_str == "./" || build_path_str == "." {
        format!("{}", root.display())
    } else {
        format!("{}/{build_path_str}", root.display())
    };

    let mut config = load_json_config(root, manifest.config_from.as_deref())?;

    if image_tags.is_empty() {
        eprintln!("No image tag specified, leaving {name} image config blank");
    } else if let serde_json::Value::Object(ref mut map) = config {
        map.insert(
            "image".to_string(),
            serde_json::Value::String(image_tags[0].clone()),
        );
    }

    Ok(BuildImageVersion {
        build_path: image_build_path,
        build_image: manifest.build,
        image_tags,
        allow_base_override: manifest.allow_base_override,
        base_image_token: manifest.base_image_token.clone(),
        config,
    })
}

fn build_pipeline_version(
    manifest: &ManifestToml,
    root: &Path,
) -> Result<BuildPipelineVersion, Error> {
    let description = manifest.description.clone().unwrap_or_default();

    let images: HashMap<String, PipelineImageOutput> = manifest
        .images
        .as_ref()
        .map(|imgs| {
            imgs.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        PipelineImageOutput {
                            version: v.version.clone(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let config = load_json_config(root, manifest.config_from.as_deref())?;

    Ok(BuildPipelineVersion {
        description,
        images,
        config,
    })
}

/// Build a toolbox manifest from a directory of image and pipeline manifests
pub fn build(cmd: &BuildToolbox) -> Result<(), Error> {
    let config_str = std::fs::read_to_string(&cmd.config).map_err(|e| {
        Error::new(format!(
            "Failed to read config file '{}': {e}",
            cmd.config.display()
        ))
    })?;
    let config: ToolboxConfig = toml::from_str(&config_str).map_err(|e| {
        Error::new(format!(
            "Failed to parse config TOML '{}': {e}",
            cmd.config.display()
        ))
    })?;

    let mut registries = config.registries.clone();
    if !registries.contains(&config.registry) {
        registries.push(config.registry.clone());
    }

    let mut images: HashMap<String, HashMap<String, BuildImageVersion>> = HashMap::new();
    let mut pipelines: HashMap<String, HashMap<String, BuildPipelineVersion>> = HashMap::new();

    for entry in WalkDir::new(&cmd.path).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.file_name() != Some(OsStr::new("manifest.toml")) {
            continue;
        }
        let root = match path.parent() {
            Some(r) if r != Path::new("") && r != Path::new(".") => r,
            _ => continue,
        };

        let manifest_str = std::fs::read_to_string(path).map_err(|e| {
            Error::new(format!("Failed to read '{}': {e}", path.display()))
        })?;
        let manifest: ManifestToml = toml::from_str(&manifest_str).map_err(|e| {
            Error::new(format!("Failed to parse '{}': {e}", path.display()))
        })?;

        match manifest.manifest_type {
            ManifestType::Image => {
                let version_entry =
                    build_image_version(&manifest, root, &registries, cmd.override_path)?;
                images
                    .entry(manifest.name.clone())
                    .or_default()
                    .insert(manifest.version.clone(), version_entry);
            }
            ManifestType::Pipeline => {
                let version_entry = build_pipeline_version(&manifest, root)?;
                pipelines
                    .entry(manifest.name.clone())
                    .or_default()
                    .insert(manifest.version.clone(), version_entry);
            }
        }
    }

    let output = BuildOutput {
        pipelines,
        images,
        name: config.name,
        registry: config.registry,
        registries,
    };

    let json = serde_json::to_string_pretty(&output)
        .map_err(|e| Error::new(format!("Failed to serialize toolbox output: {e}")))?;
    std::fs::write(&cmd.output, json)
        .map_err(|e| Error::new(format!("Failed to write '{}': {e}", cmd.output.display())))?;

    println!("Wrote toolbox manifest to '{}'", cmd.output.display());
    Ok(())
}
