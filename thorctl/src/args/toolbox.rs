//! Arguments for toolbox-related Thorctl commands

use clap::Parser;
use std::path::PathBuf;
use url::Url;

/// A command to interact with Thorium toolboxes
#[derive(Parser, Debug)]
pub enum Toolbox {
    /// Import a toolbox into Thorium
    ///
    /// A Thorium toolbox is an external collection of tools and pipelines pre-configured
    /// and ready to run in Thorium. If images or pipelines already exist in Thorium,
    /// an interactive editor will open to review and resolve differences. Use --overwrite
    /// to automatically apply all incoming changes without the editor.
    #[clap(version, author)]
    Import(ImportToolbox),
    /// Build a toolbox manifest from image and pipeline manifests
    ///
    /// Walks the current directory for image and pipeline manifest.toml files,
    /// reads their associated JSON configs, and produces a toolbox.json file
    /// suitable for import into Thorium.
    #[clap(version, author)]
    Build(BuildToolbox),
    /// Initialize toolbox, image, or pipeline scaffolding
    ///
    /// Generate default manifest.toml and JSON config files. Existing files
    /// are never overwritten.
    #[clap(version, author, subcommand)]
    Init(Init),
    /// Export Thorium images and pipelines into a toolbox directory structure
    ///
    /// Fetches image and pipeline configs from a running Thorium instance,
    /// creates a toolbox directory with manifest.toml and JSON config files,
    /// and produces a toolbox.json ready for import elsewhere.
    #[clap(version, author)]
    Export(ExportToolbox),
    /// Remove a previously imported toolbox from Thorium
    ///
    /// Deletes the pipelines and images named by a toolbox manifest from the
    /// target instance. Pipelines are deleted before the images they
    /// reference; resources that don't exist are reported and skipped.
    /// Groups are never deleted.
    #[clap(version, author)]
    Remove(RemoveToolbox),
    /// Diff an on-disk toolbox against what a Thorium instance has imported
    ///
    /// Shows what an import of this toolbox would change, rendered like git
    /// diff: resources only in the toolbox appear as new files, resources
    /// only in the instance's groups as deletions, and changed resources as
    /// unified hunks.
    #[clap(version, author)]
    Diff(DiffToolbox),
    /// Build (and optionally push) the container images in a toolbox
    ///
    /// Walks a toolbox.json for image entries with build enabled, builds each
    /// entry's docker context with its first tag, aliases the remaining tags,
    /// and pushes all tags with --push. For forks without CI: build, push,
    /// then import as usual.
    #[clap(version, author, name = "build-images")]
    BuildImages(BuildImagesToolbox),
}

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
    // split on the first '=' so values containing '=' are preserved
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got '{raw}'"))?;
    // a build arg with no key can't be passed to the runtime
    if key.is_empty() {
        return Err(format!("build arg key must not be empty in '{raw}'"));
    }
    Ok((key.to_string(), value.to_string()))
}

/// Build the container images described by a toolbox manifest
#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct BuildImagesToolbox {
    /// The path to the toolbox.json holding the build entries
    #[clap(default_value = "toolbox.json")]
    pub manifest: PathBuf,
    /// Only build these images (default: every image with build enabled)
    #[clap(short = 'i', long = "images", value_delimiter = ',')]
    pub images: Vec<String>,
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
    /// Unlike `--base-image`, these are passed to every build regardless of
    /// `allow_base_override`.
    #[clap(long = "build-arg", value_parser = parse_build_arg, value_name = "KEY=VALUE")]
    pub build_args: Vec<(String, String)>,
    /// Append this suffix to every tag built and pushed, without touching toolbox.json
    ///
    /// Lets a CI/CD feature-branch run build/push differentiated images (e.g.
    /// `:1.0-mybranch`) from an unmodified toolbox.json so they don't collide with the
    /// mainline `:1.0`. Pass the separator you want (e.g. `-mybranch`).
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

/// Diff an on-disk toolbox against a running Thorium instance
#[derive(Parser, Debug)]
pub struct DiffToolbox {
    /// A toolbox.json (path or URL) or a toolbox repo directory
    ///
    /// Directories are built in-memory from their manifests, so the diff
    /// reflects the current on-disk configs without regenerating toolbox.json.
    pub manifest: ManifestLocation,
    /// Compare against this group instead of the groups recorded in the toolbox
    ///
    /// Use this when the toolbox was imported with --group-override.
    #[clap(long)]
    pub group_override: Option<String>,
    /// Exit with code 1 when any difference exists (git diff semantics)
    #[clap(long)]
    pub exit_code: bool,
}

/// Remove a toolbox's pipelines and images from Thorium
#[derive(Parser, Debug)]
pub struct RemoveToolbox {
    /// The URL or file path on the system where the toolbox manifest is found
    pub manifest: ManifestLocation,
    /// Remove from this group instead of the groups recorded in the manifest
    ///
    /// Use this when the toolbox was imported with --group-override.
    #[clap(long)]
    pub group_override: Option<String>,
    /// Skip the confirmation dialog
    #[clap(short = 'y', long)]
    pub skip_confirm: bool,
}

/// Subcommands for `thorctl toolbox init`
#[derive(Parser, Debug)]
pub enum Init {
    /// Initialize a full toolbox with config.toml, image, and pipeline files
    #[clap(version, author)]
    Toolbox(InitToolbox),
    /// Initialize a single image with a manifest.toml and JSON config
    #[clap(version, author)]
    Image(InitImage),
    /// Initialize a single pipeline with a manifest.toml and JSON config
    #[clap(version, author)]
    Pipeline(InitPipeline),
}

/// The location of the toolbox manifest, either by URL or by file path
#[derive(Debug, Clone)]
pub enum ManifestLocation {
    /// The manifest is at this URL
    Url(Url),
    /// The manifest is at this file path
    Path(PathBuf),
}

impl std::str::FromStr for ManifestLocation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // try parsing as a URL first
        if let Ok(url) = Url::parse(s) {
            return Ok(Self::Url(url));
        }
        // if URL parsing fails, treat it as a file path
        Ok(Self::Path(PathBuf::from(s)))
    }
}

/// Download a toolbox manifest and import it into Thorium
#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct ImportToolbox {
    /// The URL or file path on the system where the toolbox manifest is found
    pub manifest: ManifestLocation,
    /// Force the tools and pipelines to be imported to a specific group
    ///
    /// The group will be created if it doesn't already exist
    #[clap(long)]
    pub group_override: Option<String>,
    /// Overwrite existing images/pipelines without opening the editor
    #[clap(long, conflicts_with = "skip_conflicts")]
    pub overwrite: bool,
    /// Skip existing images/pipelines that differ instead of updating them
    ///
    /// New resources are still created. Each skipped resource logs a warning
    /// listing the fields that differ, making this safe for non-interactive
    /// (CI/agent) imports that must never overwrite local changes.
    #[clap(long)]
    pub skip_conflicts: bool,
    /// Automatically roll back applied changes if the import stops early
    ///
    /// Only applies when the session can't prompt (--overwrite, --skip-conflicts, or
    /// no TTY); interactive sessions are asked instead. Registry pushes are not
    /// undone, only Thorium state.
    #[clap(long)]
    pub rollback_on_failure: bool,
    /// Override the default editor for reviewing merge conflicts
    #[clap(long)]
    pub editor: Option<String>,
    /// Target registry base path for images bundled in the toolbox
    ///
    /// Only used when the toolbox bundles container images (exported with
    /// `--with-images`). Each bundled image is loaded, retagged, and pushed to
    /// `<image-path-prefix>/<group>/<name>:<tag>`, and its Thorium config is
    /// rewritten to point there. If omitted for a bundled toolbox, the prefix
    /// recorded in the manifest is used, otherwise you are prompted for one.
    #[clap(long)]
    pub image_path_prefix: Option<String>,
    /// Update existing Thorium network policies to match the toolbox
    ///
    /// Without this, an existing policy that differs is left untouched and only a
    /// warning is logged. With it, the toolbox's definition is authoritative:
    /// rule/flag differences overwrite the policy, and a groups-only difference
    /// adds the toolbox's groups to the existing policy. Group coverage is only
    /// ever added, never removed.
    ///
    /// This overwrites cluster network-security state. In non-interactive runs
    /// (--skip-conflicts, --overwrite, or no TTY) it applies WITHOUT a prompt.
    /// Note: changing a policy's default_policy does not retroactively remove it
    /// from images created during this same import, so that part is not unwound
    /// by a rollback.
    #[clap(long)]
    pub update_network_policy: bool,
}

/// Build a toolbox manifest from image and pipeline manifests
#[derive(Parser, Debug, Clone)]
pub struct BuildToolbox {
    /// Path to the toolbox TOML config file (e.g., config.toml)
    #[clap(short = 'c', long = "config", default_value = "config.toml")]
    pub config: PathBuf,
    /// Tag every image with its manifest `image_name` (a repo-style path) as the leaf:
    /// `<registry>/[prefix/]<image_name>:<version>`
    ///
    /// By default images are tagged with the tool `name`
    /// (`<registry>/[prefix/]<name>:<version>`). Set this to use the path-style `image_name`
    /// from each manifest instead. No effect on images pinned to an explicit url.
    #[clap(long)]
    pub use_image_path: bool,
    /// Output file path
    #[clap(short, long, default_value = "toolbox.json")]
    pub output: PathBuf,
    /// Root directory to walk for image/pipeline manifests (default: current directory)
    #[clap(long, default_value = ".")]
    pub path: PathBuf,
    /// Append this suffix to every derived image tag's version, baking it into the
    /// generated toolbox.json (tags and embedded image urls)
    ///
    /// Lets a CI/CD feature-branch build produce a toolbox.json that references
    /// differentiated images (e.g. `:1.0-mybranch`) instead of colliding with the
    /// mainline `:1.0`. Only affects derived `<registry>/...:<version>` tags; images
    /// pinned to an explicit url are left untouched. Pass the separator you want
    /// (e.g. `-mybranch`).
    // allow_hyphen_values so a leading-dash suffix like `-mybranch` is taken as the
    // value rather than parsed as another flag
    #[clap(long, value_name = "SUFFIX", allow_hyphen_values = true)]
    pub tag_suffix: Option<String>,
}

/// Initialize a full toolbox with config.toml, image, and pipeline files
#[derive(Parser, Debug)]
pub struct InitToolbox {
    /// Paths to image build directories (each gets a manifest.toml + JSON config)
    #[clap(short = 'i', long = "images", required = true, value_delimiter = ',')]
    pub images: Vec<PathBuf>,
    /// Pipeline directories, optionally with image associations.
    ///
    /// Use colon syntax to bind specific images: -p ./pipelines/capa:capa,yara
    ///
    /// Without the colon, all images are included in the pipeline.
    #[clap(short = 'p', long = "pipeline", verbatim_doc_comment)]
    pub pipelines: Vec<String>,
    /// Group name to use in generated configs (prompted interactively if omitted)
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Path to the toolbox root directory where config.toml will be created
    /// (default: current directory)
    #[clap(long, default_value = ".")]
    pub toolbox_dir: PathBuf,
    /// Seed the new config.toml from an existing one (name, registry, registries,
    /// image_path_prefix, bundled_images) instead of --name/--registry
    ///
    /// Mutually exclusive with --name and --registry.
    #[clap(short = 'c', long = "config", conflicts_with_all = ["name", "registry"], verbatim_doc_comment)]
    pub config: Option<PathBuf>,
    /// Toolbox name for config.toml
    #[clap(long, default_value = "My Toolbox")]
    pub name: String,
    /// Container registry for config.toml (e.g., "ghcr.io/org/repo")
    ///
    /// Optional: when omitted, the toolbox declares no central registry and each
    /// image's tag is taken from the `image` url in its own config.
    #[clap(long)]
    pub registry: Option<String>,
    /// The editor to use when filling in configs (defaults to your configured `default_editor`)
    #[clap(long)]
    pub editor: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long)]
    pub overwrite: bool,
}

/// A parsed pipeline spec from the --pipeline flag
#[derive(Debug)]
pub struct PipelineSpec {
    /// Path to the pipeline directory
    pub path: PathBuf,
    /// Specific image names for this pipeline, or None to include all
    pub images: Option<Vec<String>>,
}

impl PipelineSpec {
    /// Parse a pipeline argument string.
    ///
    /// `"./pipelines/capa:capa,yara"` → path `./pipelines/capa`, images `["capa", "yara"]`
    /// `"./pipelines/capa"` → path `./pipelines/capa`, images `None` (all images)
    ///
    /// # Arguments
    ///
    /// * `s` - The pipeline argument string to parse
    pub fn parse(s: &str) -> Self {
        if let Some((path, images_str)) = s.rsplit_once(':') {
            let images: Vec<String> = images_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if images.is_empty() {
                Self {
                    path: PathBuf::from(s),
                    images: None,
                }
            } else {
                Self {
                    path: PathBuf::from(path),
                    images: Some(images),
                }
            }
        } else {
            Self {
                path: PathBuf::from(s),
                images: None,
            }
        }
    }
}

/// Initialize a single image with a manifest.toml and JSON config
#[derive(Parser, Debug)]
pub struct InitImage {
    /// Path to the image build directory
    pub path: PathBuf,
    /// The manifest `image_name`: the registry tag path leaf used at build time
    /// (`<registry>/<image_name>:<version>`); defaults to the build directory name
    #[clap(long)]
    pub image_name: Option<String>,
    /// Group name to use in the generated image config (prompted interactively if omitted)
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Skip building this image in CI/CD (image already exists in registry)
    #[clap(long)]
    pub no_build: bool,
    /// The editor to use when filling in the config (defaults to your configured `default_editor`)
    #[clap(long)]
    pub editor: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long)]
    pub overwrite: bool,
}

/// Initialize a single pipeline with a manifest.toml and JSON config
#[derive(Parser, Debug)]
pub struct InitPipeline {
    /// Path to the pipeline directory
    pub path: PathBuf,
    /// Image names to include in this pipeline (prompted interactively if omitted)
    #[clap(short = 'i', long = "images", value_delimiter = ',')]
    pub images: Vec<String>,
    /// Group name to use in the generated pipeline config (prompted interactively if omitted)
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Pipeline order as JSON (e.g., '[["img1","img2"],["img3"]]').
    /// Defaults to all images in a single parallel stage.
    #[clap(long)]
    pub order: Option<String>,
    /// The editor to use when filling in the config (defaults to your configured `default_editor`)
    #[clap(long)]
    pub editor: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long)]
    pub overwrite: bool,
}

/// Export Thorium images and pipelines into a toolbox directory
#[derive(Parser, Debug)]
pub struct ExportToolbox {
    /// Export all images and pipelines from this group
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Export specific pipelines (format: group/name, or just name if --group is set).
    /// Images referenced by exported pipelines are auto-included.
    #[clap(short = 'p', long = "pipelines", value_delimiter = ',')]
    pub pipelines: Vec<String>,
    /// Export specific standalone images (format: group/name, or just name if --group is set)
    #[clap(short = 'i', long = "images", value_delimiter = ',')]
    pub images: Vec<String>,
    /// Override the group in all exported configs to this value.
    /// Warns if name collisions would occur across source groups.
    #[clap(long)]
    pub group_override: Option<String>,
    /// Root directory for the exported toolbox
    #[clap(short = 'o', long = "output", default_value = "./toolbox")]
    pub output: PathBuf,
    /// Read the toolbox-wide settings (name, registry, registries, image_path_prefix,
    /// bundled_images) from an existing config.toml instead of --name/--registry
    ///
    /// Lets an export reuse an existing toolbox's config rather than re-specifying it.
    /// Mutually exclusive with --name and --registry.
    #[clap(short = 'c', long = "config", conflicts_with_all = ["name", "registry"])]
    pub config: Option<PathBuf>,
    /// Toolbox name for config.toml
    #[clap(long, default_value = "My Toolbox")]
    pub name: String,
    /// Container registry for config.toml
    ///
    /// Optional: when omitted, the exported toolbox declares no central registry and
    /// relies on each image's own `image` url (always captured on export).
    #[clap(long)]
    pub registry: Option<String>,
    /// Skip on-disk conflicts: write new configs and leave differing existing ones
    /// untouched with a warning (use --overwrite to overwrite instead)
    #[clap(long)]
    pub skip_conflicts: bool,
    /// Open each config in an editor to review/tweak it before writing
    #[clap(long)]
    pub review: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long, conflicts_with = "skip_conflicts")]
    pub overwrite: bool,
    /// Bundle each image's container image file into the toolbox for offline transfer
    ///
    /// Downloads (docker pull) and saves (docker save) each image to
    /// `<output>/images/<name>/<name>.tar.gz`. The resulting toolbox can be moved
    /// to an offline environment and imported with `--image-path-prefix` to push the
    /// images into a local registry. Requires docker.
    #[clap(long)]
    pub with_images: bool,
}

/// A parsed group/name resource reference
#[derive(Debug, Clone)]
pub struct ResourceSpec {
    /// The group the resource belongs to
    pub group: String,
    /// The name of the resource
    pub name: String,
}

impl ResourceSpec {
    /// Parse "group/name" or "name" (with a default group fallback)
    ///
    /// # Arguments
    ///
    /// * `s` - The resource reference to parse
    /// * `default_group` - The group to fall back to when `s` has no group prefix
    pub fn parse(s: &str, default_group: Option<&str>) -> Result<Self, String> {
        if let Some((group, name)) = s.split_once('/') {
            Ok(Self {
                group: group.to_string(),
                name: name.to_string(),
            })
        } else {
            match default_group {
                Some(g) => Ok(Self {
                    group: g.to_string(),
                    name: s.to_string(),
                }),
                None => Err(format!(
                    "'{s}' must be in group/name format when --group is not set"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple `KEY=VALUE` splits into its key and value
    #[test]
    fn parse_build_arg_splits_key_value() {
        assert_eq!(
            parse_build_arg("IMAGE=ubuntu:22.04"),
            Ok(("IMAGE".to_string(), "ubuntu:22.04".to_string()))
        );
    }

    /// Only the first `=` separates, so values containing `=` (and registry refs with
    /// `:` and `/`) survive intact
    #[test]
    fn parse_build_arg_splits_on_first_equals() {
        assert_eq!(
            parse_build_arg("OPTS=a=b=c"),
            Ok(("OPTS".to_string(), "a=b=c".to_string()))
        );
        assert_eq!(
            parse_build_arg("IMAGE=ghcr.io/org/base:1.0"),
            Ok(("IMAGE".to_string(), "ghcr.io/org/base:1.0".to_string()))
        );
    }

    /// An empty value is allowed; a missing `=` or empty key is rejected
    #[test]
    fn parse_build_arg_rejects_malformed() {
        assert_eq!(parse_build_arg("EMPTY="), Ok(("EMPTY".to_string(), String::new())));
        assert!(parse_build_arg("no-equals").is_err());
        assert!(parse_build_arg("=value").is_err());
    }
}
