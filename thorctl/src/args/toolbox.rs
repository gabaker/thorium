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
    /// an interactive editor will open to review and resolve differences. Use --force
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
pub struct ImportToolbox {
    /// The URL or file path on the system where the toolbox manifest is found
    pub manifest: ManifestLocation,
    /// Skip the confirmation dialog
    #[clap(short = 'y', long)]
    pub skip_confirm: bool,
    /// Force the tools and pipelines to be imported to a specific group
    ///
    /// The group will be created if it doesn't already exist
    #[clap(long)]
    pub group_override: Option<String>,
    /// Force update existing images/pipelines without opening the editor
    #[clap(short = 'f', long)]
    pub force: bool,
    /// Override the default editor for reviewing merge conflicts
    #[clap(long)]
    pub editor: Option<String>,
}

/// Build a toolbox manifest from image and pipeline manifests
#[derive(Parser, Debug)]
pub struct BuildToolbox {
    /// Path to the toolbox TOML config file (e.g., config.toml)
    #[clap(short = 'c', long = "config")]
    pub config: PathBuf,
    /// Use manifest name field instead of image_name for container tag paths
    #[clap(short = 'o', long)]
    pub override_path: bool,
    /// Output file path
    #[clap(long, default_value = "toolbox.json")]
    pub output: PathBuf,
    /// Root directory to walk for image/pipeline manifests (default: current directory)
    #[clap(long, default_value = ".")]
    pub path: PathBuf,
}

/// Initialize a full toolbox with config.toml, image, and pipeline files
#[derive(Parser, Debug)]
pub struct InitToolbox {
    /// Paths to image build directories (each gets a manifest.toml + JSON config)
    #[clap(short = 'i', long = "image", required = true)]
    pub images: Vec<PathBuf>,
    /// Pipeline directories, optionally with image associations.
    ///
    /// Use colon syntax to bind specific images: -p ./pipelines/capa:capa,yara
    ///
    /// Without the colon, all images are included in the pipeline.
    #[clap(short = 'p', long = "pipeline", required = true)]
    pub pipelines: Vec<String>,
    /// Group name to use in generated configs (prompted interactively if omitted)
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Path to the toolbox root directory where config.toml will be created
    /// (default: current directory)
    #[clap(long, default_value = ".")]
    pub toolbox_dir: PathBuf,
    /// Toolbox name for config.toml
    #[clap(long, default_value = "My Toolbox")]
    pub name: String,
    /// Container registry for config.toml (e.g., "ghcr.io/org/repo")
    #[clap(long, default_value = "")]
    pub registry: String,
    /// Skip interactive prompts and use defaults for all fields
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
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
    /// Group name to use in the generated image config (prompted interactively if omitted)
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Skip building this image in CI/CD (image already exists in registry)
    #[clap(long)]
    pub no_build: bool,
    /// Skip interactive prompts and use defaults for all fields
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
}

/// Initialize a single pipeline with a manifest.toml and JSON config
#[derive(Parser, Debug)]
pub struct InitPipeline {
    /// Path to the pipeline directory
    pub path: PathBuf,
    /// Image names to include in this pipeline (prompted interactively if omitted)
    #[clap(short = 'i', long = "image")]
    pub images: Vec<String>,
    /// Group name to use in the generated pipeline config (prompted interactively if omitted)
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Pipeline order as JSON (e.g., '[["img1","img2"],["img3"]]').
    /// Defaults to all images in a single parallel stage.
    #[clap(long)]
    pub order: Option<String>,
    /// Skip interactive prompts and use defaults for all fields
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
}

/// Export Thorium images and pipelines into a toolbox directory
#[derive(Parser, Debug)]
pub struct ExportToolbox {
    /// Export all images and pipelines from this group
    #[clap(short = 'g', long = "group")]
    pub group: Option<String>,
    /// Export specific pipelines (format: group/name, or just name if --group is set).
    /// Images referenced by exported pipelines are auto-included.
    #[clap(short = 'p', long = "pipeline")]
    pub pipelines: Vec<String>,
    /// Export specific standalone images (format: group/name, or just name if --group is set)
    #[clap(short = 'i', long = "image")]
    pub images: Vec<String>,
    /// Override the group in all exported configs to this value.
    /// Warns if name collisions would occur across source groups.
    #[clap(long)]
    pub group_override: Option<String>,
    /// Root directory for the exported toolbox
    #[clap(short = 'o', long = "output", default_value = "./toolbox")]
    pub output: PathBuf,
    /// Toolbox name for config.toml
    #[clap(long, default_value = "My Toolbox")]
    pub name: String,
    /// Container registry for config.toml
    #[clap(long, default_value = "")]
    pub registry: String,
    /// Skip interactive prompts — export configs as-is from Thorium
    #[clap(short = 'n', long)]
    pub non_interactive: bool,
}

/// A parsed group/name resource reference
#[derive(Debug, Clone)]
pub struct ResourceSpec {
    pub group: String,
    pub name: String,
}

impl ResourceSpec {
    /// Parse "group/name" or "name" (with a default group fallback)
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
