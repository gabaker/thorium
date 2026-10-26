//! Reading the build entries of a toolbox manifest and ordering them for building
//!
//! A legacy toolbox lists its images under `images`. A current toolbox lists the build of each
//! tool version under `tools.<name>.versions.<label>.build`, and the contexts no tool owns (base
//! images other builds start from) under `builds`. Only the parts a build reads are parsed, so
//! building never needs a manifest whose specs resolve.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use thorium::Error;

use crate::handlers::toolbox::build::{BaseImage, default_true};

/// The manifest files a toolbox directory may hold, in the order they are tried
///
/// The legacy file comes first because it is the file CI builds the tool images from.
const MANIFEST_FILES: [&str; 2] = ["toolbox.json", "toolbox.v2.json"];

/// The slice of a build entry that drives a build
#[derive(Debug, Clone, Deserialize)]
pub(super) struct BuildEntry {
    /// The build context to build from, relative to the manifest
    #[serde(default)]
    pub build_path: PathBuf,
    /// Whether this image should be built at all
    #[serde(default = "default_true")]
    pub build_image: bool,
    /// Every registry tag this image should carry
    #[serde(default)]
    pub image_tags: Vec<String>,
    /// This image's resolved base-image configuration (per-tool merged over global at build time)
    #[serde(default)]
    pub base_image: Option<BaseImage>,
}

/// The builds of one tool, as far as a build reads them
#[derive(Debug, Default, Deserialize)]
struct ToolBuilds {
    /// The tool's versions, keyed by label
    #[serde(default)]
    versions: BTreeMap<String, ToolVersionBuild>,
}

/// The build of one tool version, as far as a build reads it
#[derive(Debug, Default, Deserialize)]
struct ToolVersionBuild {
    /// How to build the version's container image, when it has one
    #[serde(default)]
    build: Option<BuildEntry>,
}

/// Only the parts of a toolbox manifest that builds care about
///
/// Deserialized as a subset on purpose: every other top-level field (name, registry, pipelines,
/// specs, and so on) is intentionally ignored, so no `deny_unknown_fields` here.
#[derive(Debug, Default, Deserialize)]
pub(super) struct BuildManifest {
    /// The legacy image entries mapped name -> version -> entry
    #[serde(default)]
    images: HashMap<String, HashMap<String, BuildEntry>>,
    /// The tools mapped name -> versions
    #[serde(default)]
    tools: BTreeMap<String, ToolBuilds>,
    /// The build contexts no tool owns, mapped name -> label -> entry
    #[serde(default)]
    builds: BTreeMap<String, BTreeMap<String, BuildEntry>>,
}

/// One image to build
#[derive(Debug, Clone)]
pub(super) struct Target {
    /// The name of the tool, base context, or legacy image
    pub name: String,
    /// The version label
    pub version: String,
    /// How to build it
    pub entry: BuildEntry,
}

/// The images to build, in the order to build them
#[derive(Debug)]
pub(super) struct Plan {
    /// The targets, base contexts first
    pub targets: Vec<Target>,
    /// Every name the manifest has an entry for, selected or not
    pub available: BTreeSet<String>,
}

/// Find the manifest file a path names, which may be a toolbox directory
///
/// # Arguments
///
/// * `path` - The path the user gave
pub(super) fn locate(path: &Path) -> Result<PathBuf, Error> {
    // a file is used as it is
    if !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    // a directory holds the manifest under one of its usual names
    MANIFEST_FILES
        .iter()
        .map(|file| path.join(file))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            Error::new(format!(
                "Failed to find a toolbox manifest in '{}': it holds no {}; run `thorctl toolbox \
                 build` to generate one",
                path.display(),
                MANIFEST_FILES.join(" or ")
            ))
        })
}

/// Choose the targets of a manifest and order them for building
///
/// Base contexts (`builds`) come first, then the tools, then legacy images, each by name and
/// label, so a tool built from a base context finds it built already.
///
/// # Arguments
///
/// * `manifest` - The parsed manifest
/// * `names` - The names to build; empty builds everything
pub(super) fn plan(manifest: BuildManifest, names: &[String]) -> Result<Plan, Error> {
    let mut available = BTreeSet::new();
    let mut bases: Vec<Target> = Vec::new();
    let mut tools: Vec<Target> = Vec::new();
    let mut images: Vec<Target> = Vec::new();
    // an entry is selected when no names were given or its name is one of them
    let selected = |name: &str| names.is_empty() || names.iter().any(|wanted| wanted == name);
    for (name, labels) in manifest.builds {
        available.insert(name.clone());
        if selected(&name) {
            bases.extend(labels.into_iter().map(|(version, entry)| Target {
                name: name.clone(),
                version,
                entry,
            }));
        }
    }
    for (name, tool) in manifest.tools {
        // a tool with no build section on any version has nothing to build
        if tool
            .versions
            .values()
            .any(|version| version.build.is_some())
        {
            available.insert(name.clone());
        }
        if selected(&name) {
            tools.extend(tool.versions.into_iter().filter_map(|(version, build)| {
                build.build.map(|entry| Target {
                    name: name.clone(),
                    version,
                    entry,
                })
            }));
        }
    }
    for (name, versions) in manifest.images {
        available.insert(name.clone());
        if selected(&name) {
            images.extend(versions.into_iter().map(|(version, entry)| Target {
                name: name.clone(),
                version,
                entry,
            }));
        }
    }
    // an image named by two sections can't be built under two different instructions
    let mut seen: BTreeMap<(String, String), &str> = BTreeMap::new();
    for (section, targets) in [("builds", &bases), ("tools", &tools), ("images", &images)] {
        for target in targets {
            if let Some(first) = seen.insert((target.name.clone(), target.version.clone()), section)
            {
                return Err(Error::new(format!(
                    "'{}@{}' has build entries in both `{first}` and `{section}`",
                    target.name, target.version
                )));
            }
        }
    }
    // sort each group by (name, version) so the order is the same on every run
    for group in [&mut bases, &mut tools, &mut images] {
        group
            .sort_by(|left, right| (&left.name, &left.version).cmp(&(&right.name, &right.version)));
    }
    let mut targets = bases;
    targets.extend(tools);
    targets.extend(images);
    Ok(Plan { targets, available })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a manifest and plan it
    ///
    /// # Arguments
    ///
    /// * `json` - The manifest
    /// * `names` - The names to build
    fn planned(json: &str, names: &[&str]) -> Result<Vec<String>, Error> {
        // the manifest is parsed as build-images parses it
        let manifest: BuildManifest = serde_json::from_str(json).expect("a manifest");
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        Ok(plan(manifest, &names)?
            .targets
            .iter()
            .map(|target| format!("{}@{}", target.name, target.version))
            .collect())
    }

    /// A v2 manifest with a base build, two tools, and a tool without a build
    const V2: &str = r#"{"tools": {
        "zeta": {"versions": {"latest": {"build": {"build_path": "z", "image_tags": ["r/z"]}}}},
        "alpha": {"versions": {"2": {"build": {"build_path": "a2"}}, "1": {"build": {"build_path": "a1"}}}},
        "plain": {"versions": {"latest": {"spec": {"Image": {}}}}}
      },
      "builds": {"base": {"1": {"build_path": "b1"}}, "ghidra-base": {"12.1": {"build_path": "g"}}}}"#;

    /// Base contexts come first, then tools, each by name and label
    #[test]
    fn v2_orders_bases_before_tools() {
        let order = planned(V2, &[]).expect("a plan");
        assert_eq!(
            order,
            [
                "base@1",
                "ghidra-base@12.1",
                "alpha@1",
                "alpha@2",
                "zeta@latest"
            ]
        );
    }

    /// A legacy manifest keeps its name order and builds nothing before it
    #[test]
    fn legacy_images_are_ordered_by_name() {
        let json = r#"{"images": {"b": {"latest": {"build_path": "b"}}, "a": {"latest": {"build_path": "a"}, "1": {"build_path": "a1"}}}}"#;
        assert_eq!(
            planned(json, &[]).expect("a plan"),
            ["a@1", "a@latest", "b@latest"]
        );
    }

    /// Names narrow the plan across every section
    #[test]
    fn names_select_across_sections() {
        let order = planned(V2, &["base", "zeta"]).expect("a plan");
        assert_eq!(order, ["base@1", "zeta@latest"]);
        // a tool with no build section is not an available name
        let manifest: BuildManifest = serde_json::from_str(V2).expect("a manifest");
        let plan = plan(manifest, &[]).expect("a plan");
        assert!(!plan.available.contains("plain"));
        assert!(plan.available.contains("ghidra-base"));
    }

    /// An entry in both `tools` and `images` is refused
    #[test]
    fn a_label_in_two_sections_is_refused() {
        let json = r#"{"images": {"a": {"latest": {"build_path": "a"}}},
            "tools": {"a": {"versions": {"latest": {"build": {"build_path": "a"}}}}}}"#;
        let err = planned(json, &[]).expect_err("a refusal");
        assert!(err.to_string().contains("a@latest"), "{err}");
    }

    /// A build entry builds unless it says otherwise
    #[test]
    fn entries_build_by_default() {
        let manifest: BuildManifest = serde_json::from_str(V2).expect("a manifest");
        let plan = plan(manifest, &[]).expect("a plan");
        assert!(plan.targets.iter().all(|target| target.entry.build_image));
    }
}
