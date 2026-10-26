//! Building the tools, base builds, toolsets, agents, workflows, and pipelines with stage tools of
//! a toolbox from their `manifest.toml` files
//!
//! The items are collected while the directory is walked and assembled afterwards, in a fixed
//! order, so the same tree always builds the same manifest whatever order the files were found in.

use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use thorium::Error;
use thorium::models::{NetworkPolicyRequest, ToolKind};
use url::Url;

use super::{
    BaseImage, ManifestToml, ManifestType, apply_description_md, build_path_relative_to_output,
    derive_image_tags, load_json_config, load_network_policies, merge_base_image,
    pipeline_order_images,
};
use crate::handlers::progress;
use crate::handlers::toolbox::v2::types::{
    AgentManifest, BuildManifest, PipelineConfig, PipelineManifestV2, PipelineVersionV2,
    ToolManifest, ToolSetManifest, ToolVersionManifest, WorkflowManifest, check_label, check_name,
};
use crate::utils;

/// The settings the image tags of tools are derived with
pub(super) struct TagSettings<'a> {
    /// The registries to derive a tag for
    pub registries: &'a [String],
    /// Whether to tag with a manifest's `image_name` instead of the tool's name
    pub use_image_path: bool,
    /// The registry path inserted before the leaf of a tag
    pub image_path_prefix: Option<&'a str>,
    /// The suffix appended to the version of a derived tag
    pub tag_suffix: Option<&'a str>,
    /// The toolbox-wide base image settings each tool's own merge over
    pub global_base_image: Option<&'a BaseImage>,
}

/// The items of a toolbox that only the current format can hold
#[derive(Debug, Default)]
pub(super) struct Items {
    /// The tools, keyed by name
    pub tools: BTreeMap<String, ToolManifest>,
    /// The base builds no tool owns, keyed by name then version label
    pub builds: BTreeMap<String, BTreeMap<String, BuildManifest>>,
    /// The toolsets, keyed by name
    pub toolsets: BTreeMap<String, ToolSetManifest>,
    /// The agents, keyed by name
    pub agents: BTreeMap<String, AgentManifest>,
    /// The workflows, keyed by name
    pub workflows: BTreeMap<String, WorkflowManifest>,
    /// The pipelines with stage tools, keyed by name
    pub pipelines: BTreeMap<String, PipelineManifestV2>,
}

impl Items {
    /// Whether there are no items
    pub(super) fn is_empty(&self) -> bool {
        // every section has to be empty
        self.tools.is_empty()
            && self.builds.is_empty()
            && self.toolsets.is_empty()
            && self.agents.is_empty()
            && self.workflows.is_empty()
            && self.pipelines.is_empty()
    }

    /// Get the container image each tool version and base build makes, keyed by name and version
    ///
    /// A legacy image can reuse a tool's or a base build's container image with `image_from`, as
    /// it does once `--rewrite-manifests` turned its base into a tool or a build; a tool's image is
    /// the one in the version's spec, or else the first tag its build derives, and a base build's
    /// is its first tag.
    pub(super) fn image_urls(&self) -> HashMap<(String, String), String> {
        // every base build that is tagged
        let mut urls = HashMap::new();
        for (name, labels) in &self.builds {
            for (label, build) in labels {
                if let Some(url) = build.image_tags.first() {
                    urls.insert((name.clone(), label.clone()), url.clone());
                }
            }
        }
        // every version of every tool that runs a container image, which wins over a base build
        // of the same name and version
        for (name, tool) in &self.tools {
            for (label, version) in &tool.versions {
                let url = version.spec.as_ref().and_then(spec_image).or_else(|| {
                    version
                        .build
                        .as_ref()
                        .and_then(|build| build.image_tags.first().cloned())
                });
                if let Some(url) = url {
                    urls.insert((name.clone(), label.clone()), url);
                }
            }
        }
        urls
    }
}

/// Get how early a kind of manifest is assembled
///
/// # Arguments
///
/// * `manifest_type` - The kind of manifest
fn type_rank(manifest_type: &ManifestType) -> u8 {
    // tools first, then the items built on them, then the base builds no item names
    match manifest_type {
        ManifestType::Tool => 0,
        ManifestType::Toolset => 1,
        ManifestType::Agent => 2,
        ManifestType::Workflow => 3,
        ManifestType::Pipeline | ManifestType::Image => 4,
        ManifestType::Build => 5,
    }
}

/// Parse the kind a tool manifest names
///
/// # Arguments
///
/// * `name` - The tool's name, for the error
/// * `kind` - The kind the manifest names
fn parse_kind(name: &str, kind: &str) -> Result<ToolKind, Error> {
    // kinds are matched without regard to case so `image` and `Image` both work
    match kind.to_ascii_lowercase().as_str() {
        "image" => Ok(ToolKind::Image),
        "agentic" => Ok(ToolKind::Agentic),
        "harness" => Ok(ToolKind::Harness),
        _ => Err(Error::new(format!(
            "Tool '{name}' has the kind '{kind}'; use image, agentic, or harness"
        ))),
    }
}

/// Read a tool's `description.md`, if it has one
///
/// # Arguments
///
/// * `root` - The directory the manifest is in
/// * `name` - The item's name, for warnings
pub(super) fn description_md(root: &Path, name: &str) -> Option<String> {
    let path = root.join("description.md");
    // only a missing file means there is no description
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            progress::warn(format!(
                "{name}: failed to read '{}' ({err}); its description is not applied",
                path.display()
            ));
            return None;
        }
    };
    // trailing whitespace is dropped so the description is the same on every run, and an empty
    // file is no description
    let text = text.trim_end().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Get the description a toolset, agent, or workflow is built with
///
/// A `description.md` beside the manifest wins, then the manifest's `description`, then the one in
/// the item's document.
///
/// # Arguments
///
/// * `manifest` - The item's manifest
/// * `root` - The directory the manifest is in
/// * `document` - The description in the item's document
fn entity_description(
    manifest: &ManifestToml,
    root: &Path,
    document: Option<String>,
) -> Option<String> {
    // the first source with a description is the item's
    description_md(root, &manifest.name)
        .or_else(|| manifest.description.clone())
        .or(document)
}

/// Whether a spec reference is a URL to fetch later rather than a file to read now
///
/// # Arguments
///
/// * `reference` - The `spec_from` or `config_from` value
fn is_remote(reference: &str) -> bool {
    // only http and https are fetched; anything else (including a Windows drive path) is a file
    Url::parse(reference).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Read a spec or document file as JSON
///
/// # Arguments
///
/// * `path` - The file to read; JSON when it ends in `.json`, YAML (which includes JSON) otherwise
fn read_value(path: &Path) -> Result<Value, Error> {
    // read the whole file
    let text = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("Failed to read '{}': {err}", path.display())))?;
    // parse it as the format its extension names
    if path.extension().is_some_and(|ext| ext == "json") {
        serde_json::from_str(&text)
            .map_err(|err| Error::new(format!("Failed to parse '{}': {err}", path.display())))
    } else {
        serde_norway::from_str(&text)
            .map_err(|err| Error::new(format!("Failed to parse '{}': {err}", path.display())))
    }
}

/// Get the `environment` object of an image or harness spec
///
/// # Arguments
///
/// * `spec` - The spec
fn environment(spec: &Value) -> Option<&Value> {
    // an image spec holds it at the top, a harness spec in its workload
    if let Some(image) = spec.get("Image") {
        return image.get("environment");
    }
    spec.get("Harness")?.get("workload")?.get("environment")
}

/// Get the `environment` object of an image or harness spec for writing
///
/// # Arguments
///
/// * `spec` - The spec
fn environment_mut(spec: &mut Value) -> Option<&mut Value> {
    // an image spec holds it at the top, a harness spec in its workload
    if spec.get("Image").is_some() {
        return spec.get_mut("Image")?.get_mut("environment");
    }
    spec.get_mut("Harness")?
        .get_mut("workload")?
        .get_mut("environment")
}

/// Get the container image a spec runs, when it names one
///
/// # Arguments
///
/// * `spec` - The spec
fn spec_image(spec: &Value) -> Option<String> {
    // only K8s and Windows environments run a container image
    let environment = environment(spec)?;
    ["K8s", "Windows"]
        .iter()
        .find_map(|scaler| environment.get(*scaler)?.get("image")?.as_str())
        .filter(|image| !image.is_empty())
        .map(str::to_owned)
}

/// Whether a spec runs a container but names no image yet
///
/// # Arguments
///
/// * `spec` - The spec
fn lacks_image(spec: &Value) -> bool {
    // the environment must run a container, and the image must be empty
    environment(spec).is_some_and(|environment| {
        ["K8s", "Windows"]
            .iter()
            .any(|scaler| environment.get(*scaler).is_some())
    }) && spec_image(spec).is_none()
}

/// Set the container image a spec runs
///
/// # Arguments
///
/// * `spec` - The spec
/// * `image` - The image to set
fn set_spec_image(spec: &mut Value, image: &str) {
    // write it into whichever container environment the spec has
    if let Some(environment) = environment_mut(spec) {
        for scaler in ["K8s", "Windows"] {
            if let Some(slot) = environment
                .get_mut(scaler)
                .and_then(|environment| environment.get_mut("image"))
            {
                *slot = Value::String(image.to_owned());
            }
        }
    }
}

/// Where the manifest of an item is, for errors
///
/// # Arguments
///
/// * `root` - The directory the manifest is in
fn here(root: &Path) -> String {
    // the directory is what an author looks for
    root.display().to_string()
}

/// Build the build section of a tool version, and set the image its spec runs
///
/// # Arguments
///
/// * `manifest` - The tool's manifest
/// * `root` - The directory the manifest is in
/// * `output_dir` - The directory the toolbox is written to
/// * `tags` - The settings image tags are derived with
/// * `spec` - The inline spec, when the manifest has one; its image is set to the first tag
fn tool_build(
    manifest: &ManifestToml,
    root: &Path,
    output_dir: &Path,
    tags: &TagSettings<'_>,
    spec: &mut Option<Value>,
) -> BuildManifest {
    let name = &manifest.name;
    let dir = build_path_relative_to_output(root, output_dir);
    // the build context is relative to the manifest and defaults to its own directory
    let context = manifest.build_path.as_deref().unwrap_or("./");
    let context_dir = if context == "./" || context == "." {
        root.to_path_buf()
    } else {
        root.join(context)
    };
    let enabled = manifest.build_image.unwrap_or(manifest.build);
    let existing = spec.as_ref().and_then(spec_image);
    // an image that is built derives its tags, while one that is not keeps the image it names
    let image_tags = if enabled {
        let derived = derive_image_tags(
            manifest.image_name.as_deref().unwrap_or(""),
            name,
            &manifest.version,
            tags.registries,
            tags.use_image_path,
            tags.image_path_prefix,
            tags.tag_suffix,
        );
        match derived.first() {
            Some(first) => {
                // a derived tag replaces the image the spec names, which is worth saying
                if let Some(existing) = existing.as_ref().filter(|existing| *existing != first) {
                    progress::warn(format!(
                        "{name}: build derived image tag '{first}' overrides the spec's image \
                         '{existing}' (set build_image = false to keep the spec's image)"
                    ));
                }
                if let Some(spec) = spec.as_mut() {
                    set_spec_image(spec, first);
                }
                derived
            }
            None => {
                // with nothing to derive from, the image the spec names is the only tag
                if let Some(existing) = &existing {
                    progress::warn(format!(
                        "{name}: could not derive a registry tag (no registry, or \
                         --use-image-path with no image_name); falling back to the image \
                         '{existing}' set in its spec"
                    ));
                }
                existing.clone().into_iter().collect()
            }
        }
    } else {
        // an image that is not built keeps the real path an export recorded, or its spec's image
        let pinned = manifest
            .exported_image_path
            .as_deref()
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .or(existing);
        if let (Some(url), Some(spec)) = (&pinned, spec.as_mut()) {
            set_spec_image(spec, url);
        }
        pinned.into_iter().collect()
    };
    // an entry that is built but derives no tag builds nothing, which is worth saying
    if enabled && image_tags.is_empty() {
        progress::warn(format!(
            "{name}: build is enabled but no image tag could be derived; add a 'registry' to \
             config.toml or an image to its spec. build-images will skip it."
        ));
    }
    BuildManifest {
        dir,
        build_path: build_path_relative_to_output(&context_dir, output_dir),
        build_image: enabled,
        image_tags,
        base_image: merge_base_image(tags.global_base_image, manifest.base_image.as_ref()),
    }
}

/// Build the build section of a tool version that reuses another item's image with `image_from`
///
/// The section is never built and has no tags yet: [`resolve_image_from`] fills in the image once
/// every tool and base build is known.
///
/// # Arguments
///
/// * `manifest` - The tool's manifest
/// * `root` - The directory the manifest is in
/// * `output_dir` - The directory the toolbox is written to
/// * `spec` - The inline spec, when the manifest has one
/// * `remote` - Whether the spec is fetched from a URL at import
fn reused_build(
    manifest: &ManifestToml,
    root: &Path,
    output_dir: &Path,
    spec: Option<&Value>,
    remote: bool,
) -> Result<BuildManifest, Error> {
    let id = utils::entry_id(None, &manifest.name, &manifest.version);
    // the reused image is written into the spec, so the spec has to be a local file
    if remote {
        return Err(Error::new(format!(
            "Tool '{id}' in '{}' reuses an image with [image_from], which needs a local spec file \
             to write the image into",
            here(root)
        )));
    }
    // the image comes from one place only
    if let Some(image) = spec.and_then(spec_image) {
        return Err(Error::new(format!(
            "Tool '{id}' in '{}' sets the image '{image}' in its spec and also reuses an image with \
             [image_from]; leave the spec's image empty",
            here(root)
        )));
    }
    // the build context is recorded like a built tool's, but nothing is built from it
    let context = manifest.build_path.as_deref().unwrap_or("./");
    let context_dir = if context == "./" || context == "." {
        root.to_path_buf()
    } else {
        root.join(context)
    };
    Ok(BuildManifest {
        dir: build_path_relative_to_output(root, output_dir),
        build_path: build_path_relative_to_output(&context_dir, output_dir),
        build_image: false,
        image_tags: Vec::new(),
        base_image: None,
    })
}

/// Build one version of a base build from its `type = "build"` manifest
///
/// A base build is an image other builds start from that no tool runs, built from its directory
/// and tagged like a legacy build-only image.
///
/// # Arguments
///
/// * `manifest` - The build's manifest
/// * `root` - The directory the manifest is in
/// * `output_dir` - The directory the toolbox is written to
/// * `tags` - The settings image tags are derived with
fn base_build(
    manifest: &ManifestToml,
    root: &Path,
    output_dir: &Path,
    tags: &TagSettings<'_>,
) -> Result<BuildManifest, Error> {
    let name = &manifest.name;
    let id = utils::entry_id(None, name, &manifest.version);
    // a base build is only built, so the keys that describe how a tool runs have no place in it
    let runtime_keys = [
        ("config_from", manifest.config_from.is_some()),
        ("spec_from", manifest.spec_from.is_some()),
        ("image_from", manifest.image_from.is_some()),
    ];
    if let Some((key, _)) = runtime_keys.iter().find(|(_, set)| *set) {
        return Err(Error::new(format!(
            "Build '{id}' in '{}' sets `{key}`, which a base build has no use for; describe what \
             runs from the image with a tool manifest",
            here(root)
        )));
    }
    // the build context is relative to the manifest and defaults to its own directory
    let context = manifest.build_path.as_deref().unwrap_or("./");
    let context_dir = if context == "./" || context == "." {
        root.to_path_buf()
    } else {
        root.join(context)
    };
    let enabled = manifest.build_image.unwrap_or(manifest.build);
    // the tags are derived as a legacy build-only image's are
    let image_tags = derive_image_tags(
        manifest.image_name.as_deref().unwrap_or(""),
        name,
        &manifest.version,
        tags.registries,
        tags.use_image_path,
        tags.image_path_prefix,
        tags.tag_suffix,
    );
    // a build that is enabled but derives no tag builds nothing, which is worth saying
    if enabled && image_tags.is_empty() {
        progress::warn(format!(
            "{name}: build is enabled but no image tag could be derived; add a 'registry' to \
             config.toml (the build name is the tag leaf; pass --use-image-path with an \
             'image_name' for a repo-style path). build-images will skip it."
        ));
    }
    Ok(BuildManifest {
        dir: build_path_relative_to_output(root, output_dir),
        build_path: build_path_relative_to_output(&context_dir, output_dir),
        build_image: enabled,
        image_tags,
        base_image: merge_base_image(tags.global_base_image, manifest.base_image.as_ref()),
    })
}

/// Fill in the image of every tool version that reuses another item's image with `image_from`
///
/// A chain of reuses is followed to a tool or base build that makes an image; the image is
/// written into the reusing version's spec and is the only tag of its (never built) build.
///
/// # Arguments
///
/// * `items` - The items built so far
/// * `reuses` - Each reusing tool version mapped to the item it names
fn resolve_image_from(
    items: &mut Items,
    reuses: &BTreeMap<(String, String), (String, String)>,
) -> Result<(), Error> {
    // the images of the items that make one themselves
    let urls: HashMap<(String, String), String> = items
        .image_urls()
        .into_iter()
        .filter(|(key, _)| !reuses.contains_key(key))
        .collect();
    let mut errors: Vec<String> = Vec::new();
    for key in reuses.keys() {
        // follow the chain until it reaches an item with an image of its own
        let mut seen: HashSet<&(String, String)> = HashSet::from([key]);
        let mut target = &reuses[key];
        let resolved = loop {
            if let Some(url) = urls.get(target) {
                break Ok(url.clone());
            }
            if !seen.insert(target) {
                break Err(format!(
                    "the image_from chain cycles back to '{}'",
                    utils::entry_id(None, &target.0, &target.1)
                ));
            }
            match reuses.get(target) {
                Some(next) => target = next,
                None => {
                    let known = items
                        .tools
                        .get(&target.0)
                        .is_some_and(|tool| tool.versions.contains_key(&target.1))
                        || items
                            .builds
                            .get(&target.0)
                            .is_some_and(|labels| labels.contains_key(&target.1));
                    break Err(if known {
                        format!(
                            "'{}' makes no container image to reuse",
                            utils::entry_id(None, &target.0, &target.1)
                        )
                    } else {
                        format!(
                            "no tool or build manifest is '{}'",
                            utils::entry_id(None, &target.0, &target.1)
                        )
                    });
                }
            }
        };
        match resolved {
            Ok(url) => {
                // the reused image goes into the spec and is the build's only tag
                if let Some(version) = items
                    .tools
                    .get_mut(&key.0)
                    .and_then(|tool| tool.versions.get_mut(&key.1))
                {
                    if let Some(spec) = version.spec.as_mut() {
                        set_spec_image(spec, &url);
                    }
                    if let Some(build) = version.build.as_mut() {
                        build.image_tags = vec![url];
                    }
                }
            }
            Err(reason) => errors.push(format!(
                "tool {} -> {reason}",
                utils::entry_id(None, &key.0, &key.1)
            )),
        }
    }
    if !errors.is_empty() {
        return Err(Error::new(format!(
            "unresolvable image_from reference(s):\n  {}",
            errors.join("\n  ")
        )));
    }
    Ok(())
}

/// Build one version of a tool from its manifest
///
/// # Arguments
///
/// * `manifest` - The tool's manifest
/// * `root` - The directory the manifest is in
/// * `output_dir` - The directory the toolbox is written to
/// * `tags` - The settings image tags are derived with
fn tool_version(
    manifest: &ManifestToml,
    root: &Path,
    output_dir: &Path,
    tags: &TagSettings<'_>,
) -> Result<ToolVersionManifest, Error> {
    let name = &manifest.name;
    let label = &manifest.version;
    // a tool needs a spec, which is a file beside the manifest or a URL fetched at import
    let spec_from = manifest.spec_from.as_deref().ok_or_else(|| {
        Error::new(format!(
            "Tool '{name}@{label}' in '{}' needs a `spec_from` naming its spec file",
            here(root)
        ))
    })?;
    let (mut spec, remote) = if is_remote(spec_from) {
        (None, Some(spec_from.to_owned()))
    } else {
        (Some(read_value(&root.join(spec_from))?), None)
    };
    // an inline spec must be an object keyed by its kind
    if let Some(value) = &spec
        && !value.is_object()
    {
        return Err(Error::new(format!(
            "The spec of tool '{name}@{label}' ('{}') must be a mapping with one key: Image, \
             Agentic, or Harness",
            root.join(spec_from).display()
        )));
    }
    // a tool reusing another item's image has a build section that is never built, and its image
    // is filled in once every tool and base build is known; otherwise a manifest with a build
    // context or a build switch has a build section
    let build = if manifest.image_from.is_some() {
        Some(reused_build(
            manifest,
            root,
            output_dir,
            spec.as_ref(),
            remote.is_some(),
        )?)
    } else if manifest.build_path.is_some() || manifest.build_image.is_some() {
        Some(tool_build(manifest, root, output_dir, tags, &mut spec))
    } else {
        None
    };
    // a container tool with no image anywhere can't be scheduled, so say so now
    if let Some(spec) = &spec
        && manifest.image_from.is_none()
        && lacks_image(spec)
        && build
            .as_ref()
            .is_none_or(|build| build.image_tags.is_empty())
    {
        return Err(Error::new(format!(
            "Tool '{name}@{label}' in '{}' has no container image: set `image` in its spec, or \
             add a 'registry' to config.toml and a `build_path` to derive one",
            here(root)
        )));
    }
    // bundled network policies are read now, and remote ones fetched at import
    let (network_policies_from, policies) =
        load_network_policies(root, manifest.network_policies_from.as_ref())?;
    let mut network_policies = Vec::new();
    for policy in policies {
        network_policies.push(
            serde_json::from_value::<NetworkPolicyRequest>(policy).map_err(|err| {
                Error::new(format!(
                    "A network policy of tool '{name}@{label}' is not valid: {err}"
                ))
            })?,
        );
    }
    Ok(ToolVersionManifest {
        dir: build_path_relative_to_output(root, output_dir),
        spec,
        spec_from: remote,
        build,
        provides: manifest.provides.clone(),
        network_policies_from,
        network_policies,
        legacy: None,
    })
}

/// Merge what a tool version's manifest says about the tool into the tool
///
/// # Arguments
///
/// * `tool` - The tool built so far
/// * `manifest` - The manifest of one of its versions
/// * `root` - The directory the manifest is in, for errors
/// * `first_root` - The directory the first version's manifest was in, for errors
fn merge_tool_root(
    tool: &mut ToolManifest,
    manifest: &ManifestToml,
    root: &Path,
    first_root: &str,
) -> Result<(), Error> {
    let name = &manifest.name;
    let differ = |what: &str| {
        Error::new(format!(
            "Tool '{name}' has a different {what} in '{}' and '{first_root}'; every version of a \
             tool must agree",
            here(root)
        ))
    };
    // the kind, groups, harness settings, and latest label must agree across versions
    if let Some(kind) = manifest.kind.as_deref() {
        let kind = parse_kind(name, kind)?;
        match tool.kind {
            Some(existing) if existing != kind => return Err(differ("kind")),
            _ => tool.kind = Some(kind),
        }
    }
    if !manifest.groups.is_empty() {
        if tool.groups.is_empty() {
            tool.groups.clone_from(&manifest.groups);
        } else if tool.groups != manifest.groups {
            return Err(differ("list of groups"));
        }
    }
    if !manifest.harness_env.is_empty() {
        if tool.harness_env.is_empty() {
            tool.harness_env.clone_from(&manifest.harness_env);
        } else if tool.harness_env != manifest.harness_env {
            return Err(differ("harness_env"));
        }
    }
    if !manifest.disabled_models.is_empty() {
        if tool.disabled_models.is_empty() {
            tool.disabled_models.clone_from(&manifest.disabled_models);
        } else if tool.disabled_models != manifest.disabled_models {
            return Err(differ("disabled_models"));
        }
    }
    if manifest.latest {
        match &tool.latest {
            Some(existing) if *existing != manifest.version => {
                return Err(Error::new(format!(
                    "Tool '{name}' marks both '{existing}' and '{}' as its latest version",
                    manifest.version
                )));
            }
            _ => tool.latest = Some(manifest.version.clone()),
        }
    }
    // the first description found is the tool's, and tags are the union of every version's
    if tool.description.is_none() {
        tool.description = description_md(root, name).or_else(|| manifest.description.clone());
    }
    for (key, values) in &manifest.tags {
        let entry = tool.tags.entry(key.clone()).or_default();
        for value in values {
            if !entry.contains(value) {
                entry.push(value.clone());
            }
        }
    }
    Ok(())
}

/// Read the document a toolset, agent, or workflow manifest points at
///
/// # Arguments
///
/// * `manifest` - The item's manifest
/// * `root` - The directory the manifest is in
/// * `what` - The kind of item, for errors
fn entity_document<T: DeserializeOwned>(
    manifest: &ManifestToml,
    root: &Path,
    what: &str,
) -> Result<T, Error> {
    let name = &manifest.name;
    // the document is a file beside the manifest
    let spec_from = manifest.spec_from.as_deref().ok_or_else(|| {
        Error::new(format!(
            "The {what} '{name}' in '{}' needs a `spec_from` naming its document",
            here(root)
        ))
    })?;
    if is_remote(spec_from) {
        return Err(Error::new(format!(
            "The {what} '{name}' in '{}' names a URL as its `spec_from`, but only tools can be \
             fetched from a URL; use a file",
            here(root)
        )));
    }
    // parse it into the item's own type, which refuses keys it doesn't know
    let path = root.join(spec_from);
    let value = read_value(&path)?;
    serde_json::from_value(value).map_err(|err| {
        Error::new(format!(
            "The {what} '{name}' in '{}' is not valid: {err}",
            path.display()
        ))
    })
}

/// Build one version of a pipeline whose stages name tools
///
/// # Arguments
///
/// * `manifest` - The pipeline's manifest
/// * `root` - The directory the manifest is in
/// * `output_dir` - The directory the toolbox is written to
fn pipeline_version(
    manifest: &ManifestToml,
    root: &Path,
    output_dir: &Path,
) -> Result<PipelineVersionV2, Error> {
    let name = &manifest.name;
    // read the config the manifest points at, or note the URL to fetch it from
    let loaded = load_json_config(root, manifest.config_from.as_deref())?;
    let mut config = loaded.value;
    // description.md becomes the pipeline's description, in the config and on the version
    let description = apply_description_md(root, name, &mut config)
        .or_else(|| manifest.description.clone())
        .unwrap_or_default();
    // the config is the request without its stage tools, which the manifest holds
    if let Some(Value::Object(map)) = &mut config {
        map.remove("stage_tools");
        map.remove("images");
    }
    let stage_tools = manifest.stage_tools.clone().unwrap_or_default();
    let config = match config {
        Some(Value::Object(map)) if !map.is_empty() => Some(
            serde_json::from_value::<PipelineConfig>(Value::Object(map)).map_err(|err| {
                Error::new(format!(
                    "The config of pipeline '{name}' in '{}' is not valid: {err}",
                    here(root)
                ))
            })?,
        ),
        _ => None,
    };
    // a pipeline with neither an inline config nor a URL to fetch one from has no stages
    if config.is_none() && loaded.url.is_none() {
        return Err(Error::new(format!(
            "Pipeline '{name}' in '{}' needs a `config_from` naming its config",
            here(root)
        )));
    }
    // the stage tools should cover the stages the order runs
    if let Some(config) = &config {
        let ordered = pipeline_order_images(&serde_json::json!({"order": config.order}));
        let mut missing: Vec<&str> = ordered
            .iter()
            .filter(|stage| !stage_tools.contains_key(*stage))
            .map(String::as_str)
            .collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            progress::warn(format!(
                "{name}: order runs stage(s) missing from manifest.toml's [stage_tools] table: {}",
                missing.join(", ")
            ));
        }
    }
    Ok(PipelineVersionV2 {
        dir: build_path_relative_to_output(root, output_dir),
        description,
        config,
        config_from: loaded.url,
        stage_tools,
        incomplete: false,
    })
}

/// Assemble the tools, base builds, toolsets, agents, workflows, and pipelines found while walking
/// a toolbox
///
/// # Arguments
///
/// * `found` - Each manifest found with the directory it is in
/// * `output_dir` - The directory the toolbox is written to
/// * `tags` - The settings image tags are derived with
#[allow(clippy::too_many_lines)]
pub(super) fn build_items(
    mut found: Vec<(ManifestToml, PathBuf)>,
    output_dir: &Path,
    tags: &TagSettings<'_>,
) -> Result<Items, Error> {
    // assemble in a fixed order, so the first of two clashing versions never depends on the walk
    found.sort_by(|(left, left_root), (right, right_root)| {
        (
            type_rank(&left.manifest_type),
            &left.name,
            &left.version,
            left_root,
        )
            .cmp(&(
                type_rank(&right.manifest_type),
                &right.name,
                &right.version,
                right_root,
            ))
    });
    let mut items = Items::default();
    // the directory each item was first found in, so a duplicate can name both
    let mut places: HashMap<(u8, String, String), String> = HashMap::new();
    // the directory each tool's first version was found in, for the errors of later versions
    let mut tool_roots: HashMap<String, String> = HashMap::new();
    // each tool version that reuses another item's image, mapped to the item it names
    let mut reuses: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    for (manifest, root) in found {
        let name = manifest.name.clone();
        // names and labels appear in references, so they have to be usable in them
        check_name(&name)
            .map_err(|err| Error::new(format!("The name '{name}' in '{}': {err}", here(&root))))?;
        check_label(&manifest.version).map_err(|err| {
            Error::new(format!(
                "The version of '{name}' in '{}': {err}",
                here(&root)
            ))
        })?;
        // two manifests with one identity would overwrite each other
        let rank = type_rank(&manifest.manifest_type);
        let key = (rank, name.clone(), manifest.version.clone());
        if let Some(first) = places.get(&key) {
            return Err(Error::new(format!(
                "Duplicate manifest '{}' in '{first}' and '{}'; each name and version must be \
                 unique across the toolbox",
                utils::entry_id(None, &name, &manifest.version),
                here(&root)
            )));
        }
        places.insert(key, here(&root));
        match manifest.manifest_type {
            ManifestType::Tool => {
                // remember an image_from reference, resolved once every item is known
                if let Some(from) = &manifest.image_from {
                    reuses.insert(
                        (name.clone(), manifest.version.clone()),
                        (from.name.clone(), from.version.clone()),
                    );
                }
                // build the version, then fold its manifest's root settings into the tool
                let version = tool_version(&manifest, &root, output_dir, tags)?;
                let first_root = tool_roots
                    .entry(name.clone())
                    .or_insert_with(|| here(&root))
                    .clone();
                let tool = items.tools.entry(name).or_default();
                merge_tool_root(tool, &manifest, &root, &first_root)?;
                tool.versions.insert(manifest.version.clone(), version);
            }
            ManifestType::Toolset => {
                let mut toolset: ToolSetManifest = entity_document(&manifest, &root, "toolset")?;
                if !manifest.groups.is_empty() {
                    toolset.groups.clone_from(&manifest.groups);
                }
                toolset.description =
                    entity_description(&manifest, &root, toolset.description.take());
                items.toolsets.insert(name, toolset);
            }
            ManifestType::Agent => {
                let mut agent: AgentManifest = entity_document(&manifest, &root, "agent")?;
                if !manifest.groups.is_empty() {
                    agent.groups.clone_from(&manifest.groups);
                }
                agent.description = entity_description(&manifest, &root, agent.description.take());
                items.agents.insert(name, agent);
            }
            ManifestType::Workflow => {
                let mut workflow: WorkflowManifest = entity_document(&manifest, &root, "workflow")?;
                if !manifest.groups.is_empty() {
                    workflow.groups.clone_from(&manifest.groups);
                }
                workflow.description =
                    entity_description(&manifest, &root, workflow.description.take());
                items.workflows.insert(name, workflow);
            }
            ManifestType::Pipeline => {
                let version = pipeline_version(&manifest, &root, output_dir)?;
                items
                    .pipelines
                    .entry(name)
                    .or_default()
                    .versions
                    .insert(manifest.version.clone(), version);
            }
            ManifestType::Build => {
                let build = base_build(&manifest, &root, output_dir, tags)?;
                items
                    .builds
                    .entry(name)
                    .or_default()
                    .insert(manifest.version.clone(), build);
            }
            // legacy image manifests are built by the legacy path
            ManifestType::Image => {}
        }
    }
    // give every tool that reuses another item's image that image
    resolve_image_from(&mut items, &reuses)?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory removed when dropped
    struct Scratch(
        /// The directory, removed on drop
        PathBuf,
    );

    impl Scratch {
        /// Create an empty scratch directory with a unique name
        fn new() -> Self {
            // a random name keeps parallel tests apart
            let path = std::env::temp_dir().join(format!("thorctl-desc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        /// Write a file in the scratch directory
        ///
        /// # Arguments
        ///
        /// * `name` - The file's name
        /// * `text` - The file's contents
        fn write(&self, name: &str, text: &str) {
            // the file lives directly in the directory
            std::fs::write(self.0.join(name), text).unwrap();
        }
    }

    impl Drop for Scratch {
        /// Remove the scratch directory
        fn drop(&mut self) {
            // a directory that is already gone is fine
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build the items of one manifest found in a scratch directory
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory the manifest and its files are in
    fn build_one(dir: &Scratch) -> Items {
        // parse the manifest the way the walk does
        let text = std::fs::read_to_string(dir.0.join("manifest.toml")).unwrap();
        let manifest: ManifestToml = toml::from_str(&text).unwrap();
        let tags = TagSettings {
            registries: &[],
            use_image_path: false,
            image_path_prefix: None,
            tag_suffix: None,
            global_base_image: None,
        };
        build_items(vec![(manifest, dir.0.clone())], &dir.0, &tags).unwrap()
    }

    /// The description a toolset, agent, or workflow of a scratch directory is built with
    ///
    /// # Arguments
    ///
    /// * `kind` - The manifest type: toolset, agent, or workflow
    /// * `manifest_description` - A `description` line for the manifest, or an empty string
    /// * `document` - The YAML document the manifest points at
    /// * `md` - The `description.md` to write, if any
    fn built_description(
        kind: &str,
        manifest_description: &str,
        document: &str,
        md: Option<&str>,
    ) -> Option<String> {
        // lay out the manifest, its document, and the optional description.md
        let dir = Scratch::new();
        dir.write(
            "manifest.toml",
            &format!(
                "type = \"{kind}\"\nname = \"item\"\n{manifest_description}\nspec_from = \"item.yaml\"\n"
            ),
        );
        dir.write("item.yaml", document);
        if let Some(md) = md {
            dir.write("description.md", md);
        }
        let items = build_one(&dir);
        match kind {
            "toolset" => items.toolsets["item"].description.clone(),
            "agent" => items.agents["item"].description.clone(),
            _ => items.workflows["item"].description.clone(),
        }
    }

    /// A `description.md` is the description of a toolset, agent, or workflow whose manifest and
    /// document have none or a null one
    #[test]
    fn entity_description_md_wins_over_null_and_absent() {
        for kind in ["toolset", "agent", "workflow"] {
            // no description anywhere else
            assert_eq!(
                built_description(kind, "", "{}", Some("from md\n")).as_deref(),
                Some("from md"),
                "{kind}: absent"
            );
            // an explicit null in the document
            assert_eq!(
                built_description(kind, "", "description: null\n", Some("from md\n")).as_deref(),
                Some("from md"),
                "{kind}: null"
            );
            // a description in the manifest or the document is replaced by the file
            assert_eq!(
                built_description(kind, "description = \"manifest\"", "{}", Some("from md"))
                    .as_deref(),
                Some("from md"),
                "{kind}: manifest"
            );
            assert_eq!(
                built_description(kind, "", "description: document\n", Some("from md")).as_deref(),
                Some("from md"),
                "{kind}: document"
            );
        }
    }

    /// Without a usable `description.md` the manifest's description, then the document's, is kept
    #[test]
    fn entity_description_without_md_is_unchanged() {
        for kind in ["toolset", "agent", "workflow"] {
            // the manifest's description replaces the document's
            assert_eq!(
                built_description(
                    kind,
                    "description = \"manifest\"",
                    "description: doc\n",
                    None
                )
                .as_deref(),
                Some("manifest"),
                "{kind}: manifest"
            );
            // the document's description stands alone
            assert_eq!(
                built_description(kind, "", "description: doc\n", None).as_deref(),
                Some("doc"),
                "{kind}: document"
            );
            // an empty description.md is no description.md
            assert_eq!(
                built_description(kind, "", "description: doc\n", Some("  \n")).as_deref(),
                Some("doc"),
                "{kind}: empty md"
            );
            // nothing anywhere is no description
            assert_eq!(
                built_description(kind, "", "{}", None),
                None,
                "{kind}: none"
            );
        }
    }

    /// A `description.md` is a tool's description whether its manifest has no description or an
    /// empty one, and the manifest's is kept when the file is empty or missing
    #[test]
    fn tool_description_md_wins_over_empty_manifest_description() {
        // build a tool with the given manifest line and description.md
        let tool_description = |manifest_description: &str, md: Option<&str>| {
            let dir = Scratch::new();
            dir.write(
                "manifest.toml",
                &format!(
                    "type = \"tool\"\nname = \"scan\"\n{manifest_description}\nspec_from = \"scan.json\"\n"
                ),
            );
            dir.write(
                "scan.json",
                r#"{"Image": {"image": "x:1", "scaler": "K8s"}}"#,
            );
            if let Some(md) = md {
                dir.write("description.md", md);
            }
            build_one(&dir).tools["scan"].description.clone()
        };
        assert_eq!(
            tool_description("", Some("from md\n")).as_deref(),
            Some("from md")
        );
        assert_eq!(
            tool_description("description = \"\"", Some("from md\n")).as_deref(),
            Some("from md")
        );
        assert_eq!(
            tool_description("description = \"manifest\"", Some("from md")).as_deref(),
            Some("from md")
        );
        assert_eq!(
            tool_description("description = \"manifest\"", Some("\n")).as_deref(),
            Some("manifest")
        );
        assert_eq!(
            tool_description("description = \"manifest\"", None).as_deref(),
            Some("manifest")
        );
    }

    /// A pipeline with a config fetched later still carries its `description.md` on the version
    #[test]
    fn pipeline_with_url_config_carries_description_md() {
        // a pipeline whose config is a URL, with a description.md and a null-described config
        let dir = Scratch::new();
        dir.write(
            "manifest.toml",
            "type = \"pipeline\"\nname = \"scan\"\nconfig_from = \"https://example.com/scan.json\"\n",
        );
        dir.write("description.md", "from md\n");
        let items = build_one(&dir);
        assert_eq!(
            items.pipelines["scan"].versions["latest"].description,
            "from md"
        );
    }
}
