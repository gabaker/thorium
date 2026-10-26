//! Rewriting legacy image and pipeline manifests in place as tool, build, and stage-tool manifests
//!
//! The converted spec of each image replaces the image's config file (the JSON file `config_from`
//! names, kept under the same name), its manifest points at that file with `spec_from`, and the
//! manifest keeps everything else it said. A build-only image becomes a `type = "build"` manifest.
//! Nothing is written until every file to write has been checked, so a refusal leaves the
//! directory as it was.

use colored::Colorize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thorium::Error;
use thorium::models::ImageRequest;
use url::Url;

use crate::handlers::progress;
use crate::handlers::toolbox::init::toml_escape;
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;

/// A legacy manifest found while walking a toolbox directory
#[derive(Debug, Clone)]
pub(super) struct LegacyManifest {
    /// The path of the `manifest.toml`
    pub path: PathBuf,
    /// Whether the manifest describes a pipeline rather than an image
    pub is_pipeline: bool,
    /// The name of the image or pipeline
    pub name: String,
    /// The version label of the image or pipeline
    pub version: String,
    /// Whether the image reuses another image's container
    pub image_from: bool,
    /// The config file the image names with `config_from`, which its converted spec replaces
    pub config_from: Option<String>,
}

/// What rewriting one manifest changes
struct Rewrite {
    /// The `manifest.toml` to rewrite
    path: PathBuf,
    /// Where the old manifest is saved first
    backup: PathBuf,
    /// The new text of the manifest
    text: String,
    /// A file beside the manifest to replace, the backup it is saved to first, and its new text
    /// (an image's config file, replaced by its converted spec)
    replaced: Option<Replaced>,
}

/// A file a rewrite replaces
struct Replaced {
    /// The file
    path: PathBuf,
    /// Where the old file is saved first
    backup: PathBuf,
    /// The new text of the file
    text: String,
}

/// Get the key a line of a manifest's top level sets
///
/// # Arguments
///
/// * `line` - The line
fn line_key(line: &str) -> Option<&str> {
    // a key is what stands before the first `=` of a line that is not a comment or a table header
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    trimmed.split_once('=').map(|(key, _)| key.trim())
}

/// Rewrite the text of a legacy image manifest as a tool manifest
///
/// The type becomes `tool` (followed by the tool's kind and groups, a description when no
/// `description.md` holds it, and a build path when a built image names none) and `config_from`
/// becomes `spec_from`, naming the same file; every other line stays as it was.
///
/// # Arguments
///
/// * `source` - The manifest's text
/// * `groups` - The groups the converted tool is in
/// * `description` - The description to carry over when no `description.md` holds it
/// * `reuses` - Whether the image reuses another item's image (`image_from`), so nothing is built
fn rewrite_image(
    source: &str,
    groups: &[String],
    description: Option<&str>,
    reuses: bool,
) -> String {
    // the top level is everything before the first table header
    let header_len = source
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .count();
    let has_build_path = source
        .lines()
        .take(header_len)
        .any(|line| line_key(line) == Some("build_path"));
    // the lines every converted manifest gets, after its type
    let quoted_groups: Vec<String> = groups
        .iter()
        .map(|group| format!("\"{}\"", toml_escape(group)))
        .collect();
    let mut added = vec![
        "kind = \"image\"".to_owned(),
        format!("groups = [{}]", quoted_groups.join(", ")),
    ];
    if let Some(description) = description {
        added.push(format!("description = \"{}\"", toml_escape(description)));
    }
    // a built image is built from its own directory unless it says otherwise, and a tool only
    // has a build section when its manifest names a build path
    if !has_build_path && !reuses {
        added.push("build_path = \"./\"".to_owned());
    }
    let mut lines: Vec<String> = Vec::new();
    for (index, line) in source.lines().enumerate() {
        match (index < header_len, line_key(line)) {
            // the type becomes `tool`, followed by what the spec and the groups need
            (true, Some("type")) => {
                lines.push("type = \"tool\"".to_owned());
                lines.extend(added.iter().cloned());
            }
            // the config file now holds the spec, under the key a tool reads it from
            (true, Some("config_from")) => {
                let indent = &line[..line.len() - line.trim_start().len()];
                let rest = line.trim_start().trim_start_matches("config_from");
                lines.push(format!("{indent}spec_from{rest}"));
            }
            _ => lines.push(line.to_owned()),
        }
    }
    format!("{}\n", lines.join("\n"))
}

/// Rewrite the text of a build-only legacy image manifest as a base build manifest
///
/// # Arguments
///
/// * `source` - The manifest's text
fn rewrite_build(source: &str) -> String {
    // the top level is everything before the first table header
    let header_len = source
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .count();
    let mut lines: Vec<String> = Vec::new();
    for (index, line) in source.lines().enumerate() {
        match (index < header_len, line_key(line)) {
            // the type becomes `build`
            (true, Some("type")) => lines.push("type = \"build\"".to_owned()),
            // an empty config has no place in a base build
            (true, Some("config_from")) => {}
            _ => lines.push(line.to_owned()),
        }
    }
    format!("{}\n", lines.join("\n"))
}

/// Render a spec as the JSON a tool's spec file holds
///
/// # Arguments
///
/// * `spec` - The spec
pub(in crate::handlers::toolbox) fn spec_json(spec: &serde_json::Value) -> Result<String, Error> {
    // two-space pretty JSON with sorted keys, ending in a newline like the other JSON files
    serde_json::to_string_pretty(spec)
        .map(|text| format!("{text}\n"))
        .map_err(|err| Error::new(format!("Failed to write a spec as JSON: {err}")))
}

/// Find the config file of a legacy image that its converted spec replaces
///
/// The file has to be a local file beside or below the manifest that still holds the image's
/// legacy config, so a spec never overwrites anything else.
///
/// # Arguments
///
/// * `legacy` - The legacy image manifest
/// * `dir` - The directory the manifest is in
fn config_file(legacy: &LegacyManifest, dir: &Path) -> Result<PathBuf, String> {
    let manifest = legacy.path.display();
    let config_from = legacy.config_from.as_deref().ok_or_else(|| {
        format!("{manifest}: names no config file (config_from) to hold its spec")
    })?;
    // a URL or a path that leaves the manifest's directory is not a file the rewrite may replace
    let relative = Path::new(config_from);
    if Url::parse(config_from).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!(
            "{manifest}: its config_from '{config_from}' is not a file beside the manifest"
        ));
    }
    let path = dir.join(relative);
    // the file must still be the image's legacy config
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("{manifest}: failed to read '{}': {err}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|err| {
        format!(
            "{manifest}: '{}' is not JSON ({err}), so it is not the image's config",
            path.display()
        )
    })?;
    if serde_json::from_value::<ImageRequest>(value).is_err() {
        return Err(format!(
            "{manifest}: '{}' is not an image config, so it is not replaced",
            path.display()
        ));
    }
    Ok(path)
}

/// Rewrite the text of a legacy pipeline manifest to use stage tools
///
/// # Arguments
///
/// * `source` - The manifest's text
fn rewrite_pipeline(source: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in source.lines() {
        // an `[images.NAME]` table becomes a `[stage_tools.NAME]` table naming the tool of that name
        let trimmed = line.trim();
        match trimmed
            .strip_prefix("[images.")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            Some(key) => {
                lines.push(format!("[stage_tools.{key}]"));
                lines.push(format!("tool = \"{}\"", key.trim_matches('"')));
            }
            None => lines.push(line.to_owned()),
        }
    }
    format!("{}\n", lines.join("\n"))
}

/// Get the path a manifest's backup is saved to
///
/// # Arguments
///
/// * `path` - The manifest's path
fn backup_path(path: &Path) -> PathBuf {
    // `.bak` is added to the whole file name
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Plan the rewrite of every legacy manifest that became a tool, a base build, or a stage-tool
/// pipeline
///
/// # Arguments
///
/// * `found` - The legacy manifests found
/// * `manifest` - The converted toolbox
fn plan(found: &[LegacyManifest], manifest: &ToolboxManifestV2) -> Result<Vec<Rewrite>, Error> {
    let mut found: Vec<&LegacyManifest> = found.iter().collect();
    found.sort_by(|left, right| left.path.cmp(&right.path));
    let mut rewrites = Vec::new();
    // config files that can't be replaced, and the ones already claimed by another manifest
    let mut refusals: Vec<String> = Vec::new();
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    for legacy in found {
        let dir = legacy.path.parent().unwrap_or_else(|| Path::new("."));
        let source = std::fs::read_to_string(&legacy.path).map_err(|err| {
            Error::new(format!("Failed to read '{}': {err}", legacy.path.display()))
        })?;
        if legacy.is_pipeline {
            // a pipeline only moves to stage tools when the conversion kept it
            let kept = manifest
                .pipelines
                .get(&legacy.name)
                .is_some_and(|pipeline| pipeline.versions.contains_key(&legacy.version));
            if !kept {
                progress::warn(format!(
                    "{}: not rewritten, because the conversion left the pipeline out",
                    legacy.path.display()
                ));
                continue;
            }
            rewrites.push(Rewrite {
                path: legacy.path.clone(),
                backup: backup_path(&legacy.path),
                text: rewrite_pipeline(&source),
                replaced: None,
            });
            continue;
        }
        // an image that became a tool gets its spec; a build-only context becomes a base build
        let tool = manifest.tools.get(&legacy.name).and_then(|tool| {
            tool.versions
                .get(&legacy.version)
                .map(|version| (tool, version))
        });
        let Some((tool, version)) = tool else {
            let is_build = manifest
                .builds
                .get(&legacy.name)
                .is_some_and(|labels| labels.contains_key(&legacy.version));
            if is_build {
                rewrites.push(Rewrite {
                    path: legacy.path.clone(),
                    backup: backup_path(&legacy.path),
                    text: rewrite_build(&source),
                    replaced: None,
                });
            }
            continue;
        };
        let Some(spec) = version.spec.as_ref() else {
            continue;
        };
        // the spec replaces the image's own config file, which must be one it may replace
        let path = match config_file(legacy, dir) {
            Ok(path) => path,
            Err(reason) => {
                refusals.push(reason);
                continue;
            }
        };
        if !claimed.insert(path.clone()) {
            refusals.push(format!(
                "{}: its config file '{}' is the config of another image too",
                legacy.path.display(),
                path.display()
            ));
            continue;
        }
        // an image reusing another's container leaves its spec's image empty, to be filled in
        // with the reused image at every build
        let mut spec = spec.clone();
        if legacy.image_from {
            clear_spec_image(&mut spec);
        }
        // a description.md keeps carrying the description; otherwise the manifest takes it
        let description = if dir.join("description.md").is_file() {
            None
        } else {
            tool.description.as_deref()
        };
        rewrites.push(Rewrite {
            path: legacy.path.clone(),
            backup: backup_path(&legacy.path),
            text: rewrite_image(&source, &tool.groups, description, legacy.image_from),
            replaced: Some(Replaced {
                backup: backup_path(&path),
                path,
                text: spec_json(&spec)?,
            }),
        });
    }
    if !refusals.is_empty() {
        return Err(Error::new(format!(
            "Failed to rewrite the manifests, and nothing was changed:\n  {}",
            refusals.join("\n  ")
        )));
    }
    Ok(rewrites)
}

/// Empty the container image a spec names
///
/// # Arguments
///
/// * `spec` - The spec
fn clear_spec_image(spec: &mut serde_json::Value) {
    // an image spec holds its environment at the top, a harness spec in its workload
    let environment = if spec.get("Image").is_some() {
        spec.get_mut("Image")
            .and_then(|image| image.get_mut("environment"))
    } else {
        spec.get_mut("Harness")
            .and_then(|harness| harness.get_mut("workload"))
            .and_then(|workload| workload.get_mut("environment"))
    };
    if let Some(environment) = environment {
        for scaler in ["K8s", "Windows"] {
            if let Some(slot) = environment
                .get_mut(scaler)
                .and_then(|environment| environment.get_mut("image"))
            {
                *slot = serde_json::Value::String(String::new());
            }
        }
    }
}

/// Rewrite the legacy manifests of a toolbox directory as tool, build, and stage-tool manifests
///
/// # Arguments
///
/// * `found` - The legacy manifests found while walking the directory
/// * `manifest` - The converted toolbox, which holds the spec of each image
pub(super) fn rewrite_manifests(
    found: &[LegacyManifest],
    manifest: &ToolboxManifestV2,
) -> Result<(), Error> {
    let rewrites = plan(found, manifest)?;
    // refuse before writing anything when a backup is already there
    let mut refusals = Vec::new();
    for rewrite in &rewrites {
        let backups = std::iter::once(&rewrite.backup)
            .chain(rewrite.replaced.as_ref().map(|replaced| &replaced.backup));
        for backup in backups {
            if backup.exists() {
                refusals.push(format!(
                    "'{}' already exists; move it aside first",
                    backup.display()
                ));
            }
        }
    }
    if !refusals.is_empty() {
        return Err(Error::new(format!(
            "Failed to rewrite the manifests, and nothing was changed:\n  {}",
            refusals.join("\n  ")
        )));
    }
    // save each old manifest and config file, then write the new manifest and the spec
    for rewrite in &rewrites {
        std::fs::copy(&rewrite.path, &rewrite.backup).map_err(|err| {
            Error::new(format!(
                "Failed to save '{}': {err}",
                rewrite.backup.display()
            ))
        })?;
        std::fs::write(&rewrite.path, &rewrite.text).map_err(|err| {
            Error::new(format!(
                "Failed to write '{}': {err}",
                rewrite.path.display()
            ))
        })?;
        if let Some(replaced) = &rewrite.replaced {
            std::fs::copy(&replaced.path, &replaced.backup).map_err(|err| {
                Error::new(format!(
                    "Failed to save '{}': {err}",
                    replaced.backup.display()
                ))
            })?;
            std::fs::write(&replaced.path, &replaced.text).map_err(|err| {
                Error::new(format!(
                    "Failed to write '{}': {err}",
                    replaced.path.display()
                ))
            })?;
        }
        println!(
            "{} {} (old manifest kept as {})",
            "Rewrote".bright_green(),
            rewrite.path.display(),
            rewrite.backup.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image manifest becomes a tool manifest whose spec is the file its config was in
    #[test]
    fn images_become_tools() {
        let source = "name = \"scan\"\ntype = \"image\"\nconfig_from = \"scan.json\"\nimage_name = \"x\"\n\n[base_image]\nallow_override = false\n";
        let text = rewrite_image(source, &["static".to_owned()], Some("A \"tool\""), false);
        assert_eq!(
            text,
            "name = \"scan\"\ntype = \"tool\"\nkind = \"image\"\ngroups = [\"static\"]\n\
             description = \"A \\\"tool\\\"\"\nbuild_path = \"./\"\nspec_from = \"scan.json\"\n\
             image_name = \"x\"\n\n[base_image]\nallow_override = false\n"
        );
    }

    /// An image manifest that already names a build path keeps it and gains no second one
    #[test]
    fn images_keep_their_build_path() {
        let source = "name = \"scan\"\ntype = \"image\"\nbuild_path = \"ctx\"\n";
        let text = rewrite_image(source, &["g".to_owned()], None, false);
        assert_eq!(text.matches("build_path").count(), 1, "{text}");
        assert!(!text.contains("description"), "{text}");
    }

    /// An image reusing another's container becomes a tool that keeps its `image_from` table and
    /// gains no build path
    #[test]
    fn image_from_images_become_tools_without_a_build() {
        let source = "name = \"s16\"\ntype = \"image\"\nconfig_from = \"s16.json\"\n\n[image_from]\nname = \"strings\"\n";
        assert_eq!(
            rewrite_image(source, &["static".to_owned()], None, true),
            "name = \"s16\"\ntype = \"tool\"\nkind = \"image\"\ngroups = [\"static\"]\n\
             spec_from = \"s16.json\"\n\n[image_from]\nname = \"strings\"\n"
        );
    }

    /// A build-only image manifest becomes a base build manifest and keeps every other key
    #[test]
    fn build_only_images_become_builds() {
        let source = "name = \"base\"\ntype = \"image\"\nbuild_path = \"./\"\nimage_name = \"tools/base\"\nversion = \"1.0\"\n\n[base_image]\nallow_override = true\n";
        assert_eq!(
            rewrite_build(source),
            "name = \"base\"\ntype = \"build\"\nbuild_path = \"./\"\nimage_name = \"tools/base\"\n\
             version = \"1.0\"\n\n[base_image]\nallow_override = true\n"
        );
    }

    /// A spec is written as two-space JSON with sorted keys and a trailing newline
    #[test]
    fn specs_are_written_as_json() {
        let spec =
            serde_json::json!({"Image": {"environment": {"K8s": {"image": ""}}, "data": {}}});
        assert_eq!(
            spec_json(&spec).unwrap(),
            "{\n  \"Image\": {\n    \"data\": {},\n    \"environment\": {\n      \"K8s\": {\n        \"image\": \"\"\n      }\n    }\n  }\n}\n"
        );
    }

    /// Only a local config file that still holds an image config may be replaced by a spec
    #[test]
    fn only_a_local_image_config_is_replaced() {
        let dir = std::env::temp_dir().join(format!("thorctl-rewrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = |config_from: Option<&str>| LegacyManifest {
            path: dir.join("manifest.toml"),
            is_pipeline: false,
            name: "scan".to_owned(),
            version: "latest".to_owned(),
            image_from: false,
            config_from: config_from.map(str::to_owned),
        };
        std::fs::write(
            dir.join("scan.json"),
            r#"{"group":"g","name":"scan","scaler":"K8s","image":""}"#,
        )
        .unwrap();
        std::fs::write(dir.join("spec.json"), r#"{"Image": {"environment": {}}}"#).unwrap();
        assert_eq!(
            config_file(&legacy(Some("scan.json")), &dir).unwrap(),
            dir.join("scan.json")
        );
        for config_from in [
            None,
            Some("spec.json"),
            Some("missing.json"),
            Some("../scan.json"),
            Some("https://x.test/scan.json"),
        ] {
            assert!(
                config_file(&legacy(config_from), &dir).is_err(),
                "{config_from:?}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A pipeline's image tables become stage-tool tables naming the tool of the same name
    #[test]
    fn pipelines_use_stage_tools() {
        let source = "name = \"p\"\ntype = \"pipeline\"\n\n[images.a]\nversion = \"1\"\n\n[images.\"b.c\"]\nversion = \"2\"\n";
        assert_eq!(
            rewrite_pipeline(source),
            "name = \"p\"\ntype = \"pipeline\"\n\n[stage_tools.a]\ntool = \"a\"\nversion = \"1\"\n\n\
             [stage_tools.\"b.c\"]\ntool = \"b.c\"\nversion = \"2\"\n"
        );
    }
}
