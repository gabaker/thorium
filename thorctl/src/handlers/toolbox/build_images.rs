//! Build (and optionally push) the container images described by a toolbox
//!
//! This is the local stand-in for the CI pipeline that normally builds a
//! toolbox repo's images: walk `toolbox.json`, build every entry with build
//! enabled from its build context, alias the extra registry tags, and push.
//! With the images in a registry, `toolbox import` works as usual.

use colored::Colorize;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thorium::Error;

use crate::args::toolbox::BuildImagesToolbox;
use crate::handlers::container;

use super::build::{BaseImage, DEFAULT_BASE_IMAGE_ARG};

/// The slice of a toolbox.json image entry that drives a build
///
/// Deserialized standalone (not via [`super::manifest::ToolboxManifest`]) so
/// building doesn't require resolvable configs.
#[derive(Deserialize)]
struct BuildEntry {
    /// The build context to build from
    build_path: PathBuf,
    /// Whether this image should be built at all
    #[serde(default = "default_true")]
    build_image: bool,
    /// Every registry tag this image should carry
    #[serde(default)]
    image_tags: Vec<String>,
    /// This image's resolved base-image configuration (per-tool merged over global at build time)
    #[serde(default)]
    base_image: Option<BaseImage>,
}

/// Resolve the effective base-image build-arg `(arg, image)` for one image
///
/// The entry's `base_image` is already the per-tool-over-global merge done at `toolbox build` time,
/// so this only layers the `--base-image` CLI escape hatch on top and applies the `allow_override`
/// gate. `token`/`user` are deliberately not consulted — `build-images` performs no base-registry
/// login.
///
/// # Arguments
///
/// * `cli` - The `--base-image ARG=IMAGE` override, if given
/// * `entry` - This image's resolved `base_image` from `toolbox.json`, if any
fn resolve_base_build_arg(
    cli: Option<&(String, String)>,
    entry: Option<&BaseImage>,
) -> Option<(String, String)> {
    // an image can opt out of the image/image_arg substitution (default is to allow it)
    let allow = entry.and_then(|base| base.allow_override).unwrap_or(true);
    if !allow {
        return None;
    }
    // the CLI flag is the operator escape hatch and wins outright, carrying its own arg
    if let Some((arg, image)) = cli {
        return Some((arg.clone(), image.clone()));
    }
    // otherwise use the entry's resolved image with its arg (defaulted at build, but guard here too)
    let base = entry?;
    let image = base.image.clone()?;
    let arg = base
        .image_arg
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_IMAGE_ARG.to_string());
    Some((arg, image))
}

/// Merge the generic `--build-arg` set with the resolved base-image override
///
/// The base override (already resolved by [`resolve_base_build_arg`]) is more specific than a
/// generic build arg, so it replaces any generic arg sharing its key.
///
/// # Arguments
///
/// * `generic` - The generic `--build-arg KEY=VALUE` pairs applied to every build
/// * `base_override` - The resolved `(arg, image)` override for this image, if any
fn resolve_build_args(
    generic: &[(String, String)],
    base_override: Option<&(String, String)>,
) -> Vec<(String, String)> {
    let mut args = generic.to_vec();
    if let Some((key, value)) = base_override {
        // the override wins on a key collision, so drop any generic arg with this key
        args.retain(|(existing, _)| existing != key);
        args.push((key.clone(), value.clone()));
    }
    args
}

/// The default for manifest booleans that should be on unless explicitly disabled
fn default_true() -> bool {
    true
}

/// Append a tag suffix to an image tag, e.g. `reg/x:1.0` + `-mybranch` -> `reg/x:1.0-mybranch`
///
/// The suffix lands on the version (the last component of the tag), differentiating a
/// feature-branch build from the mainline tag. An absent or empty suffix leaves the tag
/// unchanged.
///
/// # Arguments
///
/// * `tag` - The image tag to suffix
/// * `suffix` - The suffix to append, if any
fn apply_tag_suffix(tag: &str, suffix: Option<&str>) -> String {
    match suffix {
        Some(suffix) if !suffix.is_empty() => format!("{tag}{suffix}"),
        _ => tag.to_string(),
    }
}

/// Only the parts of toolbox.json that builds care about
#[derive(Deserialize)]
struct BuildManifest {
    /// The image entries mapped name -> version -> entry (each carries its resolved `base_image`)
    images: HashMap<String, HashMap<String, BuildEntry>>,
}

/// Build and optionally push every buildable image in a toolbox
///
/// # Arguments
///
/// * `cmd` - The build-images command that was run
pub async fn build_images(cmd: &BuildImagesToolbox) -> Result<(), Error> {
    let manifest_str = tokio::fs::read_to_string(&cmd.manifest)
        .await
        .map_err(|err| {
            Error::new(format!(
                "Failed to read toolbox manifest '{}': {err}",
                cmd.manifest.display()
            ))
        })?;
    let manifest: BuildManifest = serde_json::from_str(&manifest_str).map_err(|err| {
        Error::new(format!(
            "Failed to parse toolbox manifest '{}': {err}",
            cmd.manifest.display()
        ))
    })?;
    // build_path values in toolbox.json are recorded relative to the manifest's own
    // directory, so resolve them against it rather than the cwd build-images runs from
    let manifest_dir = cmd.manifest.parent().unwrap_or_else(|| Path::new("."));
    // collect the entries to build, sorted for deterministic order
    let mut targets: Vec<(String, String, BuildEntry)> = manifest
        .images
        .into_iter()
        // an explicit --image list narrows the build set
        .filter(|(name, _)| cmd.images.is_empty() || cmd.images.contains(name))
        .flat_map(|(name, versions)| {
            versions
                .into_iter()
                .map(move |(version, entry)| (name.clone(), version, entry))
        })
        .collect();
    targets.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    if targets.is_empty() {
        return Err(Error::new(
            "No matching image entries found in the toolbox manifest",
        ));
    }
    // warn once if the base image arg key also appears in the generic build args; the
    // base image value is more specific and wins for images that allow base overrides
    if let Some((base_key, _)) = &cmd.base_image
        && cmd.build_args.iter().any(|(key, _)| key == base_key)
    {
        eprintln!(
            "{} build arg '{base_key}' is set by both --base-image and --build-arg; \
             the --base-image value takes precedence where base overrides are allowed",
            "Warning:".bright_yellow()
        );
    }
    // the build-behavior flags (--no-cache / --pull) apply uniformly to every image
    let build_opts = container::BuildOptions {
        no_cache: cmd.no_cache,
        pull: cmd.pull,
    };
    let mut built = 0usize;
    // labels of images whose build/push failed (collected unless --exit-on-error)
    let mut failures: Vec<String> = Vec::new();
    for (name, version, entry) in targets {
        let label = format!("{name}:{version}");
        // entries can opt out of building (prebuilt images in a registry)
        if !entry.build_image {
            println!("{} {label} (build disabled)", "Skipping".bright_yellow());
            continue;
        }
        // a tag is required to anchor the build
        if entry.image_tags.is_empty() {
            println!("{} {label} (no image tags)", "Skipping".bright_yellow());
            continue;
        }
        // apply the tag suffix so a feature-branch run builds/pushes differentiated tags
        // (e.g. `:1.0-mybranch`) without colliding with the mainline tag in toolbox.json
        let tags: Vec<String> = entry
            .image_tags
            .iter()
            .map(|tag| apply_tag_suffix(tag, cmd.tag_suffix.as_deref()))
            .collect();
        // the image's allow_override gate (default true when unset)
        let allow_override = entry
            .base_image
            .as_ref()
            .and_then(|base| base.allow_override)
            .unwrap_or(true);
        // resolve the effective base-image build-arg: CLI escape hatch over the entry's resolved
        // base_image, gated by allow_override
        let base_override = resolve_base_build_arg(cmd.base_image.as_ref(), entry.base_image.as_ref());
        // note when a requested override is withheld because the image opts out
        let override_requested =
            cmd.base_image.is_some() || entry.base_image.as_ref().is_some_and(|b| b.image.is_some());
        if base_override.is_none() && override_requested && !allow_override {
            println!(
                "{} base image override for {label} (allow_override = false)",
                "Skipping".bright_yellow()
            );
        }
        // note which base image won when one applies
        if let Some((arg, image)) = &base_override {
            println!(
                "{} base image '{image}' (build-arg {arg}) for {label}",
                "Overriding".bright_cyan()
            );
        }
        // token/user are pass-through for an external CI/CD pipeline; build-images does no
        // base-registry login, so remind the operator to handle it themselves
        if let Some(base) = &entry.base_image
            && (base.token.is_some() || base.user.is_some())
        {
            println!(
                "{} base image token/user for {label} are not used by build-images; run \
                 docker/podman login yourself if the base image needs auth",
                "Note:".bright_yellow()
            );
        }
        // build args for this image: the generic --build-arg set, plus the resolved base
        // override (which replaces a generic arg with the same key)
        let build_args = resolve_build_args(&cmd.build_args, base_override.as_ref());
        // resolve the build context against the manifest's directory (toolbox.json is
        // the source of truth for paths); an absolute build_path is kept as-is
        let context = manifest_dir.join(&entry.build_path);
        println!(
            "{} {label} from '{}'",
            "Building".bright_green(),
            context.display()
        );
        // the runtime's output streams straight to the terminal so progress is visible
        match build_one(&context, &tags, cmd.push, &build_args, build_opts).await {
            Ok(()) => {
                println!(
                    "{} {label} ({} tag{}{})",
                    "Built".bright_green(),
                    tags.len(),
                    if tags.len() == 1 { "" } else { "s" },
                    if cmd.push { ", pushed" } else { "" },
                );
                built += 1;
            }
            // --exit-on-error stops the whole run at the first failure
            Err(err) if cmd.exit_on_error => {
                return Err(Error::new(format!("Failed to build '{label}': {err}")));
            }
            // by default, log the failure and move on to the next image
            Err(err) => {
                eprintln!("{} {label}: {err}", "Failed".bright_red());
                failures.push(label);
            }
        }
    }
    println!(
        "\n{} {built} image{} built{}",
        "Done!".bright_green(),
        if built == 1 { "" } else { "s" },
        if cmd.push { " and pushed" } else { "" },
    );
    // surface the collected failures and exit non-zero so the run isn't silently green
    if !failures.is_empty() {
        eprintln!(
            "{} {} image{} failed: {}",
            "Errors:".bright_red(),
            failures.len(),
            if failures.len() == 1 { "" } else { "s" },
            failures.join(", "),
        );
        return Err(Error::new(format!(
            "{} image(s) failed to build",
            failures.len()
        )));
    }
    Ok(())
}

/// Build an image's primary tag, alias its extra tags, and optionally push them,
/// streaming the runtime's output to the terminal
///
/// # Arguments
///
/// * `context` - The build context, already resolved against the manifest directory
/// * `tags` - The image tags to build/alias/push (first is the primary build tag);
///   must be non-empty
/// * `push` - Whether to push every tag after building
/// * `build_args` - The `(key, value)` build args to pass to the build
/// * `opts` - The build-behavior flags (`--no-cache` / `--pull`)
async fn build_one(
    context: &Path,
    tags: &[String],
    push: bool,
    build_args: &[(String, String)],
    opts: container::BuildOptions,
) -> Result<(), Error> {
    // the caller guarantees at least one tag (it skips empty-tag entries)
    let primary_tag = &tags[0];
    // build once with the primary tag, then alias the rest; a single build covers
    // every registry the toolbox is configured for
    container::build_streamed(primary_tag, context, build_args, opts).await?;
    for extra_tag in tags.iter().skip(1) {
        container::tag_streamed(primary_tag, extra_tag).await?;
    }
    if push {
        for tag in tags {
            container::push_streamed(tag).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `(key, value)` pair as owned strings, for terse test fixtures
    fn arg(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    /// A resolved `BaseImage` entry from an image, an optional arg, and an allow_override flag,
    /// for terse test fixtures
    fn entry(image: Option<&str>, image_arg: Option<&str>, allow_override: Option<bool>) -> BaseImage {
        BaseImage {
            image: image.map(str::to_string),
            image_arg: image_arg.map(str::to_string),
            token: None,
            user: None,
            allow_override,
        }
    }

    /// With no base image override, the generic build args pass through unchanged
    #[test]
    fn resolve_keeps_generic_without_override() {
        let generic = vec![arg("HTTP_PROXY", "p"), arg("VERSION", "1")];
        let resolved = resolve_build_args(&generic, None);
        assert_eq!(resolved, generic);
    }

    /// A resolved override is appended to the generic args
    #[test]
    fn resolve_appends_override() {
        let generic = vec![arg("VERSION", "1")];
        let base = arg("IMAGE", "ubuntu:22.04");
        let resolved = resolve_build_args(&generic, Some(&base));
        assert_eq!(resolved, vec![arg("VERSION", "1"), arg("IMAGE", "ubuntu:22.04")]);
    }

    /// An image that opts out (allow_override = false) resolves to no override, even with a CLI flag
    #[test]
    fn base_build_arg_none_when_disallowed() {
        let cli = arg("IMAGE", "from-cli");
        let entry = entry(Some("from-tool"), Some("IMAGE"), Some(false));
        assert_eq!(resolve_base_build_arg(Some(&cli), Some(&entry)), None);
    }

    /// The CLI flag wins over the entry's resolved base image
    #[test]
    fn base_build_arg_cli_wins() {
        let cli = arg("IMAGE", "from-cli");
        let entry = entry(Some("from-tool"), Some("TOOL_ARG"), None);
        assert_eq!(
            resolve_base_build_arg(Some(&cli), Some(&entry)),
            Some(arg("IMAGE", "from-cli"))
        );
    }

    /// The entry's resolved image is used with its arg when no CLI flag is given
    #[test]
    fn base_build_arg_uses_entry() {
        let entry = entry(Some("from-tool"), Some("TOOL_ARG"), None);
        assert_eq!(
            resolve_base_build_arg(None, Some(&entry)),
            Some(arg("TOOL_ARG", "from-tool"))
        );
    }

    /// An entry image with no arg falls back to the built-in default
    #[test]
    fn base_build_arg_defaults_arg() {
        let entry = entry(Some("from-tool"), None, None);
        assert_eq!(
            resolve_base_build_arg(None, Some(&entry)),
            Some((DEFAULT_BASE_IMAGE_ARG.to_string(), "from-tool".to_string()))
        );
    }

    /// No entry and no CLI resolves to no override
    #[test]
    fn base_build_arg_none_when_unset() {
        assert_eq!(resolve_base_build_arg(None, None), None);
        // an entry with token/user but no image also yields no build-arg substitution
        let token_only = BaseImage {
            image: None,
            image_arg: None,
            token: Some("TOK".to_string()),
            user: Some("USR".to_string()),
            allow_override: None,
        };
        assert_eq!(resolve_base_build_arg(None, Some(&token_only)), None);
    }

    /// A non-empty suffix lands on the tag's version; absent/empty leaves it unchanged
    #[test]
    fn apply_tag_suffix_appends_to_version() {
        assert_eq!(apply_tag_suffix("reg/x:1.0", Some("-mybranch")), "reg/x:1.0-mybranch");
        assert_eq!(apply_tag_suffix("reg/x:1.0", None), "reg/x:1.0");
        assert_eq!(apply_tag_suffix("reg/x:1.0", Some("")), "reg/x:1.0");
    }

    /// On a key collision the override replaces the generic arg rather than producing a
    /// duplicate key; a different key coexists
    #[test]
    fn resolve_override_replaces_generic_on_collision() {
        let generic = vec![arg("IMAGE", "from-build-arg"), arg("VERSION", "1")];
        let base = arg("IMAGE", "from-base-image");
        let resolved = resolve_build_args(&generic, Some(&base));
        // exactly one IMAGE entry, and it carries the override value
        let images: Vec<&(String, String)> =
            resolved.iter().filter(|(key, _)| key == "IMAGE").collect();
        assert_eq!(images, vec![&arg("IMAGE", "from-base-image")]);
        // a build arg with a different key is left in place
        assert!(resolved.contains(&arg("VERSION", "1")));
    }
}
