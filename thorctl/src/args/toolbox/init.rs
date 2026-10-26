//! Arguments for `thorctl toolbox init`

use clap::Parser;
use std::path::PathBuf;

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
    /// Initialize a tool, toolset, agent, or workflow with a manifest.toml and a spec file
    ///
    /// Writes a `type = "tool"` (or toolset, agent, workflow) manifest.toml and the spec file it
    /// points at: `<name>.json` for a tool, `<name>.toolset.yaml`, `<name>.agent.yaml`, or
    /// `<name>.workflow.yaml` for the others. Fill in the spec, then run `toolbox build`.
    #[clap(version, author)]
    Tool(InitTool),
}

/// The kind of item `toolbox init tool` scaffolds
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitToolKind {
    /// A tool run as Thorium jobs from a container image
    Image,
    /// A tool a model calls (backed by Thorium's MCP server, a container tool, or a harness)
    Agentic,
    /// A swappable agent harness
    Harness,
    /// A toolset of tools
    Toolset,
    /// An agent that uses tools
    Agent,
    /// A workflow of agents
    Workflow,
}

/// Initialize a full toolbox with config.toml, image, and pipeline files
#[derive(Parser, Debug)]
pub struct InitToolbox {
    /// Image build directories, comma-separated. Format: `path[,path,...]` (each gets a
    /// manifest.toml + JSON config)
    ///
    /// Relative paths are resolved under --toolbox-dir.
    #[clap(
        short = 'i',
        long = "images",
        value_name = "PATH",
        required = true,
        value_delimiter = ','
    )]
    pub images: Vec<PathBuf>,
    /// Pipeline directories, optionally binding images. Format: `path[:image,...]`
    ///
    /// Use the optional `[:image,...]` colon suffix to bind specific images, e.g.
    /// `-p ./pipelines/capa:capa,yara`. Without it, all --images are bound. Repeat -p for
    /// multiple pipelines. A suffix containing a path separator, or a Windows drive
    /// prefix like `C:`, is part of the path rather than an image list. Relative paths
    /// are resolved under --toolbox-dir.
    #[clap(
        short = 'p',
        long = "pipeline",
        value_name = "PATH[:IMAGE,...]",
        verbatim_doc_comment
    )]
    pub pipelines: Vec<String>,
    /// Group name to use in generated configs (prompted interactively if omitted)
    #[clap(short = 'g', long = "group", value_name = "GROUP")]
    pub group: Option<String>,
    /// Path to the toolbox root directory where config.toml will be created
    #[clap(long, value_name = "DIR", default_value = ".")]
    pub toolbox_dir: PathBuf,
    /// Seed the new config.toml from an existing one (name, registry, registries,
    /// `image_path_prefix`, export paths, `bundled_images`, `[base_image]`) instead of --name/--registry
    ///
    /// Mutually exclusive with --name, --registry, --image-path, and --pipeline-path.
    #[clap(short = 'c', long = "config", value_name = "CONFIG.TOML", conflicts_with_all = ["name", "registry", "image_path", "pipeline_path"], verbatim_doc_comment)]
    pub config: Option<PathBuf>,
    /// Toolbox name for config.toml
    #[clap(long, value_name = "NAME", default_value = "My Toolbox")]
    pub name: String,
    /// Container registry for config.toml, e.g. ghcr.io/org/repo
    ///
    /// Optional: when omitted, the toolbox declares no central registry and each
    /// image's tag is taken from the `image` url in its own config.
    #[clap(long, value_name = "REGISTRY")]
    pub registry: Option<String>,
    /// Directory (relative to the toolbox root) `export` writes image tool dirs under (default: images)
    ///
    /// Sets `export_image_path` in config.toml. Must be a relative subpath (no absolute path, no
    /// `..`). Only affects where `export` places files — `build` still discovers manifests at any depth.
    #[clap(long, value_name = "DIR")]
    pub image_path: Option<String>,
    /// Directory (relative to the toolbox root) `export` writes pipeline tool dirs under (default: pipelines)
    ///
    /// Sets `export_pipeline_path` in config.toml. Must be a relative subpath (no absolute path, no `..`).
    #[clap(long, value_name = "DIR")]
    pub pipeline_path: Option<String>,
    /// The editor to use when filling in configs (defaults to your configured `default_editor`)
    #[clap(long, value_name = "EDITOR")]
    pub editor: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    ///
    /// Interactive mode needs a terminal on stdin and stderr; without one, pass -n together
    /// with --group.
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing per-tool files instead of skipping them
    ///
    /// Covers the scaffolded manifest.toml/JSON/description files. It does NOT touch an existing
    /// config.toml — use --overwrite-config for that.
    #[clap(long)]
    pub overwrite: bool,
    /// Overwrite an existing config.toml instead of preserving it
    ///
    /// By default an existing config.toml is kept (so re-running init in a toolbox doesn't clobber
    /// its settings). Distinct from --overwrite, which covers per-tool files.
    #[clap(long)]
    pub overwrite_config: bool,
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
    /// `"C:\tb\capa"` → path `C:\tb\capa`, images `None` (a drive prefix is not a binding)
    ///
    /// The last `:` only separates an image list when what follows it contains no path
    /// separator and the colon isn't a Windows drive prefix (a single ASCII letter at the
    /// start of the string).
    ///
    /// # Arguments
    ///
    /// * `s` - The pipeline argument string to parse
    pub fn parse(s: &str) -> Self {
        // the whole string is the path whenever there is no usable image binding
        let unbound = || Self {
            path: PathBuf::from(s),
            images: None,
        };
        // split on the LAST colon so a path that itself contains a colon keeps everything
        // up to the final one as the directory and only the trailing segment is the image list
        let Some((path, images_str)) = s.rsplit_once(':') else {
            // no colon at all means bind every image
            return unbound();
        };
        // a single letter before the colon is a Windows drive prefix (`C:`), not a path
        let is_drive_prefix = path.len() == 1 && path.chars().all(|c| c.is_ascii_alphabetic());
        // image names never contain path separators, so such a suffix is part of the path
        let has_separator = images_str.contains(['/', '\\']);
        if is_drive_prefix || has_separator {
            return unbound();
        }
        // split the post-colon segment into individual image names, trimming
        // whitespace and dropping empties so `capa, yara,` yields just `["capa","yara"]`
        let images: Vec<String> = images_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // a colon with no real image names (e.g. a trailing `:` or `: `) is treated as
        // "no binding": keep the FULL original string as the path so the colon isn't
        // silently stripped off a directory that legitimately contained one
        if images.is_empty() {
            return unbound();
        }
        Self {
            path: PathBuf::from(path),
            images: Some(images),
        }
    }
}

/// Initialize a single image with a manifest.toml and JSON config
#[derive(Parser, Debug)]
pub struct InitImage {
    /// Path to the image build directory (the scaffolded files land here)
    #[clap(value_name = "PATH")]
    pub path: PathBuf,
    /// The manifest `image_name`: the registry tag path leaf used at build time, formatted as a
    /// repo-style path (`<registry>/<image_name>:<version>`); defaults to the build directory name
    #[clap(long, value_name = "IMAGE_NAME")]
    pub image_name: Option<String>,
    /// Group name to use in the generated image config (prompted interactively if omitted)
    #[clap(short = 'g', long = "group", value_name = "GROUP")]
    pub group: Option<String>,
    /// Validate against an existing toolbox's config.toml (resolution source, not placement)
    ///
    /// When set, scaffolding errors if an image of the same name+version already exists in that
    /// toolbox (pass --overwrite to replace). Does not move files — the positional path is the
    /// destination.
    #[clap(short = 'c', long = "config", value_name = "CONFIG.TOML")]
    pub config: Option<PathBuf>,
    /// Skip building this image in CI/CD (image already exists in registry)
    #[clap(long)]
    pub no_build: bool,
    /// The editor to use when filling in the config (defaults to your configured `default_editor`)
    #[clap(long, value_name = "EDITOR")]
    pub editor: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    ///
    /// Interactive mode needs a terminal on stdin and stderr; without one, pass -n together
    /// with --group.
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long)]
    pub overwrite: bool,
}

/// Initialize a single pipeline with a manifest.toml and JSON config
#[derive(Parser, Debug)]
pub struct InitPipeline {
    /// Path to the pipeline directory (the scaffolded files land here)
    #[clap(value_name = "PATH")]
    pub path: PathBuf,
    /// Image names this pipeline runs, comma-separated. Format: `image[,image,...]`
    ///
    /// Required with --non-interactive. Otherwise, when omitted, the editor opens with an
    /// empty order to fill in.
    #[clap(
        short = 'i',
        long = "images",
        value_name = "IMAGE",
        value_delimiter = ','
    )]
    pub images: Vec<String>,
    /// Group name to use in the generated pipeline config (prompted interactively if omitted)
    #[clap(short = 'g', long = "group", value_name = "GROUP")]
    pub group: Option<String>,
    /// Pipeline order as JSON: a list of parallel stages, e.g. `[["img1","img2"],["img3"]]`,
    /// or a flat list like `["img1","img2"]` (defaults to all images in a single parallel stage)
    #[clap(long, value_name = "JSON")]
    pub order: Option<String>,
    /// Resolve the pipeline's images against an existing toolbox's config.toml (the "look here"
    /// source, not placement)
    ///
    /// When set, every referenced image must exist in that toolbox (else an error — `init pipeline`
    /// never creates images), and each is version-pinned from the toolbox instead of `latest`. Does
    /// not move files — the positional path is the destination.
    #[clap(short = 'c', long = "config", value_name = "CONFIG.TOML")]
    pub config: Option<PathBuf>,
    /// The editor to use when filling in the config (defaults to your configured `default_editor`)
    #[clap(long, value_name = "EDITOR")]
    pub editor: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    ///
    /// Interactive mode needs a terminal on stdin and stderr; without one, pass -n together
    /// with --group and --images.
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long)]
    pub overwrite: bool,
}

/// Initialize a tool, toolset, agent, or workflow with a manifest.toml and a spec file
#[derive(Parser, Debug)]
pub struct InitTool {
    /// Path to the directory to scaffold into; its name is the item's name
    #[clap(value_name = "PATH")]
    pub path: PathBuf,
    /// The kind of item to scaffold
    #[clap(long, value_enum, value_name = "KIND")]
    pub kind: InitToolKind,
    /// Group name to put the item in (prompted interactively if omitted)
    #[clap(short = 'g', long = "group", value_name = "GROUP")]
    pub group: Option<String>,
    /// The manifest `image_name`: the registry tag path leaf used at build time when
    /// `toolbox build --use-image-path` is set (image tools only; defaults to the item's name)
    #[clap(long, value_name = "IMAGE_NAME")]
    pub image_name: Option<String>,
    /// Skip building this image in CI/CD (image already exists in registry; image tools only)
    #[clap(long)]
    pub no_build: bool,
    /// Skip interactive prompts and use defaults for all fields
    ///
    /// Interactive mode needs a terminal on stdin and stderr; without one, pass -n together
    /// with --group.
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
    /// Overwrite existing files instead of skipping them
    #[clap(long)]
    pub overwrite: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trailing image list is split off, and a missing one binds every image
    #[test]
    fn pipeline_spec_parses_image_binding() {
        let spec = PipelineSpec::parse("./pipelines/capa:capa, yara,");
        assert_eq!(spec.path, PathBuf::from("./pipelines/capa"));
        assert_eq!(
            spec.images,
            Some(vec!["capa".to_string(), "yara".to_string()])
        );
        // no colon binds everything
        let spec = PipelineSpec::parse("./pipelines/capa");
        assert_eq!(spec.path, PathBuf::from("./pipelines/capa"));
        assert!(spec.images.is_none());
        // a trailing colon with no names keeps the whole string as the path
        let spec = PipelineSpec::parse("./pipelines/capa:");
        assert_eq!(spec.path, PathBuf::from("./pipelines/capa:"));
        assert!(spec.images.is_none());
    }

    /// Windows drive prefixes and colon-containing directories are never split as bindings
    #[test]
    fn pipeline_spec_handles_drive_letters() {
        // a drive path with no binding keeps the whole path
        let spec = PipelineSpec::parse("C:\\tb\\pipelines\\capa");
        assert_eq!(spec.path, PathBuf::from("C:\\tb\\pipelines\\capa"));
        assert!(spec.images.is_none());
        // forward slashes after the drive are handled the same way
        let spec = PipelineSpec::parse("C:/tb/pipelines/capa");
        assert_eq!(spec.path, PathBuf::from("C:/tb/pipelines/capa"));
        assert!(spec.images.is_none());
        // a drive path with a binding splits only at the last colon
        let spec = PipelineSpec::parse("C:\\tb\\capa:capa,yara");
        assert_eq!(spec.path, PathBuf::from("C:\\tb\\capa"));
        assert_eq!(
            spec.images,
            Some(vec!["capa".to_string(), "yara".to_string()])
        );
        // a bare drive-relative path is a path, not a binding of image `capa` to dir `C`
        let spec = PipelineSpec::parse("C:capa");
        assert_eq!(spec.path, PathBuf::from("C:capa"));
        assert!(spec.images.is_none());
        // a directory whose name contains a colon keeps its full path
        let spec = PipelineSpec::parse("./a:b/capa");
        assert_eq!(spec.path, PathBuf::from("./a:b/capa"));
        assert!(spec.images.is_none());
    }
}
