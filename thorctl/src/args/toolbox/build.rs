//! Arguments for `thorctl toolbox build` and `thorctl toolbox build-images`

use clap::Parser;
use std::path::PathBuf;

/// Parse a `KEY=VALUE` build arg, splitting on the first `=`
///
/// Used by `--base-image` and `--build-arg`. The key must be non-empty; the value may
/// be empty and may itself contain `=` (only the first `=` is the separator), so image
/// references and values with `=` pass through intact.
///
/// # Arguments
///
/// * `raw` - The raw `KEY=VALUE` argument string
fn parse_build_arg(raw: &str) -> Result<(String, String), String> {
    // split on the first '=' so a value that itself contains '=' (or a registry ref with
    // ':' and '/') is preserved whole; a missing '=' has no key/value boundary and errors
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got '{raw}'"))?;
    // reject an empty key (a leading '='); a nameless build arg can't be applied to a build
    if key.is_empty() {
        return Err(format!("build arg key must not be empty in '{raw}'"));
    }
    // own both halves so the parsed pair outlives the borrowed input string
    Ok((key.to_string(), value.to_string()))
}

/// Build the container images described by a toolbox manifest
#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct BuildImagesToolbox {
    /// The toolbox.json holding the build entries, or the directory holding it
    ///
    /// Reads either format: a v2 toolbox builds every tool version with a `build` section plus
    /// the `builds` section (base contexts other builds start from, built first), and a legacy
    /// toolbox builds its `images` entries. A directory is searched for `toolbox.json`, then
    /// `toolbox.v2.json`.
    #[clap(value_name = "TOOLBOX.JSON", default_value = "toolbox.json")]
    pub manifest: PathBuf,
    /// Only build these tools and base builds, comma-separated. Format: `name[,name,...]`
    /// (default: everything with build enabled)
    ///
    /// `-i/--images` is a hidden alias of this flag for older scripts.
    #[clap(
        short = 't',
        long = "tools",
        alias = "images",
        short_alias = 'i',
        value_name = "NAME",
        value_delimiter = ','
    )]
    pub tools: Vec<String>,
    /// Print what would be built, tagged, and pushed without running the container runtime
    ///
    /// Lists the entries in build order with their tags, build arguments, and the entries that
    /// are skipped, then exits.
    #[clap(long)]
    pub dry_run: bool,
    /// Push every built tag to its registry after building
    #[clap(long)]
    pub push: bool,
    /// Override an image's base image: `ARG=IMAGE`, passed to the build as
    /// `--build-arg ARG=IMAGE`
    ///
    /// Run-global escape hatch that overrides each image's resolved `[base_image]`. Only
    /// applied to images whose `[base_image].allow_override` is true (the default); images
    /// that opt out are built with their default base. `ARG` is the build arg the
    /// Dockerfile reads (e.g. `IMAGE` for `FROM ${IMAGE}`). On a key collision with a
    /// `--build-arg`, this value wins for images that allow the override.
    #[clap(long, value_parser = parse_build_arg, value_name = "ARG=IMAGE")]
    pub base_image: Option<(String, String)>,
    /// Extra build arg passed to every image build: `KEY=VALUE` (repeatable)
    ///
    /// Unlike `--base-image`, these are passed to every build regardless of each entry's
    /// `base_image.allow_override` gate.
    #[clap(long = "build-arg", value_parser = parse_build_arg, value_name = "KEY=VALUE")]
    pub build_args: Vec<(String, String)>,
    /// Append this suffix to every tag built and pushed, without touching toolbox.json
    ///
    /// Lets a CI/CD feature-branch run build/push differentiated images (e.g.
    /// `:1.0-mybranch`) from an unmodified toolbox.json so they don't collide with the
    /// mainline `:1.0`. Pass the separator you want (e.g. `-mybranch`).
    ///
    /// This is the alternative to `toolbox build --tag-suffix`, which bakes the suffix into
    /// toolbox.json instead; use one or the other. A tag that already ends with the suffix is
    /// not suffixed again.
    // allow_hyphen_values so a leading-dash suffix like `-mybranch` is taken as the
    // value rather than parsed as another flag
    #[clap(long, value_name = "SUFFIX", allow_hyphen_values = true)]
    pub tag_suffix: Option<String>,
    /// Stop at the first image whose build or push fails
    ///
    /// By default the build keeps going on failure — logging each one and exiting
    /// non-zero at the end if any image failed. This flag instead aborts the run
    /// on the first failure.
    #[clap(short = 'e', long = "exit-on-error")]
    pub exit_on_error: bool,
    /// Build without the layer cache (passes `--no-cache` to docker/podman)
    ///
    /// Applies to every image built in this run.
    #[clap(long)]
    pub no_cache: bool,
    /// Force a fresh pull of referenced images before building (passes `--pull` to
    /// docker/podman)
    ///
    /// Like docker/podman `--pull`, this refreshes every image the build references
    /// (including the `FROM` base) instead of using a locally cached one. Applies to
    /// every image built in this run.
    #[clap(long)]
    pub pull: bool,
}

/// Build a toolbox manifest from tool, image, and pipeline manifests
#[derive(Parser, Debug, Clone)]
pub struct BuildToolbox {
    /// Path to the toolbox TOML config file
    #[clap(
        short = 'c',
        long = "config",
        value_name = "CONFIG.TOML",
        default_value = "config.toml"
    )]
    pub config: PathBuf,
    /// Tag every image with its manifest `image_name` (a repo-style path) as the leaf:
    /// `<registry>/[prefix/]<image_name>:<version>`
    ///
    /// By default images are tagged with the tool `name`
    /// (`<registry>/[prefix/]<name>:<version>`). Set this to use the path-style `image_name`
    /// from each manifest instead. No effect on images pinned to an explicit url.
    #[clap(long)]
    pub use_image_path: bool,
    /// Output path for the generated toolbox.json
    ///
    /// Defaults to a `toolbox.json` beside the --config file (the toolbox root). Overriding this
    /// only redirects the artifact; it does not change where manifests are crawled from.
    #[clap(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Root directory to walk for tool, image, and pipeline manifests
    ///
    /// Defaults to the directory containing --config (the toolbox root). Overriding this only
    /// changes the crawl root; it does not change where toolbox.json is written.
    #[clap(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
    /// Append this suffix to every derived image tag's version, baking it into the
    /// generated toolbox.json (tags and embedded image urls)
    ///
    /// Lets a CI/CD feature-branch build produce a toolbox.json that references
    /// differentiated images (e.g. `:1.0-mybranch`) instead of colliding with the
    /// mainline `:1.0`. Only affects derived `<registry>/...:<version>` tags; images
    /// pinned to an explicit url are left untouched. Pass the separator you want
    /// (e.g. `-mybranch`).
    ///
    /// This is the alternative to `toolbox build-images --tag-suffix`, which suffixes tags at
    /// build time without touching toolbox.json; use one or the other (build-images won't
    /// re-apply a suffix a tag already carries).
    // allow_hyphen_values so a leading-dash suffix like `-mybranch` is taken as the
    // value rather than parsed as another flag
    #[clap(long, value_name = "SUFFIX", allow_hyphen_values = true)]
    pub tag_suffix: Option<String>,
    /// The format of the toolbox to write: `1` (the legacy images and pipelines) or `2`
    /// (tools, toolsets, agents, workflows, pipelines with stage tools, and base builds)
    ///
    /// By default the format follows the manifests found: any `type = "tool"`, `"build"`,
    /// `"toolset"`, `"agent"`, or `"workflow"` manifest, or a pipeline with `[stage_tools]`, writes
    /// format 2,
    /// and legacy `type = "image"` manifests alone write format 1. Format 2 over legacy
    /// manifests converts them exactly as `toolbox convert` does. Format 1 refuses manifests
    /// that only format 2 can hold.
    #[clap(long, value_name = "VERSION", value_parser = ["1", "2"])]
    pub format_version: Option<String>,
    /// Rewrite the legacy image and pipeline manifests in place as tool, build, and stage-tool
    /// manifests
    ///
    /// Each `type = "image"` manifest.toml becomes a `type = "tool"` manifest whose `spec_from`
    /// names its config file (`config_from`), which now holds the converted spec as JSON; an
    /// image with `[image_from]` keeps it and leaves its spec's image empty. A build-only image
    /// (no config) becomes a `type = "build"` manifest, and each pipeline's `[images.*]` tables
    /// become `[stage_tools.*]` tables. Every replaced file is kept with `.bak` added to its name
    /// and an existing backup is never replaced. Writes format 2, so it can't be combined with
    /// `--format-version 1`.
    #[clap(long)]
    pub rewrite_manifests: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple `KEY=VALUE` splits into its key and value
    #[test]
    fn parse_build_arg_splits_key_value() {
        // the `:` in the image tag lives in the value, so it must not affect the split
        assert_eq!(
            parse_build_arg("IMAGE=ubuntu:22.04"),
            Ok(("IMAGE".to_string(), "ubuntu:22.04".to_string()))
        );
    }

    /// Only the first `=` separates, so values containing `=` (and registry refs with
    /// `:` and `/`) survive intact
    #[test]
    fn parse_build_arg_splits_on_first_equals() {
        // a value that is itself a chain of `=` keeps every `=` after the first
        assert_eq!(
            parse_build_arg("OPTS=a=b=c"),
            Ok(("OPTS".to_string(), "a=b=c".to_string()))
        );
        // a full registry reference (with `/` and `:`) passes through untouched as the value
        assert_eq!(
            parse_build_arg("IMAGE=ghcr.io/org/base:1.0"),
            Ok(("IMAGE".to_string(), "ghcr.io/org/base:1.0".to_string()))
        );
    }

    /// An empty value is allowed; a missing `=` or empty key is rejected
    #[test]
    fn parse_build_arg_rejects_malformed() {
        // a trailing `=` yields an empty value, which is intentionally permitted
        assert_eq!(
            parse_build_arg("EMPTY="),
            Ok(("EMPTY".to_string(), String::new()))
        );
        // no `=` at all has no key/value boundary, so it must error
        assert!(parse_build_arg("no-equals").is_err());
        // an empty key (leading `=`) is rejected because a nameless build arg is unusable
        assert!(parse_build_arg("=value").is_err());
    }
}
