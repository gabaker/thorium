//! Export Thorium images and pipelines into a toolbox directory structure

use colored::Colorize;
use futures::stream::{self, StreamExt};
use http::StatusCode;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use thorium::models::{
    Image, ImageRequest, ImageVersion, NetworkPolicyRequest, Pipeline, PipelineRequest,
};
use thorium::{CtlConf, Error, Thorium};
use url::Url;

use super::init::{
    generate_image_manifest, generate_pipeline_manifest, render_config_toml,
    validate_relative_subpath,
};
use super::manifest::{self, ToolboxManifest};
use super::{build, collisions, policies, shared};
use crate::args::Args;
use crate::args::toolbox::{BuildToolbox, ExportToolbox, ResourceSpec};
use crate::handlers::container;
use crate::handlers::exports::{self, DiskConflictResolver, WriteOutcome};
use crate::handlers::imports::editor::{resolve_editor, review_config_in_editor};
use crate::handlers::progress::{Bar, BarKind};
use crate::utils;
use crate::utils::images::list_all_images;
use crate::utils::pipelines::list_all_pipelines;

/// Render an image's version as a toolbox version label, defaulting to "latest"
///
/// # Arguments
///
/// * `version` - The image version to render, or `None` for the default label
fn version_label(version: Option<&ImageVersion>) -> String {
    match version {
        Some(ImageVersion::SemVer(v)) => v.to_string(),
        Some(ImageVersion::Custom(s)) => s.clone(),
        None => "latest".to_string(),
    }
}

/// Render a count followed by the singular or plural form of a noun
///
/// # Arguments
///
/// * `count` - The number of items
/// * `singular` - The noun used when `count` is exactly one
/// * `plural` - The noun used for every other count
fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    // pick the noun form that agrees with the count
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

/// Normalize a toolbox-relative directory to the forward-slash form `build` records in toolbox.json
///
/// Splits on both separators and drops empty and `.` segments, so `./images/x/`, `images\x`, and
/// `images/x` all compare equal to the `dir` values recorded by `build`.
///
/// # Arguments
///
/// * `dir` - The relative directory to normalize
fn normalize_rel_dir(dir: &str) -> String {
    // split on either separator and keep only the meaningful segments
    dir.split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Normalize a description to the form `toolbox build` embeds from `description.md`
///
/// `build` trims trailing whitespace from `description.md` before it overrides the inline
/// description, so export writes that same trimmed form into both files; otherwise a description
/// ending in a newline would never round-trip and every re-export would report a difference.
///
/// # Arguments
///
/// * `description` - The description fetched from Thorium
fn normalize_description(description: Option<&str>) -> Option<String> {
    // trim only trailing whitespace, matching build's handling of description.md
    description.map(|text| text.trim_end().to_string())
}

/// The kind of toolbox resource being exported
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ResourceKind {
    /// A Thorium image
    Image,
    /// A Thorium pipeline
    Pipeline,
}

impl ResourceKind {
    /// The lowercase label used for this kind in messages
    fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Image => "image",
            ResourceKind::Pipeline => "pipeline",
        }
    }
}

// ─── Resource Resolution ─────────────────────────────────────────────────────

/// Resolves the images and pipelines an export run should write
///
/// Supports a full-group export (every image and pipeline in a group) and a targeted
/// export of named pipelines/images, deduplicating so a pipeline-referenced image and
/// a standalone `--images` selection never fetch the same image twice. A pipeline-referenced
/// image that no longer exists is warned about and left out, so the pipeline is dropped by the
/// structural validation later, exactly like a whole-group export handles the same broken state.
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to fetch resources
/// * `cmd` - The export args (group and/or named pipelines/images)
/// * `workers` - The number of concurrent fetches to run
/// * `progress` - The progress bar, for missing-dependency warnings
#[allow(clippy::too_many_lines)]
async fn resolve_resources(
    thorium: &Thorium,
    cmd: &ExportToolbox,
    workers: usize,
    progress: &Bar,
) -> Result<(Vec<Image>, Vec<Pipeline>), Error> {
    // buffer_unordered with 0 never polls anything, so clamp to at least one worker
    let workers = workers.max(1);
    let mut images: Vec<Image> = Vec::new();
    let mut pipelines: Vec<Pipeline> = Vec::new();
    // tracks which image identities we've already queued so a pipeline-referenced
    // image and a standalone --image (or two pipelines) don't fetch it twice
    let mut seen_images: HashSet<(String, String)> = HashSet::new();
    // Full group export: list images and pipelines concurrently
    if let Some(group) = &cmd.group
        && cmd.pipelines.is_empty()
        && cmd.images.is_empty()
    {
        let (group_images, group_pipelines) = futures::try_join!(
            list_all_images(thorium, group),
            list_all_pipelines(thorium, group)
        )?;
        // seed the dedup set with every group image so a later --pipeline that references
        // one of them doesn't re-fetch it
        for img in group_images {
            seen_images.insert((img.group.clone(), img.name.clone()));
            images.push(img);
        }
        pipelines.extend(group_pipelines);
    }
    // Specific pipelines: fetch concurrently, then auto-resolve referenced images
    if !cmd.pipelines.is_empty() {
        // parse each "group/name" (falling back to --group) up front so a malformed spec
        // fails before any network calls are issued
        let specs = cmd
            .pipelines
            .iter()
            .map(|s| ResourceSpec::parse(s, cmd.group.as_deref()).map_err(Error::new))
            .collect::<Result<Vec<_>, _>>()?;
        // fetch the named pipelines bounded-parallel; each result keeps the error context
        // (group/name) so a failure points at the offending spec
        let fetched: Vec<Result<Pipeline, Error>> = stream::iter(specs)
            .map(|spec| async move {
                thorium
                    .pipelines
                    .get(&spec.group, &spec.name)
                    .await
                    .map_err(|e| {
                        Error::new(format!(
                            "Failed to get pipeline '{}': {e}",
                            utils::resource_id(&spec.group, &spec.name)
                        ))
                    })
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        // walk every fetched pipeline's order and record each referenced image identity
        // exactly once, scoping the lookup to the pipeline's own group
        let mut referenced: Vec<(String, String)> = Vec::new();
        for pipeline in fetched {
            // propagate the first fetch error here rather than during the concurrent stream
            // so partial successes are still surfaced as a single failure
            let pipeline = pipeline?;
            for image_name in pipeline.order.iter().flatten() {
                let key = (pipeline.group.clone(), image_name.clone());
                // insert returns false when the identity was already queued, skipping the
                // duplicate fetch
                if seen_images.insert(key.clone()) {
                    referenced.push(key);
                }
            }
            pipelines.push(pipeline);
        }
        // pull in every image a pipeline depends on so the exported toolbox is self-contained,
        // keeping each identity beside its result so a failure can be reported precisely
        let fetched_images: Vec<(String, String, Result<Image, Error>)> = stream::iter(referenced)
            .map(|(group, name)| async move {
                let result = thorium.images.get(&group, &name).await;
                (group, name, result)
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        for (group, name, result) in fetched_images {
            match result {
                Ok(image) => images.push(image),
                // a referenced image that no longer exists leaves its pipelines broken; warn and let
                // the structural validation drop them, as a whole-group export does
                Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => {
                    progress.warning(format!(
                        "Image '{}' (referenced by a pipeline) was not found; pipelines that run \
                         it will be skipped",
                        utils::resource_id(&group, &name)
                    ));
                }
                // any other failure (transport, auth, server) is fatal rather than silently
                // producing a partial toolbox
                Err(err) => {
                    return Err(Error::new(format!(
                        "Failed to get image '{}' (referenced by a pipeline): {err}",
                        utils::resource_id(&group, &name)
                    )));
                }
            }
        }
    }
    // Specific standalone images: dedup against everything already queued, then fetch concurrently
    let standalone = cmd
        .images
        .iter()
        .map(|s| ResourceSpec::parse(s, cmd.group.as_deref()).map_err(Error::new))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        // keep only identities not already queued by a group export or a pipeline reference
        .filter_map(|spec| {
            let key = (spec.group, spec.name);
            seen_images.insert(key.clone()).then_some(key)
        })
        .collect::<Vec<_>>();
    let fetched_standalone: Vec<Result<Image, Error>> = stream::iter(standalone)
        .map(|(group, name)| async move {
            thorium.images.get(&group, &name).await.map_err(|e| {
                Error::new(format!(
                    "Failed to get image '{}': {e}",
                    utils::resource_id(&group, &name)
                ))
            })
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    // a standalone image the user explicitly named that can't be fetched is fatal
    for image in fetched_standalone {
        images.push(image?);
    }
    // nothing was selected by any of the three paths: fail loudly rather than write an empty toolbox
    if images.is_empty() && pipelines.is_empty() {
        return Err(Error::new(
            "No resources to export. Specify --group, --pipelines, or --images.",
        ));
    }
    Ok((images, pipelines))
}

// ─── Toolbox-wide settings ───────────────────────────────────────────────────

/// The toolbox-wide settings written into the exported `config.toml`
///
/// Sourced from the output's preserved `config.toml`, a seeding `--config`, or the
/// `--name`/`--registry` flags.
struct ToolboxSettings {
    /// Human-readable toolbox name
    name: String,
    /// Primary container registry, unset when the toolbox declares no central one
    registry: Option<String>,
    /// Extra registries to additionally tag images for
    registries: Vec<String>,
    /// Default registry base path bundled images push under on import
    image_path_prefix: Option<String>,
    /// Directory (relative to the output root) image tool dirs are written under; `None` = `images`
    export_image_path: Option<String>,
    /// Directory (relative to the output root) pipeline tool dirs are written under; `None` = `pipelines`
    export_pipeline_path: Option<String>,
    /// Whether the toolbox bundles image tarballs (driven by `--with-images`)
    bundled_images: bool,
    /// The toolbox-wide default base-image config, preserved from a reused config
    base_image: Option<build::BaseImage>,
}

/// The first registry `toolbox build` derives image tags under, if the settings declare one
///
/// Mirrors build's registry list: the primary `registry` followed by `registries`, skipping empty
/// entries.
///
/// # Arguments
///
/// * `settings` - The resolved toolbox-wide settings
fn primary_registry(settings: &ToolboxSettings) -> Option<&str> {
    // walk the primary registry then the extras, taking the first non-empty one
    settings
        .registry
        .iter()
        .chain(settings.registries.iter())
        .map(String::as_str)
        .find(|registry| !registry.is_empty())
}

/// The image url `toolbox build` derives for an image that has no pinned url
///
/// Mirrors build's default tag shape `<registry>/[<image_path_prefix>/]<name>:<version>` (export's
/// auto-build never uses `--use-image-path` or a tag suffix). `None` when no registry is configured.
///
/// # Arguments
///
/// * `settings` - The resolved toolbox-wide settings
/// * `name` - The tool name (the tag leaf)
/// * `version` - The toolbox version label (the tag)
fn derived_image_url(settings: &ToolboxSettings, name: &str, version: &str) -> Option<String> {
    // no registry means build derives nothing
    let registry = primary_registry(settings)?;
    // insert the optional path prefix between the registry and the leaf
    let path = match settings.image_path_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}/{name}"),
        _ => name.to_string(),
    };
    Some(format!("{registry}/{path}:{version}"))
}

/// The image config as written to `<name>.json`
///
/// With `--strip-registry` a set `image` url is written empty (the form `init` scaffolds) so a
/// rebuild derives it from `config.toml`; an unset url stays unset.
///
/// # Arguments
///
/// * `config` - The resolved image config
/// * `strip_registry` - Whether `--strip-registry` is set
fn disk_image_config(config: &ImageRequest, strip_registry: bool) -> ImageRequest {
    let mut written = config.clone();
    // clear only a url that is present; `None` has nothing to strip
    if strip_registry && written.image.is_some() {
        written.image = Some(String::new());
    }
    written
}

/// The image config as `toolbox build` embeds it in toolbox.json, for the unchanged comparison
///
/// Without `--strip-registry` build pins the exported url, so the config is unchanged. With it, the
/// cleared url is replaced by the one build derives from `config.toml`.
///
/// # Arguments
///
/// * `config` - The resolved image config
/// * `strip_url` - The url build derives under `--strip-registry`, or `None` when not stripping
fn toolbox_image_config(config: &ImageRequest, strip_url: Option<&str>) -> ImageRequest {
    let mut expected = config.clone();
    // a stripped url is re-derived by build; an unset url stays unset
    if let Some(url) = strip_url
        && expected.image.is_some()
    {
        expected.image = Some(url.to_string());
    }
    expected
}

/// Lexically normalize a path by folding `.` and `..` components without touching the filesystem
///
/// Used to compare a placement destination against the toolbox root when neither directory need
/// exist yet, so `..` can't be resolved by `canonicalize`. A `..` cancels a preceding normal
/// component; one at a relative root is kept (it still escapes), and one just past an absolute root
/// is dropped (it can't go above root). This is purely lexical — it does not follow symlinks.
///
/// # Arguments
///
/// * `path` - The path to normalize
fn lexical_normalize(path: &Path) -> PathBuf {
    // fold components onto a stack so a trailing `..` can pop the previous normal segment
    let mut stack: Vec<std::path::Component> = Vec::new();
    for comp in path.components() {
        match comp {
            // a bare `.` contributes nothing to the resolved path
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(std::path::Component::Normal(_)) = stack.last() {
                    // cancel the preceding real directory
                    stack.pop();
                } else if !matches!(
                    stack.last(),
                    Some(std::path::Component::RootDir | std::path::Component::Prefix(_))
                ) {
                    // keep a `..` that has no normal component to cancel (a relative-root escape);
                    // drop one sitting right on an absolute root, which can't go higher
                    stack.push(comp);
                }
            }
            // root, prefix, and normal components carry through verbatim
            other => stack.push(other),
        }
    }
    // reassemble the folded components into a path
    let mut out = PathBuf::new();
    for comp in stack {
        out.push(comp.as_os_str());
    }
    out
}

/// Whether two paths name the same file, falling back to a lexical comparison when either is missing
///
/// # Arguments
///
/// * `a` - The first path
/// * `b` - The second path
fn same_path(a: &Path, b: &Path) -> bool {
    // prefer the filesystem's view (resolves symlinks and relative forms); fall back to lexical
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => lexical_normalize(a) == lexical_normalize(b),
    }
}

/// Resolve a per-resource `=dest` placement to a directory relative to the toolbox root
///
/// A relative `dest` is interpreted against the toolbox root; an absolute one is taken as given. Both
/// it and the root are made absolute (against the current directory) and lexically normalized, then
/// the dest is re-expressed relative to the root with forward slashes (the form `build` records in
/// toolbox.json, on every platform). The placement MUST land inside the toolbox — `build` crawls the
/// output tree, so files written outside it would never be discovered — so a dest that resolves
/// outside (or onto the root itself) is a hard error with an actionable message.
///
/// # Arguments
///
/// * `output` - The resolved toolbox output directory (the toolbox root)
/// * `dest` - The raw `=dest` string from the selection
fn resolve_dest_within(output: &Path, dest: &str) -> Result<String, Error> {
    // both paths are made absolute against the working directory so a relative output and a relative
    // dest are compared on equal footing
    let cwd = std::env::current_dir()
        .map_err(|e| Error::new(format!("Failed to read the current directory: {e}")))?;
    let make_abs = |path: &Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };
    let output_abs = lexical_normalize(&make_abs(output));
    let dest_path = Path::new(dest);
    // a relative dest is rooted at the toolbox; an absolute dest is taken literally
    let dest_abs = if dest_path.is_absolute() {
        lexical_normalize(dest_path)
    } else {
        lexical_normalize(&output_abs.join(dest_path))
    };
    // re-express the dest relative to the toolbox root; anything that won't strip (or strips to
    // empty, i.e. the root itself) is outside the toolbox and can't be a valid placement
    match dest_abs.strip_prefix(&output_abs) {
        Ok(rel) if !rel.as_os_str().is_empty() => Ok(rel
            .components()
            .map(|comp| comp.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")),
        _ => Err(Error::new(format!(
            "destination '{dest}' resolves outside the toolbox root '{}'; a placement path must \
             point to a subdirectory inside the toolbox (build only includes files under it)",
            output.display()
        ))),
    }
}

/// The per-resource `=dest` placements of an export run, split by kind
#[derive(Debug, Default)]
struct DestOverrides {
    /// Image placements keyed by the selected `(group, name)`
    images: HashMap<(String, String), String>,
    /// Pipeline placements keyed by the selected `(group, name)`
    pipelines: HashMap<(String, String), String>,
}

/// Resolve the per-resource `=dest` placements from the `--images`/`--pipelines` selections
///
/// Returns each kind's placements keyed by the selected `(group, name)`, each a directory **relative
/// to the toolbox root** (the stored/reconciliation form) as resolved by [`resolve_dest_within`].
/// Images and pipelines are kept apart, so a `=dest` on a pipeline never redirects a same-named
/// image and vice versa. A resource without an explicit `=dest` is absent and falls back to the
/// existing/configured/default layout; an auto-pulled dependency image is only redirected when it is
/// also named in `--images` with a `=dest`. Giving the same resource two different destinations, or
/// a dest that resolves outside the toolbox, is a hard error.
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `output` - The resolved toolbox output directory
fn resolve_dest_overrides(cmd: &ExportToolbox, output: &Path) -> Result<DestOverrides, Error> {
    let mut overrides = DestOverrides::default();
    // walk each kind's selections into its own map
    for (kind, specs) in [
        (ResourceKind::Image, &cmd.images),
        (ResourceKind::Pipeline, &cmd.pipelines),
    ] {
        let map = match kind {
            ResourceKind::Image => &mut overrides.images,
            ResourceKind::Pipeline => &mut overrides.pipelines,
        };
        for spec in specs {
            // resolve_resources already parsed and validated every spec, so a parse failure here is
            // unexpected; surface it rather than silently dropping the placement
            let parsed = ResourceSpec::parse(spec, cmd.group.as_deref()).map_err(Error::new)?;
            let Some(dest) = parsed.dest else {
                continue;
            };
            let dest = resolve_dest_within(output, &dest)?;
            // the same resource named twice with different placements is ambiguous
            if let Some(previous) = map.get(&(parsed.group.clone(), parsed.name.clone()))
                && previous != &dest
            {
                return Err(Error::new(format!(
                    "{} '{}' is given two different destinations ('{previous}' and '{dest}')",
                    kind.as_str(),
                    utils::resource_id(&parsed.group, &parsed.name)
                )));
            }
            map.insert((parsed.group, parsed.name), dest);
        }
    }
    Ok(overrides)
}

/// Load the existing toolbox manifest at the output for append reconciliation
///
/// When the output is a toolbox (has a `config.toml`), the on-disk tool manifests are crawled first:
/// they are the source of truth, and `toolbox.json` is derived output that goes stale as soon as a
/// tool directory is edited, moved, or deleted without a rebuild. `toolbox.json` is only used when
/// the crawl fails (with a warning) or there is no `config.toml`. Returns `None` for a fresh export
/// (neither source available).
///
/// # Arguments
///
/// * `output` - The resolved toolbox output directory
/// * `progress` - The progress bar, for the reconciliation notice and crawl-failure warning
async fn load_existing_manifest(output: &Path, progress: &Bar) -> Option<ToolboxManifest> {
    let config_path = output.join("config.toml");
    let json_path = output.join("toolbox.json");
    // crawl the on-disk tool manifests into the same shape as toolbox.json when this is a toolbox
    if config_path.exists() {
        // build walks with synchronous std::fs, so run it off the async runtime. This is an
        // index-only crawl: `use_image_path`/`tag_suffix` only affect derived tags (irrelevant to the
        // reconcile identity) and `output: None` because nothing is written
        let build_cmd = BuildToolbox {
            config: config_path,
            use_image_path: false,
            output: None,
            path: Some(output.to_path_buf()),
            tag_suffix: None,
        };
        let crawled = tokio::task::spawn_blocking(move || build::build_in_memory(&build_cmd))
            .await
            .map_err(|err| Error::new(format!("the crawl task failed: {err}")))
            .and_then(|result| result)
            .and_then(|value| {
                serde_json::from_value::<ToolboxManifest>(value).map_err(|err| {
                    Error::new(format!("the crawled manifest could not be parsed: {err}"))
                })
            });
        match crawled {
            Ok(existing) => {
                progress.info_anonymous(format!(
                    "Reconciling against the tool manifests under '{}'",
                    output.display()
                ));
                return Some(existing);
            }
            // a broken tree shouldn't silently turn an append into a fresh export; say so
            Err(err) => progress.warning(format!(
                "Failed to read the tool manifests under '{}': {err}; falling back to toolbox.json \
                 (if any) for reconciliation",
                output.display()
            )),
        }
    }
    // fall back to the committed toolbox.json when it reads and parses
    if let Ok(bytes) = tokio::fs::read(&json_path).await
        && let Ok(existing) = serde_json::from_slice::<ToolboxManifest>(&bytes)
    {
        progress.info_anonymous(format!(
            "Reconciling against toolbox.json at '{}'",
            json_path.display()
        ));
        return Some(existing);
    }
    None
}

/// The reconciliation index for one resource kind: `(group, name, version)` → `(canonical JSON, dir)`
type ExactIndex = HashMap<(String, String, String), (String, String)>;

/// Index an already-loaded toolbox manifest for append reconciliation
///
/// Returns `(images, pipelines)` maps keyed by `(group, name, version)`, each value the resource's
/// `(canonical-config JSON, on-disk dir)`. The canonical JSON drives the unchanged/differs comparison
/// and the recorded dir (normalized to forward slashes) lets the write pass update a resource in
/// place where it already lives. Empty when `existing` is `None` (a fresh export).
///
/// # Arguments
///
/// * `existing` - The loaded existing toolbox manifest, or `None` for a fresh export
fn index_existing(existing: Option<&ToolboxManifest>) -> (ExactIndex, ExactIndex) {
    let mut images = HashMap::new();
    let mut pipelines = HashMap::new();
    // no existing toolbox → empty indexes (fresh export)
    let Some(existing) = existing else {
        return (images, pipelines);
    };
    // index each embedded image config by identity, keeping its canonical JSON (for the unchanged
    // comparison) and recorded dir (to update it in place / catch a cross-directory duplicate)
    for image in existing.images.values() {
        for (version, entry) in &image.versions {
            if let Some(config) = &entry.config
                && let Ok(json) = crate::utils::canonical_json(config)
            {
                images.insert(
                    (config.group.clone(), config.name.clone(), version.clone()),
                    (json, normalize_rel_dir(&entry.dir)),
                );
            }
        }
    }
    // index each pipeline the same way as images, keyed by identity with its recorded dir
    for pipeline in existing.pipelines.values() {
        for (version, entry) in &pipeline.versions {
            if let Some(config) = &entry.config
                && let Ok(json) = crate::utils::canonical_json(config)
            {
                pipelines.insert(
                    (config.group.clone(), config.name.clone(), version.clone()),
                    (json, normalize_rel_dir(&entry.dir)),
                );
            }
        }
    }
    (images, pipelines)
}

/// Collect the unique `(group, name)` identities of every image and pipeline in a toolbox manifest
///
/// Used by the no-selection "refresh all" export to know which tools to re-fetch from Thorium.
/// Deduplicates across version entries (a tool present at multiple versions is fetched once), reads
/// the identity from each entry's embedded config, and returns each list sorted.
///
/// # Arguments
///
/// * `existing` - The loaded existing toolbox manifest
#[allow(clippy::type_complexity)]
fn existing_resource_ids(
    existing: &ToolboxManifest,
) -> (Vec<(String, String)>, Vec<(String, String)>) {
    // collect each kind's identities into a sorted set, which dedups across versions
    let images: BTreeSet<(String, String)> = existing
        .images
        .values()
        .flat_map(|image| image.versions.values())
        .filter_map(|entry| entry.config.as_ref())
        .map(|config| (config.group.clone(), config.name.clone()))
        .collect();
    let pipelines: BTreeSet<(String, String)> = existing
        .pipelines
        .values()
        .flat_map(|pipeline| pipeline.versions.values())
        .filter_map(|entry| entry.config.as_ref())
        .map(|config| (config.group.clone(), config.name.clone()))
        .collect();
    (
        images.into_iter().collect(),
        pipelines.into_iter().collect(),
    )
}

/// The resources fetched for a no-selection refresh, plus the tools that could not be fetched
struct RefreshedResources {
    /// The images re-fetched from Thorium
    images: Vec<Image>,
    /// The pipelines re-fetched from Thorium
    pipelines: Vec<Pipeline>,
    /// A description of each tool that could not be fetched (`<kind> '<group>/<name>' (<error>)`)
    failures: Vec<String>,
}

/// Re-fetch every image and pipeline a toolbox already contains, for a no-selection refresh export
///
/// Enumerates the toolbox's `(group, name)` tools (see [`existing_resource_ids`]) and fetches each from
/// Thorium bounded by `workers`. A tool that fails to fetch is warned about, left unchanged on disk,
/// and recorded in [`RefreshedResources::failures`] so the run exits non-zero. When nothing at all
/// can be fetched the refresh is an error, since the run would otherwise report success while doing
/// nothing.
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to fetch resources
/// * `existing` - The loaded existing toolbox manifest to refresh
/// * `workers` - The number of concurrent fetches to run
/// * `progress` - The progress bar, for the refresh notice and per-tool skip warnings
async fn refresh_existing_resources(
    thorium: &Thorium,
    existing: &ToolboxManifest,
    workers: usize,
    progress: &Bar,
) -> Result<RefreshedResources, Error> {
    // buffer_unordered with 0 never polls anything, so clamp to at least one worker
    let workers = workers.max(1);
    let (image_ids, pipeline_ids) = existing_resource_ids(existing);
    let total = image_ids.len() + pipeline_ids.len();
    progress.info_anonymous(format!(
        "Refreshing {} and {} already in the toolbox",
        pluralize(image_ids.len(), "image", "images"),
        pluralize(pipeline_ids.len(), "pipeline", "pipelines")
    ));
    let mut failures: Vec<String> = Vec::new();
    // fetch the images bounded-parallel; a tool that can't be fetched is warned and recorded
    let mut images: Vec<Image> = Vec::new();
    let fetched_images = stream::iter(image_ids)
        .map(|(group, name)| async move {
            let result = thorium.images.get(&group, &name).await;
            (group, name, result)
        })
        .buffer_unordered(workers)
        .collect::<Vec<_>>()
        .await;
    for (group, name, result) in fetched_images {
        match result {
            Ok(image) => images.push(image),
            Err(err) => {
                // name the image the same way in the warning and the failure summary
                let id = utils::resource_id(&group, &name);
                progress.warning(format!(
                    "Failed to fetch image '{id}' from Thorium: {err}; leaving its existing files \
                     unchanged"
                ));
                failures.push(format!("image '{id}' ({err})"));
            }
        }
    }
    // the same fetch for pipelines
    let mut pipelines: Vec<Pipeline> = Vec::new();
    let fetched_pipelines = stream::iter(pipeline_ids)
        .map(|(group, name)| async move {
            let result = thorium.pipelines.get(&group, &name).await;
            (group, name, result)
        })
        .buffer_unordered(workers)
        .collect::<Vec<_>>()
        .await;
    for (group, name, result) in fetched_pipelines {
        match result {
            Ok(pipeline) => pipelines.push(pipeline),
            Err(err) => {
                // name the pipeline the same way in the warning and the failure summary
                let id = utils::resource_id(&group, &name);
                progress.warning(format!(
                    "Failed to fetch pipeline '{id}' from Thorium: {err}; leaving its existing files \
                     unchanged"
                ));
                failures.push(format!("pipeline '{id}' ({err})"));
            }
        }
    }
    // nothing could be fetched: a refresh that does nothing must not look like a success
    if images.is_empty() && pipelines.is_empty() && total > 0 {
        return Err(Error::new(format!(
            "Failed to refresh the toolbox: none of its {} could be fetched from Thorium. If the \
             toolbox was exported with --group-override, its recorded groups may not exist in \
             Thorium; re-export with --group <source-group> --group-override <toolbox-group> instead",
            pluralize(total, "tool", "tools")
        )));
    }
    Ok(RefreshedResources {
        images,
        pipelines,
        failures,
    })
}

/// Build a `(group, name) → dir` lookup from a reconciliation index keyed by `(group, name, version)`
///
/// Lets the write pass find where a tool already lives by name (regardless of version), so a
/// re-export updates it in place instead of writing a duplicate at the default layout. When a tool
/// has versions recorded at different directories (an unusual, near-malformed toolbox) an arbitrary
/// non-empty dir wins (the index is a `HashMap`, so iteration order isn't stable) — `build`'s
/// duplicate check still guards a true conflict.
///
/// # Arguments
///
/// * `index` - The reconciliation index (`(group, name, version) → (json, dir)`)
fn dirs_by_name(index: &ExactIndex) -> HashMap<(String, String), String> {
    let mut dirs = HashMap::new();
    for ((group, name, _version), (_json, dir)) in index {
        // skip empty dirs (older toolboxes predating the field); keep one real dir per tool
        if !dir.is_empty() {
            dirs.entry((group.clone(), name.clone()))
                .or_insert_with(|| dir.clone());
        }
    }
    dirs
}

/// Build a `name → set of groups` lookup from a reconciliation index keyed by `(group, name, version)`
///
/// Lets the planner notice when an exported tool's *name* already exists in the toolbox under a
/// *different* group — the tell-tale of a group rename (e.g. a tool that is `static2/<name>` in Thorium
/// but `static1/<name>` in the toolbox) where the user forgot `--group-override`. The group set is
/// sorted so the warning lists them deterministically.
///
/// # Arguments
///
/// * `index` - The reconciliation index (`(group, name, version) → (json, dir)`)
fn groups_by_name(index: &ExactIndex) -> HashMap<String, BTreeSet<String>> {
    let mut by_name: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (group, name, _version) in index.keys() {
        by_name
            .entry(name.clone())
            .or_default()
            .insert(group.clone());
    }
    by_name
}

/// Build a `(name, version) → (group, dir)` lookup from a reconciliation index keyed by
/// `(group, name, version)`
///
/// This mirrors `build`'s identity for a tool — `(name, version)`, **group-independent** — so the
/// planner can refuse to lay down a second `manifest.toml` for a `(name, version)` that already lives
/// in the toolbox (under any group, at any directory). Without this, exporting a tool from a Thorium
/// group that doesn't match the toolbox's group writes a fresh copy that `build` then rejects as a
/// duplicate. A valid toolbox holds each `(name, version)` once, so the last writer wins on the off
/// chance of a pre-existing on-disk duplicate.
///
/// # Arguments
///
/// * `index` - The reconciliation index (`(group, name, version) → (json, dir)`)
fn locs_by_name_version(index: &ExactIndex) -> HashMap<(String, String), (String, String)> {
    let mut locs = HashMap::new();
    for ((group, name, version), (_json, dir)) in index {
        locs.insert(
            (name.clone(), version.clone()),
            (group.clone(), dir.clone()),
        );
    }
    locs
}

/// Every lookup the planner needs over one resource kind of the existing toolbox
#[derive(Debug, Default)]
struct ReconcileIndex {
    /// `(group, name, version)` → `(canonical config JSON, dir)`
    exact: ExactIndex,
    /// `(group, name)` → the dir the tool occupies
    dirs: HashMap<(String, String), String>,
    /// name → the groups that name appears under
    groups: HashMap<String, BTreeSet<String>>,
    /// `(name, version)` → `(group, dir)`, build's group-independent identity
    locs: HashMap<(String, String), (String, String)>,
}

impl ReconcileIndex {
    /// Build every lookup from one kind's exact index
    ///
    /// # Arguments
    ///
    /// * `exact` - The `(group, name, version)` reconciliation index
    fn new(exact: ExactIndex) -> Self {
        // derive the secondary lookups before moving the exact index in
        let dirs = dirs_by_name(&exact);
        let groups = groups_by_name(&exact);
        let locs = locs_by_name_version(&exact);
        ReconcileIndex {
            exact,
            dirs,
            groups,
            locs,
        }
    }

    /// The version label a tool already carries in the toolbox, if it can be determined
    ///
    /// Pipelines have no version in Thorium, so export would otherwise always use `latest` and never
    /// match a toolbox-authored label. Looks up `(group, name)` first (preferring `latest` when several
    /// labels exist, else the smallest), then falls back to the name alone when it carries exactly one
    /// label toolbox-wide (a group-mismatched tool that may be re-grouped).
    ///
    /// # Arguments
    ///
    /// * `group` - The tool's group
    /// * `name` - The tool's name
    fn existing_label(&self, group: &str, name: &str) -> Option<String> {
        // gather the labels recorded for this exact (group, name)
        let exact: BTreeSet<&String> = self
            .exact
            .keys()
            .filter(|(g, n, _)| g == group && n == name)
            .map(|(_, _, version)| version)
            .collect();
        if !exact.is_empty() {
            // prefer latest, otherwise the first label in sorted order
            return exact
                .iter()
                .find(|label| label.as_str() == "latest")
                .or_else(|| exact.iter().next())
                .map(|label| (*label).clone());
        }
        // fall back to the name alone when it is unambiguous toolbox-wide
        let by_name: BTreeSet<&String> = self
            .exact
            .keys()
            .filter(|(_, n, _)| n == name)
            .map(|(_, _, version)| version)
            .collect();
        (by_name.len() == 1)
            .then(|| by_name.into_iter().next().cloned())
            .flatten()
    }
}

/// The tool that occupies a directory of the existing toolbox
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirOwner {
    /// Whether the occupant is an image or a pipeline
    kind: ResourceKind,
    /// The occupant's group
    group: String,
    /// The occupant's name
    name: String,
}

/// Build a `dir → occupant` lookup over every image and pipeline of the existing toolbox
///
/// Lets the planner refuse a brand-new placement that lands in a directory another tool already
/// owns, which would otherwise overwrite that tool's files.
///
/// # Arguments
///
/// * `images` - The image reconciliation index
/// * `pipelines` - The pipeline reconciliation index
fn dir_owners(images: &ExactIndex, pipelines: &ExactIndex) -> HashMap<String, DirOwner> {
    let mut owners = HashMap::new();
    // record each tool's dir, skipping legacy entries with no recorded dir
    for (kind, index) in [
        (ResourceKind::Image, images),
        (ResourceKind::Pipeline, pipelines),
    ] {
        for ((group, name, _version), (_json, dir)) in index {
            if !dir.is_empty() {
                owners.insert(
                    dir.clone(),
                    DirOwner {
                        kind,
                        group: group.clone(),
                        name: name.clone(),
                    },
                );
            }
        }
    }
    owners
}

/// The reconciliation outcome for one resource being exported into a (possibly existing) toolbox
#[derive(Debug, PartialEq, Eq)]
enum Placement {
    /// Write a brand-new resource at this directory (a full write, manifest generated)
    New(String),
    /// The resource is already current (its config matches toolbox.json) at this directory — only
    /// files missing from disk are written, and a redundant container re-bundle is skipped
    Unchanged(String),
    /// The resource exists and differs; update it in place at this directory (`--overwrite`)
    Update(String),
    /// The resource exists and differs but `--overwrite` is not set — skip it with a warning
    SkipDiffers,
    /// An explicit `=dest` points somewhere other than where the resource already lives — skip it
    /// (relocating would leave a duplicate); carries the existing directory for the message
    SkipMove(String),
}

/// Decide where to write a resource and how to reconcile it against the existing toolbox
///
/// Target-directory precedence: an explicit `=dest` → the directory the tool already occupies (so a
/// re-export updates it in place) → the configured/default layout. A tool is "already in the toolbox"
/// when it has a recorded directory (`existing_dir`); an exact config match makes it `Unchanged`, a
/// differing one is `Update` (with `--overwrite`) or `SkipDiffers` (without). An explicit `=dest`
/// that names a different directory than the tool's home is a `SkipMove` (a second copy would fail
/// `build`'s duplicate check). Directories are compared in normalized forward-slash form.
///
/// # Arguments
///
/// * `explicit_dest` - The resolved per-resource `=dest`, if one was given
/// * `existing_dir` - The directory this tool already occupies in the toolbox, if any
/// * `existing_exact_json` - The canonical config of the exact `(group, name, version)` already in the
///   toolbox, if present
/// * `current_json` - The canonical config being exported (for the unchanged comparison)
/// * `default_rel` - The configured/default layout directory for this resource
/// * `overwrite` - Whether `--overwrite` is set
fn plan_placement(
    explicit_dest: Option<&str>,
    existing_dir: Option<&str>,
    existing_exact_json: Option<&str>,
    current_json: &str,
    default_rel: &str,
    overwrite: bool,
) -> Placement {
    // normalize every directory so equivalent spellings compare equal
    let explicit_dest = explicit_dest.map(normalize_rel_dir);
    let existing_dir = existing_dir.map(normalize_rel_dir);
    // target precedence: explicit =dest > the dir the tool already occupies > configured/default
    let target_rel = explicit_dest
        .clone()
        .or_else(|| existing_dir.clone())
        .unwrap_or_else(|| normalize_rel_dir(default_rel));
    // an explicit =dest naming a different dir than where the tool lives would create a second copy
    // (build rejects duplicate manifests), so refuse the move
    if let (Some(dest), Some(dir)) = (&explicit_dest, &existing_dir)
        && dest != dir
    {
        return Placement::SkipMove(dir.clone());
    }
    // no recorded directory → the tool isn't in the toolbox yet → brand new
    if existing_dir.is_none() {
        return Placement::New(target_rel);
    }
    // present and matching → already current
    if existing_exact_json == Some(current_json) {
        return Placement::Unchanged(target_rel);
    }
    // present but differing (a config change, or a different version into the tool's dir)
    if overwrite {
        Placement::Update(target_rel)
    } else {
        Placement::SkipDiffers
    }
}

/// The decided action for one resource, with all messages pre-rendered
///
/// This is the pure decision shared by images and pipelines: it resolves placement
/// ([`plan_placement`]), folds in build's `(name, version)` identity (the group-mismatch re-group /
/// skip) and directory ownership, and renders any warnings — so the caller only has to emit messages
/// and do I/O.
#[derive(Debug, PartialEq, Eq)]
enum WriteAction {
    /// Skip this resource, emitting this warning (a duplicate/move/differ that can't be written)
    Skip(String),
    /// Write this resource
    Write {
        /// The tool directory (relative to the toolbox root) to write into
        target_rel: String,
        /// Whether to generate `manifest.toml` from scratch — `true` only for a genuinely new tool
        full_write: bool,
        /// The group this tool is being re-grouped from, when a group-mismatched `(name, version)` is
        /// updated in place under `--overwrite`; drives the "Re-grouping" notice
        regrouped_from: Option<String>,
        /// Whether the incoming config matches the existing one (only missing files are written)
        unchanged: bool,
        /// An optional informational warning to emit before writing (a softer cross-group signal)
        soft_warn: Option<String>,
    },
}

/// Decide how to write one resource against the existing toolbox, rendering messages but doing no I/O
///
/// Mirrors `build`'s identity rule: a tool is `(name, version)` toolbox-wide, independent of group. So
/// a placement of `New` whose `(name, version)` already lives in the toolbox under a different
/// group/dir would duplicate at build time — with `--overwrite` it is re-grouped in place, otherwise
/// skipped. A `New` placement into a directory another tool already occupies is skipped too, since it
/// would overwrite that tool's files. Pure and deterministic so it can be unit-tested without a
/// client or filesystem.
///
/// # Arguments
///
/// * `kind` - Whether the resource is an image or a pipeline
/// * `group` - The incoming resource's group (already overridden)
/// * `name` - The incoming resource's name
/// * `version` - The incoming resource's toolbox version label
/// * `current_json` - The incoming config's canonical JSON (for the unchanged comparison)
/// * `explicit_dest` - The resolved per-resource `=dir`, if any
/// * `default_rel` - The configured/default layout directory for this resource
/// * `overwrite` - Whether `--overwrite` is set
/// * `index` - The existing toolbox's lookups for this resource kind
/// * `occupied` - Every directory of the existing toolbox mapped to the tool occupying it
#[allow(clippy::too_many_arguments)]
fn decide_write(
    kind: ResourceKind,
    group: &str,
    name: &str,
    version: &str,
    current_json: &str,
    explicit_dest: Option<&str>,
    default_rel: &str,
    overwrite: bool,
    index: &ReconcileIndex,
    occupied: &HashMap<String, DirOwner>,
) -> WriteAction {
    let kind_label = kind.as_str();
    // resolve where this tool goes relative to the dir it already occupies (if any)
    let placement = plan_placement(
        explicit_dest,
        index
            .dirs
            .get(&(group.to_string(), name.to_string()))
            .map(String::as_str),
        index
            .exact
            .get(&(group.to_string(), name.to_string(), version.to_string()))
            .map(|(json, _dir)| json.as_str()),
        current_json,
        default_rel,
        overwrite,
    );
    // build-identity collision: a New write whose (name, version) already lives elsewhere (a different
    // group/dir than we're writing under) would be a build-breaking duplicate. With --overwrite,
    // re-group it in place; without, skip rather than corrupt the toolbox.
    if let Placement::New(_) = &placement
        && let Some((existing_group, existing_dir)) =
            index.locs.get(&(name.to_string(), version.to_string()))
        && !existing_dir.is_empty()
    {
        if overwrite {
            return WriteAction::Write {
                target_rel: existing_dir.clone(),
                full_write: false,
                regrouped_from: Some(existing_group.clone()),
                unchanged: false,
                soft_warn: None,
            };
        }
        return WriteAction::Skip(format!(
            "{kind_label} '{}' already exists in the toolbox at '{existing_dir}' under group \
             '{existing_group}'; not writing a duplicate under group '{group}' (it would fail \
             build) — re-run with --overwrite to re-group it in place",
            utils::entry_id(None, name, version)
        ));
    }
    // directory ownership: a brand-new tool must not land in a directory another tool already owns
    if let Placement::New(target_rel) = &placement
        && let Some(owner) = occupied.get(target_rel)
        && (owner.kind != kind || owner.group != group || owner.name != name)
    {
        return WriteAction::Skip(format!(
            "{kind_label} '{}' would be written to '{target_rel}', which already holds {} '{}'; \
             not overwriting it — pass --group-override {} if they are the same tool, or give this \
             one a distinct =dir",
            utils::entry_id(Some(group), name, version),
            owner.kind.as_str(),
            utils::resource_id(&owner.group, &owner.name),
            owner.group
        ));
    }
    match placement {
        // a genuinely new tool: full write, but flag the softer cross-group rename signal (same name
        // under a different group at a different version — allowed, but likely an unbridged rename)
        Placement::New(target_rel) => {
            let soft_warn = index
                .groups
                .get(name)
                .filter(|groups| groups.iter().any(|other| other != group))
                .map(|groups| {
                    format!(
                        "{kind_label} '{name}' is being written under group '{group}' at \
                         '{target_rel}', but the toolbox already has a {kind_label} named '{name}' \
                         under group(s) {}; this adds a separate copy — pass --group-override \
                         <toolbox-group> to reconcile against the existing one",
                        groups.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                });
            WriteAction::Write {
                target_rel,
                full_write: true,
                regrouped_from: None,
                unchanged: false,
                soft_warn,
            }
        }
        Placement::Unchanged(target_rel) => WriteAction::Write {
            target_rel,
            full_write: false,
            regrouped_from: None,
            unchanged: true,
            soft_warn: None,
        },
        Placement::Update(target_rel) => WriteAction::Write {
            target_rel,
            full_write: false,
            regrouped_from: None,
            unchanged: false,
            soft_warn: None,
        },
        Placement::SkipMove(existing) => WriteAction::Skip(format!(
            "{kind_label} '{}' already exists in the toolbox at '{existing}'; not writing a second \
             copy at '{}' (it would fail build) — omit =dir to update it in place, or remove the \
             old copy first",
            utils::entry_id(Some(group), name, version),
            explicit_dest.map_or_else(String::new, normalize_rel_dir)
        )),
        Placement::SkipDiffers => WriteAction::Skip(format!(
            "{kind_label} '{}' already exists in the toolbox and differs; not updated — pass \
             --overwrite to update it",
            utils::entry_id(Some(group), name, version)
        )),
    }
}

/// One directory and identity claimed by a planned write, for the in-run conflict check
#[derive(Debug, Clone, Copy)]
struct RunClaim<'a> {
    /// Whether the claimant is an image or a pipeline
    kind: ResourceKind,
    /// The claimant's group
    group: &'a str,
    /// The claimant's name
    name: &'a str,
    /// The claimant's toolbox version label
    version: &'a str,
    /// The tool directory (relative to the toolbox root) the claimant is written to
    target_rel: &'a str,
}

/// Find the planned writes of one run that would clobber each other or break `build`
///
/// Two claims conflict when they share a `(kind, name, version)` from different groups (a toolbox
/// holds each name and version once, so `build` would reject them) or when two different tools
/// would be written into the same directory (the second would overwrite the first's files). Returns
/// one rendered message per conflict, in a deterministic order.
///
/// # Arguments
///
/// * `claims` - Every planned write's identity and target directory
fn find_run_conflicts(claims: &[RunClaim<'_>]) -> Vec<String> {
    let mut conflicts = Vec::new();
    // compare every pair once; runs are small, so the quadratic walk is fine
    for (i, a) in claims.iter().enumerate() {
        for b in &claims[i + 1..] {
            // the same tool planned twice isn't a conflict (collision resolution already deduped)
            if a.kind == b.kind && a.group == b.group && a.name == b.name {
                continue;
            }
            // build identity clash: same kind, name, and version from different groups
            if a.kind == b.kind && a.name == b.name && a.version == b.version {
                conflicts.push(format!(
                    "{}s '{}' and '{}' share the toolbox identity '{}' (a toolbox holds each name \
                     and version once)",
                    a.kind.as_str(),
                    utils::resource_id(a.group, a.name),
                    utils::resource_id(b.group, b.name),
                    utils::entry_id(None, a.name, a.version)
                ));
                continue;
            }
            // directory clash: two different tools would write into the same directory
            if normalize_rel_dir(a.target_rel) == normalize_rel_dir(b.target_rel) {
                conflicts.push(format!(
                    "{} '{}' and {} '{}' would both be written to '{}'",
                    a.kind.as_str(),
                    utils::resource_id(a.group, a.name),
                    b.kind.as_str(),
                    utils::resource_id(b.group, b.name),
                    normalize_rel_dir(a.target_rel)
                ));
            }
        }
    }
    conflicts.sort();
    conflicts
}

/// Resolve the directory the exported toolbox is written to
///
/// An explicit `--output` always wins. Otherwise the output anchors on `--config` (the toolbox that
/// config lives in) so pointing at a toolbox's `config.toml` exports into it; with neither flag the
/// default is `./toolbox` for a brand-new toolbox. Mirrors `build`'s config-anchored path resolution.
///
/// # Arguments
///
/// * `cmd` - The export command
fn resolve_output(cmd: &ExportToolbox) -> PathBuf {
    // an explicit --output wins; else the --config directory; else the create-new default
    cmd.output.clone().unwrap_or_else(|| match &cmd.config {
        Some(config) => build::config_base_dir(config),
        None => PathBuf::from("./toolbox"),
    })
}

/// The `config.toml` at the export output root, if one already exists
///
/// Its presence makes the export an append into an existing toolbox: the file becomes the settings
/// source and is preserved unless `--overwrite-config`.
///
/// # Arguments
///
/// * `output` - The resolved toolbox output directory
fn existing_config_path(output: &Path) -> Option<PathBuf> {
    let path = output.join("config.toml");
    path.exists().then_some(path)
}

/// Build [`ToolboxSettings`] from a loaded `config.toml`, with bundling driven by the run
///
/// # Arguments
///
/// * `config` - The loaded toolbox config
/// * `with_images` - Whether this run bundles images (drives `bundled_images`, never the config's claim)
fn settings_from_config(config: build::ToolboxConfig, with_images: bool) -> ToolboxSettings {
    ToolboxSettings {
        name: config.name,
        registry: config.registry,
        registries: config.registries,
        image_path_prefix: config.image_path_prefix,
        export_image_path: config.export_image_path,
        export_pipeline_path: config.export_pipeline_path,
        // bundling reflects what this run actually exports, never the reused config's claim
        bundled_images: with_images,
        base_image: config.base_image,
    }
}

/// Warn about run flags that the preserved `config.toml` contradicts
///
/// The preserved config wins unless `--overwrite-config`, so without these warnings the flags would
/// be silently ignored.
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `config` - The preserved toolbox config
/// * `progress` - The progress bar, for the warnings
fn warn_preserved_config_mismatch(
    cmd: &ExportToolbox,
    config: &build::ToolboxConfig,
    progress: &Bar,
) {
    // --with-images into a toolbox that doesn't claim to be bundled
    if cmd.with_images && !config.bundled_images {
        progress.warning(
            "--with-images is set but the existing config.toml has bundled_images = false; the \
             existing setting is kept (pass --overwrite-config to update it)",
        );
    }
    // a bundled toolbox gets no tarballs for the tools written by this run
    if !cmd.with_images && config.bundled_images {
        progress.warning(
            "The existing config.toml has bundled_images = true but --with-images is not set; \
             images written by this run get no tarball, so importing them from this toolbox will \
             fail — pass --with-images to bundle them",
        );
    }
    // a --name that differs from the kept one
    if let Some(name) = &cmd.name
        && config.name != *name
    {
        progress.warning(format!(
            "--name '{name}' differs from the existing config.toml; the existing name '{}' is kept \
             (pass --overwrite-config to update it)",
            config.name
        ));
    }
    // a --registry that differs from the kept one
    if let Some(registry) = &cmd.registry
        && config.registry.as_deref() != Some(registry.as_str())
    {
        progress.warning(format!(
            "--registry '{registry}' differs from the existing config.toml; the existing registry \
             is kept (pass --overwrite-config to update it)"
        ));
    }
}

/// The name given to a new toolbox when `--name` is not set
const DEFAULT_TOOLBOX_NAME: &str = "My Toolbox";

/// Resolve the toolbox-wide settings for an export
///
/// Priority: an existing `config.toml` at the output that is preserved (no `--overwrite-config`) is
/// the effective config, since `build` reads it afterwards — a different `--config` is then ignored
/// with a warning; else an explicit `--config` that exists (seed from another toolbox); else, with
/// `--overwrite-config`, the output's existing config with an explicit `--name`/`--registry` applied
/// on top; else the `--name`/`--registry` flags (a new toolbox, named "My Toolbox" by default). A `--config` that points at a missing file
/// warns and falls through. Bundling is always driven by the `--with-images` export action.
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `existing_config` - The output's `config.toml` if it already exists (the append source)
/// * `progress` - The progress bar, for the "using existing config" notice and mismatch warnings
fn resolve_settings(
    cmd: &ExportToolbox,
    existing_config: Option<&Path>,
    progress: &Bar,
) -> Result<ToolboxSettings, Error> {
    // (1) a preserved config.toml at the output is what build will read, so it is the source
    if let Some(path) = existing_config
        && !cmd.overwrite_config
    {
        progress.info_anonymous(format!(
            "Using the existing config.toml at '{}' for toolbox settings (pass --overwrite-config \
             to replace it)",
            path.display()
        ));
        let config = build::load_config(path)?;
        // a seed from another toolbox can't take effect while this config is kept
        if let Some(seed) = &cmd.config
            && !same_path(seed, path)
        {
            progress.warning(format!(
                "--config '{}' is ignored: the output already has a config.toml at '{}', which is \
                 kept (pass --overwrite-config to replace it with the seeded settings)",
                seed.display(),
                path.display()
            ));
        }
        warn_preserved_config_mismatch(cmd, &config, progress);
        return Ok(settings_from_config(config, cmd.with_images));
    }
    // (2) an explicit --config seeds settings from another toolbox — when it exists. A --config that
    // names a missing file means the intended toolbox isn't there (yet), so warn and fall through
    if let Some(config_path) = &cmd.config {
        if config_path.exists() {
            return Ok(settings_from_config(
                build::load_config(config_path)?,
                cmd.with_images,
            ));
        }
        progress.warning(format!(
            "config.toml not found at '{}'; creating a new toolbox there instead of appending",
            config_path.display()
        ));
    }
    // (3) --overwrite-config on an existing toolbox: keep its settings as the base, with an explicit
    // --name/--registry replacing the recorded one
    if let Some(path) = existing_config {
        let mut settings = settings_from_config(build::load_config(path)?, cmd.with_images);
        // an explicit --name replaces the recorded name
        if let Some(name) = &cmd.name {
            settings.name.clone_from(name);
        }
        // an explicit --registry replaces the recorded registry
        if let Some(registry) = &cmd.registry {
            settings.registry = Some(registry.clone());
        }
        return Ok(settings);
    }
    // (4) new toolbox: derive from flags; registries/prefix/layout/base_image have no flag
    Ok(ToolboxSettings {
        name: cmd
            .name
            .clone()
            .unwrap_or_else(|| DEFAULT_TOOLBOX_NAME.to_string()),
        registry: cmd.registry.clone(),
        registries: Vec::new(),
        image_path_prefix: None,
        export_image_path: None,
        export_pipeline_path: None,
        bundled_images: cmd.with_images,
        base_image: None,
    })
}

/// Check that the configured export layout dirs are safe relative subpaths of the toolbox root
///
/// # Arguments
///
/// * `settings` - The resolved toolbox-wide settings
fn validate_layout_paths(settings: &ToolboxSettings) -> Result<(), Error> {
    // validate each configured layout dir; an unset one uses the built-in default
    for (key, path) in [
        ("export_image_path", &settings.export_image_path),
        ("export_pipeline_path", &settings.export_pipeline_path),
    ] {
        if let Some(path) = path {
            validate_relative_subpath(&format!("config.toml's {key}"), path)?;
        }
    }
    Ok(())
}

// ─── Manifest assembly ───────────────────────────────────────────────────────

/// Build an in-memory toolbox manifest from the resolved Thorium resources
///
/// Entries are keyed by `<group>/<name>` so that same-named resources from
/// different groups stay distinct until collision resolution runs (the manifest's
/// own maps are otherwise keyed by name). Each image carries the network policy
/// definitions it references so they can be written alongside it. Descriptions are
/// normalized to the form `build` embeds (see [`normalize_description`]).
///
/// Each entry's `dir` carries the explicit `=dest` placement given for that resource
/// (empty when none). It travels with the entry through group overrides and collision
/// renames, so the write pass applies the placement to exactly the resource it was
/// given for; the authoritative per-tool dir is recorded later by the auto-build.
///
/// # Arguments
///
/// * `settings` - The resolved toolbox-wide settings (name/registry/bundling/etc.)
/// * `images` - The resolved Thorium images
/// * `pipelines` - The resolved Thorium pipelines
/// * `policies` - Fetched network policy definitions keyed by `(group, name)`
/// * `dests` - The explicit per-resource placements
fn build_manifest(
    settings: &ToolboxSettings,
    images: &[Image],
    pipelines: &[Pipeline],
    policies: &HashMap<(String, String), NetworkPolicyRequest>,
    dests: &DestOverrides,
) -> ToolboxManifest {
    let mut image_entries: HashMap<String, manifest::ImageManifest> = HashMap::new();
    // map (group, name) -> exported version label so each pipeline's image map pins
    // the version we actually exported (rather than a name-keyed guess); built in the
    // same pass as the entries so the version label is only computed once per image
    let mut image_versions: HashMap<(String, String), String> = HashMap::new();
    for image in images {
        // the version label both keys this image's manifest entry and pins it in any pipeline
        // image map below, so compute it once here
        let version = version_label(image.version.as_ref());
        // the on-disk config is the image's Thorium request form, with the description normalized
        let mut config = ImageRequest::from(image.clone());
        config.description = normalize_description(config.description.as_deref());
        // bundle the definitions of the policies this image references (looked up within the
        // image's own group), scoping each copy to the group this toolbox exports the image in
        // rather than the policy's full instance-wide group set
        let network_policies = config
            .network_policies
            .iter()
            .filter_map(|name| policies.get(&(image.group.clone(), name.clone())).cloned())
            .map(|mut policy| {
                policy.groups = vec![image.group.clone()];
                policy
            })
            .collect();
        // build_path is "./" because the manifest sits in the tool's own dir; config is embedded
        // inline (not config_from) and the bundled policies travel as definitions (not _from refs)
        let entry = manifest::ImageVersion {
            dir: dests
                .images
                .get(&(image.group.clone(), image.name.clone()))
                .cloned()
                .unwrap_or_default(),
            build_path: "./".to_string(),
            config_from: None,
            config: Some(config),
            network_policies_from: Vec::new(),
            network_policies,
        };
        // remember this image's exported version so pipelines can pin the exact version exported
        image_versions.insert((image.group.clone(), image.name.clone()), version.clone());
        // key the entry by <group>/<name> to keep same-named images from different groups
        // distinct until collision resolution collapses/renames them
        image_entries.insert(
            format!("{}/{}", image.group, image.name),
            manifest::ImageManifest {
                versions: HashMap::from([(version, entry)]),
            },
        );
    }
    let mut pipeline_entries: HashMap<String, manifest::PipelineManifest> = HashMap::new();
    for pipeline in pipelines {
        // flatten the (possibly staged) order into the unique image names this pipeline runs,
        // pinning each to the version we actually exported it under (falling back to "latest"
        // when the image wasn't part of this export, e.g. it lives outside the selection)
        let images_map = pipeline
            .order
            .iter()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|name| {
                let version = image_versions
                    .get(&(pipeline.group.clone(), name.clone()))
                    .cloned()
                    .unwrap_or_else(|| "latest".to_string());
                (name, manifest::PipelineImage { version })
            })
            .collect();
        // the on-disk config is the pipeline's request form, with the description normalized
        let mut config = PipelineRequest::from(pipeline.clone());
        config.description = normalize_description(config.description.as_deref());
        // pipelines carry no version axis in Thorium, so every entry is keyed "latest" here (the
        // planner swaps in a toolbox-authored label when the pipeline already has one)
        let entry = manifest::PipelineVersion {
            dir: dests
                .pipelines
                .get(&(pipeline.group.clone(), pipeline.name.clone()))
                .cloned()
                .unwrap_or_default(),
            description: config.description.clone().unwrap_or_default(),
            images: images_map,
            config_from: None,
            config: Some(config),
        };
        // key by <group>/<name> for the same group-distinctness reason as images above
        pipeline_entries.insert(
            format!("{}/{}", pipeline.group, pipeline.name),
            manifest::PipelineManifest {
                versions: HashMap::from([("latest".to_string(), entry)]),
            },
        );
    }
    ToolboxManifest {
        name: settings.name.clone(),
        registry: settings.registry.clone(),
        images: image_entries,
        pipelines: pipeline_entries,
        bundled_images: settings.bundled_images,
        image_path_prefix: settings.image_path_prefix.clone(),
    }
}

// ─── Manifest patching ───────────────────────────────────────────────────────

/// The kind of one logical item in a TOML document's line structure
#[derive(Debug, PartialEq, Eq)]
enum TomlLine {
    /// A `key = value` pair (possibly spanning several lines); carries the raw key text
    Key(String),
    /// A `[table]` or `[[array]]` header; carries the header path text
    Header(String),
    /// A blank line, a comment, or anything else
    Other,
}

/// One logical item of a TOML document and the half-open range of source lines it spans
#[derive(Debug)]
struct TomlItem {
    /// What this item is
    kind: TomlLine,
    /// The first source line of the item
    start: usize,
    /// One past the last source line of the item
    end: usize,
}

/// Tracks whether a TOML value is still open across lines (an array/inline table or a
/// multi-line string)
#[derive(Debug, Default)]
struct ValueScan {
    /// The number of unclosed `[`/`{` brackets
    depth: i32,
    /// The delimiter of an unclosed multi-line string, if inside one
    multiline: Option<&'static [u8]>,
}

impl ValueScan {
    /// Scan one line (or line fragment) of a value, updating the open state
    ///
    /// # Arguments
    ///
    /// * `text` - The text to scan
    fn feed(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // inside a multi-line string only its closing delimiter (or an escape) matters
            if let Some(delim) = self.multiline {
                if bytes[i..].starts_with(delim) {
                    self.multiline = None;
                    i += delim.len();
                } else if delim == b"\"\"\"" && bytes[i] == b'\\' {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            match bytes[i] {
                // a multi-line basic or literal string opens
                b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                    self.multiline = Some(b"\"\"\"");
                    i += 3;
                }
                b'\'' if bytes[i..].starts_with(b"'''") => {
                    self.multiline = Some(b"'''");
                    i += 3;
                }
                // a single-line basic string: skip to its unescaped closing quote
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                }
                // a single-line literal string: skip to its closing quote
                b'\'' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                }
                // a comment ends the meaningful part of the line
                b'#' => break,
                // brackets and braces nest
                b'[' | b'{' => {
                    self.depth += 1;
                    i += 1;
                }
                b']' | b'}' => {
                    self.depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }

    /// Whether the value continues onto the next line
    fn open(&self) -> bool {
        self.depth > 0 || self.multiline.is_some()
    }
}

/// Split a `key = value` line into its raw key text and the value text after `=`
///
/// Returns `None` when the line isn't a key/value pair.
///
/// # Arguments
///
/// * `line` - The line with leading whitespace already trimmed
fn split_key_line(line: &str) -> Option<(String, &str)> {
    let bytes = line.as_bytes();
    // a quoted key runs to its closing quote; a bare/dotted key runs over its allowed characters
    let key_end = if bytes.first() == Some(&b'"') {
        line[1..].find('"').map(|pos| pos + 2)?
    } else {
        line.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ')))?
    };
    let key = line[..key_end].trim();
    // the key must be followed by `=`
    let rest = line[key_end..].trim_start().strip_prefix('=')?;
    (!key.is_empty()).then(|| (key.to_string(), rest))
}

/// Split a TOML document's lines into logical items (keys, table headers, and other lines)
///
/// This is a line-structure scan rather than a full parse: it only needs to know where each
/// top-level key and each table begins and ends so individual keys or tables can be replaced while
/// every other line (comments included) is kept verbatim. Multi-line arrays, inline tables, and
/// multi-line strings are followed so their continuation lines are never mistaken for headers.
///
/// # Arguments
///
/// * `lines` - The document's lines
fn toml_items(lines: &[&str]) -> Vec<TomlItem> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        // a header line opens a table (or array-of-tables)
        if trimmed.starts_with('[') {
            let inner = trimmed.trim_start_matches('[');
            let name = inner
                .split(']')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            items.push(TomlItem {
                kind: TomlLine::Header(name),
                start: i,
                end: i + 1,
            });
            i += 1;
            continue;
        }
        // a key line spans until its value closes
        if let Some((key, value)) = split_key_line(trimmed) {
            let start = i;
            let mut scan = ValueScan::default();
            scan.feed(value);
            i += 1;
            while scan.open() && i < lines.len() {
                scan.feed(lines[i]);
                i += 1;
            }
            items.push(TomlItem {
                kind: TomlLine::Key(key),
                start,
                end: i,
            });
            continue;
        }
        // anything else (blank, comment) is carried as-is
        items.push(TomlItem {
            kind: TomlLine::Other,
            start: i,
            end: i + 1,
        });
        i += 1;
    }
    items
}

/// Set, replace, or remove top-level keys of a TOML document, keeping every other line verbatim
///
/// Each update replaces the key's existing lines in place (a `None` value removes it); a key that
/// isn't present yet is inserted after the last top-level key, before the first table.
///
/// # Arguments
///
/// * `text` - The TOML document
/// * `updates` - The top-level keys to set (`Some`) or remove (`None`)
fn patch_top_level_keys(text: &str, updates: &[(&str, Option<toml::Value>)]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let items = toml_items(&lines);
    // the top-level region ends at the first table header
    let first_header = items
        .iter()
        .position(|item| matches!(item.kind, TomlLine::Header(_)))
        .unwrap_or(items.len());
    // note which updated keys already exist at the top level, and where the last top-level key is
    let present: HashSet<&str> = items[..first_header]
        .iter()
        .filter_map(|item| match &item.kind {
            TomlLine::Key(key) => Some(key.as_str()),
            _ => None,
        })
        .collect();
    let last_key = items[..first_header]
        .iter()
        .rposition(|item| matches!(item.kind, TomlLine::Key(_)));
    // render the keys that must be inserted because they don't exist yet
    let inserts: Vec<String> = updates
        .iter()
        .filter(|(key, _)| !present.contains(key))
        .filter_map(|(key, value)| value.as_ref().map(|value| format!("{key} = {value}")))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut replaced: HashSet<&str> = HashSet::new();
    // with no top-level key at all, new keys go first
    if last_key.is_none() {
        out.extend(inserts.iter().cloned());
    }
    for (idx, item) in items.iter().enumerate() {
        // a top-level key being updated is replaced once (later duplicates are dropped)
        if idx < first_header
            && let TomlLine::Key(key) = &item.kind
            && let Some((name, value)) = updates.iter().find(|(name, _)| name == key)
        {
            if replaced.insert(name)
                && let Some(value) = value
            {
                out.push(format!("{name} = {value}"));
            }
        } else {
            out.extend(
                lines[item.start..item.end]
                    .iter()
                    .map(|line| (*line).to_string()),
            );
        }
        // new keys follow the last existing top-level key
        if Some(idx) == last_key {
            out.extend(inserts.iter().cloned());
        }
    }
    let mut patched = out.join("\n");
    patched.push('\n');
    patched
}

/// Whether a table header path belongs to a pipeline manifest's `images` map
///
/// # Arguments
///
/// * `header` - The header path text (e.g. `images.clamav`)
fn is_images_header(header: &str) -> bool {
    // the first dotted segment names the root table
    header
        .split('.')
        .next()
        .is_some_and(|root| root.trim() == "images")
}

/// Replace every `images` table (and any top-level `images = ...` key) of a TOML document
///
/// All other lines are kept verbatim; the replacement tables are appended at the end.
///
/// # Arguments
///
/// * `text` - The TOML document
/// * `tables` - The rendered `[images.*]` tables to append (may be empty)
fn replace_images_tables(text: &str, tables: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let items = toml_items(&lines);
    let first_header = items
        .iter()
        .position(|item| matches!(item.kind, TomlLine::Header(_)))
        .unwrap_or(items.len());
    let mut out: Vec<&str> = Vec::new();
    // tracks whether the items being walked belong to an images table being dropped
    let mut in_images = false;
    for (idx, item) in items.iter().enumerate() {
        match &item.kind {
            // a header starts a new table: drop it (and its body) when it is an images table
            TomlLine::Header(name) => in_images = is_images_header(name),
            // a top-level inline `images = ...` map is dropped too
            TomlLine::Key(key) if idx < first_header && key == "images" => continue,
            _ => {}
        }
        if !in_images {
            out.extend(&lines[item.start..item.end]);
        }
    }
    // trim trailing blank lines so the appended tables are separated by exactly one
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    let mut patched = out.join("\n");
    patched.push('\n');
    // append the regenerated tables after a separating blank line
    if !tables.trim().is_empty() {
        patched.push('\n');
        patched.push_str(tables.trim_matches('\n'));
        patched.push('\n');
    }
    patched
}

/// Parse a TOML document into a table, rendering the error as a string
///
/// # Arguments
///
/// * `text` - The TOML document
fn parse_toml_table(text: &str) -> Result<toml::Table, String> {
    text.parse::<toml::Table>()
        .map_err(|err| format!("it is not valid TOML: {err}"))
}

/// Check that a patched manifest holds exactly the expected keys and preserved every other key
///
/// # Arguments
///
/// * `before` - The manifest before patching
/// * `patched` - The patched manifest text
/// * `expected` - The keys that were set (`Some`) or removed (`None`)
fn verify_patch(
    before: &toml::Table,
    patched: &str,
    expected: &[(&str, Option<toml::Value>)],
) -> Result<(), String> {
    let after = parse_toml_table(patched)?;
    // every updated key must hold exactly its new value (or be gone)
    for (key, value) in expected {
        if after.get(*key) != value.as_ref() {
            return Err(format!("its '{key}' key could not be updated"));
        }
    }
    // every other key must be untouched
    for (key, value) in before {
        if !expected.iter().any(|(name, _)| name == key) && after.get(key) != Some(value) {
            return Err(format!("its '{key}' key would have changed"));
        }
    }
    Ok(())
}

/// Update the export-owned keys of an existing image `manifest.toml`, preserving everything else
///
/// `version`, `exported_image_path`, and `network_policies_from` mirror Thorium state, so an in-place
/// update must refresh them or `build` would keep embedding the stale url/version/policies. Every
/// toolbox-authored key (`build`, `build_path`, `[base_image]`, `image_from`, comments, ...) is kept
/// verbatim. URL entries in `network_policies_from` are toolbox-authored and kept; local entries are
/// replaced by the policy files this export wrote. The result is verified by re-parsing it.
///
/// # Arguments
///
/// * `text` - The existing manifest
/// * `version` - The image's toolbox version label
/// * `exported_image_path` - The url to pin, or `None` to remove the pin
/// * `policy_files` - The policy files this export wrote beside the manifest
fn patch_image_manifest(
    text: &str,
    version: &str,
    exported_image_path: Option<&str>,
    policy_files: &[String],
) -> Result<String, String> {
    let before = parse_toml_table(text)?;
    // keep the toolbox-authored URL policy references after the export-written files
    let kept_urls = before
        .get("network_policies_from")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .filter(|entry| Url::parse(entry).is_ok())
        .map(str::to_string);
    let policies: Vec<toml::Value> = policy_files
        .iter()
        .cloned()
        .chain(kept_urls)
        .map(toml::Value::String)
        .collect();
    // the export-owned keys and their new values
    let updates = [
        ("version", Some(toml::Value::String(version.to_string()))),
        (
            "exported_image_path",
            exported_image_path
                .filter(|url| !url.is_empty())
                .map(|url| toml::Value::String(url.to_string())),
        ),
        (
            "network_policies_from",
            (!policies.is_empty()).then_some(toml::Value::Array(policies)),
        ),
    ];
    // patch the keys in place and make sure nothing else moved
    let patched = patch_top_level_keys(text, &updates);
    verify_patch(&before, &patched, &updates)?;
    Ok(patched)
}

/// Update the `[images.*]` tables of an existing pipeline `manifest.toml`, preserving everything else
///
/// The image map mirrors the pipeline's order in Thorium, so it is regenerated; the toolbox-authored
/// `version` label, `description`, comments, and any other keys are kept verbatim. The result is
/// verified by re-parsing it.
///
/// # Arguments
///
/// * `text` - The existing manifest
/// * `name` - The pipeline name (used to render the fresh image tables)
/// * `image_versions` - The `(image name, version)` pairs for the image map
fn patch_pipeline_manifest(
    text: &str,
    name: &str,
    image_versions: &[(String, String)],
) -> Result<String, String> {
    let before = parse_toml_table(text)?;
    // render a fresh manifest and lift its image tables out
    let generated = generate_pipeline_manifest(name, image_versions);
    let generated_lines: Vec<&str> = generated.lines().collect();
    let tables = toml_items(&generated_lines)
        .iter()
        .find(|item| matches!(&item.kind, TomlLine::Header(header) if is_images_header(header)))
        .map_or_else(String::new, |item| generated_lines[item.start..].join("\n"));
    let expected_images = parse_toml_table(&generated)?.get("images").cloned();
    // swap the image tables and make sure nothing else moved
    let patched = replace_images_tables(text, &tables);
    verify_patch(&before, &patched, &[("images", expected_images)])?;
    Ok(patched)
}

// ─── File Writing ────────────────────────────────────────────────────────────

/// How a tool's files are written, derived from its reconciled placement
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    /// A brand-new tool: every file is written and `manifest.toml` is generated
    Full,
    /// An in-place update: the config, policy, and description files are rewritten, and
    /// `manifest.toml` has only its export-owned keys patched (generated when missing)
    Update,
    /// An unchanged tool: only files missing from disk are written, and no editor review runs
    MissingOnly,
}

/// How [`write_image_entry`] writes one image's files
#[derive(Clone, Copy)]
struct ImageWriteOptions {
    /// Mark a generated manifest `build = true` (a Dockerfile sits in the image's explicit `=dir`);
    /// otherwise the reference-only manifest is written, pinned to the captured url via
    /// `exported_image_path`
    build: bool,
    /// How the image's files are written
    mode: WriteMode,
    /// Write the config's `image` url empty and omit `exported_image_path`, so a rebuild derives each
    /// image path from `config.toml` (a registry-agnostic release) instead of a pinned url
    strip_registry: bool,
    /// Open the config in an editor for review before writing
    review: bool,
}

/// Whether a file should be offered to the conflict resolver under a write mode
///
/// # Arguments
///
/// * `path` - The file about to be written
/// * `mode` - The write mode of the tool the file belongs to
fn should_write(path: &Path, mode: WriteMode) -> bool {
    // an unchanged tool only fills in files that are missing
    mode != WriteMode::MissingOnly || !path.exists()
}

/// Check that a (possibly hand-reviewed) resource name is usable as a file stem
///
/// # Arguments
///
/// * `kind` - Whether the resource is an image or a pipeline
/// * `name` - The resource name
fn validate_file_stem(kind: ResourceKind, name: &str) -> Result<(), Error> {
    // reject names that would escape the tool dir or corrupt the generated manifest
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '"') || c.is_control());
    if invalid {
        return Err(Error::new(format!(
            "Failed to write {} '{name}': the name can't be used as a file name",
            kind.as_str()
        )));
    }
    Ok(())
}

/// Write a resolved image entry to the toolbox directory, resolving on-disk conflicts
///
/// Returns the write outcome ([`WriteOutcome::Quit`] if the user asked to stop) and the config as it
/// was written — after `--strip-registry` and any `--review` edits — so the caller bundles the
/// container the user actually kept. The file stem, manifest name, `exported_image_path`, and
/// `description.md` are all derived from that final config.
///
/// # Arguments
///
/// * `image_dir` - The tool directory to write this image's files into (resolved by the caller)
/// * `config` - The resolved image request
/// * `version` - The toolbox version label to record
/// * `opts` - How to write this image (build flag, write mode, registry stripping, review)
/// * `network_policies` - The policy definitions this image references
/// * `editor` - The editor command used when `opts.review` is set
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn write_image_entry(
    image_dir: &Path,
    config: &ImageRequest,
    version: &str,
    opts: ImageWriteOptions,
    network_policies: &[NetworkPolicyRequest],
    editor: &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<(WriteOutcome, ImageRequest), Error> {
    // --strip-registry clears a set container url (the same form `init` scaffolds) so a rebuild
    // derives the path from config.toml; the exported_image_path is dropped below too
    let config = disk_image_config(config, opts.strip_registry);
    // curated (prioritized) field order so the written <name>.json matches what `init` scaffolds;
    // curated keys first, remaining keys sorted, so it stays diff-stable
    let config_json =
        crate::utils::curated_json(&config, crate::handlers::imports::merge::IMAGE_FIELD_ORDER)
            .map_err(|e| Error::new(format!("Failed to serialize image '{}': {e}", config.name)))?;
    // let the user hand-edit the config first when --review is set (never for an unchanged tool)
    let (final_json, config) = if opts.review && opts.mode != WriteMode::MissingOnly {
        // suspend the spinner while the editor owns the terminal
        let reviewed = progress
            .suspend_async(review_config_in_editor::<ImageRequest>(
                &config_json,
                &format!("export-image-{}", config.name),
                editor,
                crate::handlers::imports::merge::IMAGE_FIELD_ORDER,
            ))
            .await?;
        // re-read the reviewed config so every derived file follows the user's edits
        let parsed: ImageRequest = serde_json::from_str(&reviewed).map_err(|e| {
            Error::new(format!(
                "Failed to parse the reviewed config of image '{}': {e}",
                config.name
            ))
        })?;
        (reviewed, parsed)
    } else {
        (config_json, config)
    };
    // the final config's name is the json file stem and the manifest name
    validate_file_stem(ResourceKind::Image, &config.name)?;
    let name = config.name.clone();
    // short-circuit the whole export if the resolver prompt returns Quit at any write
    let json_path = image_dir.join(format!("{name}.json"));
    if should_write(&json_path, opts.mode)
        && resolver
            .write_yaml::<ImageRequest>(&json_path, &final_json, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok((WriteOutcome::Quit, config));
    }
    // write the definition of every network policy this image references so an
    // import can create them in instances that lack them
    let mut policy_files: Vec<String> = Vec::new();
    for policy in network_policies {
        // name each policy file <policy>.policy.json so the manifest can list it under
        // network_policies_from and the importer recognizes it by suffix
        let file_name = format!("{}.policy.json", policy.name);
        // policy files are pretty-printed (not canonical) to stay human-readable on disk
        let policy_json = serde_json::to_string_pretty(policy).map_err(|e| {
            Error::new(format!(
                "Failed to serialize network policy '{}': {e}",
                policy.name
            ))
        })?;
        let policy_path = image_dir.join(&file_name);
        if should_write(&policy_path, opts.mode)
            && resolver
                .write_yaml::<NetworkPolicyRequest>(&policy_path, &policy_json, progress)
                .await?
                == WriteOutcome::Quit
        {
            return Ok((WriteOutcome::Quit, config));
        }
        policy_files.push(file_name);
    }
    // sort so regenerated manifests don't churn on set iteration order
    policy_files.sort_unstable();
    // pin the url the image lives at unless it is rebuilt from a Dockerfile (build = true) or the
    // release is registry-agnostic (--strip-registry); both derive the path from config.toml instead
    let exported_image_path = if opts.build || opts.strip_registry {
        None
    } else {
        config.image.as_deref().filter(|url| !url.is_empty())
    };
    // generate manifest.toml for a new tool (or when it is missing); for an in-place update patch
    // only the export-owned keys so the toolbox-authored build settings survive
    let manifest_path = image_dir.join("manifest.toml");
    let existing_manifest = tokio::fs::read_to_string(&manifest_path).await.ok();
    let manifest = match (opts.mode, existing_manifest) {
        // an unchanged tool keeps its manifest untouched
        (WriteMode::MissingOnly, Some(_)) => None,
        // an update refreshes version/exported_image_path/network_policies_from in place
        (WriteMode::Update, Some(existing)) => {
            match patch_image_manifest(&existing, version, exported_image_path, &policy_files) {
                Ok(patched) => Some(patched),
                Err(reason) => {
                    progress.warning(format!(
                        "Failed to update '{}': {reason}; update its version, exported_image_path, \
                         and network_policies_from by hand",
                        manifest_path.display()
                    ));
                    None
                }
            }
        }
        // a new tool, or a missing manifest: generate the reference-only (or build = true) manifest.
        // image_name is set to the tool name: it's irrelevant while the image is pinned via
        // exported_image_path, and only matters under build = true + --use-image-path
        _ => Some(generate_image_manifest(
            &name,
            &name,
            version,
            !opts.build,
            &policy_files,
            exported_image_path,
        )),
    };
    if let Some(manifest) = manifest
        && resolver
            .write_toml::<build::ManifestToml>(&manifest_path, &manifest, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok((WriteOutcome::Quit, config));
    }
    // always write description.md so every tool carries a docs file, in the trimmed form build
    // embeds; a None/empty description is an empty file (never the literal "null"), which build
    // treats as absent. The markdown is the toolbox's source of truth.
    let description_path = image_dir.join("description.md");
    let description = normalize_description(config.description.as_deref()).unwrap_or_default();
    if should_write(&description_path, opts.mode)
        && resolver
            .write_text(&description_path, &description, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok((WriteOutcome::Quit, config));
    }
    Ok((WriteOutcome::Written, config))
}

/// Write a resolved pipeline entry to the toolbox directory, resolving on-disk
/// conflicts; returns [`WriteOutcome::Quit`] if the user asked to stop
///
/// The file stem, manifest name, and `description.md` follow the final (possibly reviewed) config.
/// A new pipeline gets a generated `manifest.toml`; an in-place update only regenerates its
/// `[images.*]` tables, keeping the toolbox-authored `version` label and `description`.
///
/// # Arguments
///
/// * `pipeline_dir` - The tool directory to write this pipeline's files into (resolved by the caller)
/// * `config` - The resolved pipeline request
/// * `image_versions` - The (image name, version) pairs for the manifest's image map
/// * `mode` - How the pipeline's files are written
/// * `review` - Open the config in an editor for review before writing
/// * `editor` - The editor command used when `review` is set
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar
#[allow(clippy::too_many_arguments)]
async fn write_pipeline_entry(
    pipeline_dir: &Path,
    config: &PipelineRequest,
    image_versions: &[(String, String)],
    mode: WriteMode,
    review: bool,
    editor: &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<WriteOutcome, Error> {
    // curated (prioritized) field order so the written <name>.json matches what `init` scaffolds.
    // The config's `description` field is serialized as-is (kept as `null` when unset, like the
    // Thorium struct) — the markdown source of truth is the description.md file written below.
    let config_json = crate::utils::curated_json(
        config,
        crate::handlers::imports::merge::PIPELINE_FIELD_ORDER,
    )
    .map_err(|e| {
        Error::new(format!(
            "Failed to serialize pipeline '{}': {e}",
            config.name
        ))
    })?;
    // let the user hand-edit the config first when --review is set (never for an unchanged tool)
    let (final_json, config) = if review && mode != WriteMode::MissingOnly {
        // suspend the spinner while the editor owns the terminal
        let reviewed = progress
            .suspend_async(review_config_in_editor::<PipelineRequest>(
                &config_json,
                &format!("export-pipeline-{}", config.name),
                editor,
                crate::handlers::imports::merge::PIPELINE_FIELD_ORDER,
            ))
            .await?;
        // re-read the reviewed config so every derived file follows the user's edits
        let parsed: PipelineRequest = serde_json::from_str(&reviewed).map_err(|e| {
            Error::new(format!(
                "Failed to parse the reviewed config of pipeline '{}': {e}",
                config.name
            ))
        })?;
        (reviewed, parsed)
    } else {
        (config_json, config.clone())
    };
    // the final config's name is the json file stem and the manifest name
    validate_file_stem(ResourceKind::Pipeline, &config.name)?;
    let name = config.name.as_str();
    // short-circuit the whole export if the resolver prompt returns Quit at any write
    let json_path = pipeline_dir.join(format!("{name}.json"));
    if should_write(&json_path, mode)
        && resolver
            .write_yaml::<PipelineRequest>(&json_path, &final_json, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    // generate the manifest for a new pipeline (or a missing one); an update only swaps its image map
    let manifest_path = pipeline_dir.join("manifest.toml");
    let existing_manifest = tokio::fs::read_to_string(&manifest_path).await.ok();
    let manifest = match (mode, existing_manifest) {
        // an unchanged pipeline keeps its manifest untouched
        (WriteMode::MissingOnly, Some(_)) => None,
        // an update regenerates only the [images.*] tables
        (WriteMode::Update, Some(existing)) => {
            match patch_pipeline_manifest(&existing, name, image_versions) {
                Ok(patched) => Some(patched),
                Err(reason) => {
                    progress.warning(format!(
                        "Failed to update '{}': {reason}; update its [images] tables by hand",
                        manifest_path.display()
                    ));
                    None
                }
            }
        }
        // a new pipeline, or a missing manifest: generate it pinning each image to its version
        _ => Some(generate_pipeline_manifest(name, image_versions)),
    };
    if let Some(manifest) = manifest
        && resolver
            .write_toml::<build::ManifestToml>(&manifest_path, &manifest, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    // always write description.md so every pipeline carries a docs file, in the trimmed form build
    // embeds; a None/empty description is an empty file, which build treats as absent
    let description_path = pipeline_dir.join("description.md");
    let description = normalize_description(config.description.as_deref()).unwrap_or_default();
    if should_write(&description_path, mode)
        && resolver
            .write_text(&description_path, &description, progress)
            .await?
            == WriteOutcome::Quit
    {
        return Ok(WriteOutcome::Quit);
    }
    Ok(WriteOutcome::Written)
}

// ─── Planning ────────────────────────────────────────────────────────────────

/// The manifest entry a planned write comes from
#[derive(Clone, Copy)]
enum PlannedEntry<'a> {
    /// An image version entry
    Image(&'a manifest::ImageVersion),
    /// A pipeline version entry
    Pipeline(&'a manifest::PipelineVersion),
}

/// One resource the write pass will write, with its reconciled placement
struct PlannedWrite<'a> {
    /// Whether the resource is an image or a pipeline
    kind: ResourceKind,
    /// The resource's group (after any override)
    group: String,
    /// The resource's name (after any collision rename)
    name: String,
    /// The toolbox version label to record
    version: String,
    /// The tool directory (relative to the toolbox root) to write into
    target_rel: String,
    /// Whether this is a brand-new tool (manifest generated from scratch)
    full_write: bool,
    /// The group this tool is being re-grouped from, when re-grouped in place
    regrouped_from: Option<String>,
    /// Whether the resource already matches the toolbox (only missing files are written)
    unchanged: bool,
    /// Whether the target directory came from an explicit `=dest`
    explicit_dest: bool,
    /// The manifest entry to write
    entry: PlannedEntry<'a>,
}

impl PlannedWrite<'_> {
    /// How this resource's files are written
    fn mode(&self) -> WriteMode {
        if self.unchanged {
            WriteMode::MissingOnly
        } else if self.full_write {
            WriteMode::Full
        } else {
            WriteMode::Update
        }
    }
}

/// Everything the planner needs besides the manifest itself
struct PlanContext<'a> {
    /// The resolved toolbox-wide settings (layout and registry)
    settings: &'a ToolboxSettings,
    /// The existing toolbox's image lookups
    images: &'a ReconcileIndex,
    /// The existing toolbox's pipeline lookups
    pipelines: &'a ReconcileIndex,
    /// Every directory of the existing toolbox mapped to its occupant
    occupied: &'a HashMap<String, DirOwner>,
    /// Whether `--strip-registry` is set
    strip_registry: bool,
    /// Whether `--overwrite` is set
    overwrite: bool,
}

/// Turn a decided action into a planned write, emitting its warnings
///
/// Returns `None` for a skipped resource.
///
/// # Arguments
///
/// * `action` - The decided action
/// * `progress` - The progress bar, for skip and soft warnings
fn accept_action(
    action: WriteAction,
    progress: &Bar,
) -> Option<(String, bool, Option<String>, bool)> {
    match action {
        // a skip warns and drops the resource (the rest still export)
        WriteAction::Skip(msg) => {
            progress.warning(msg);
            None
        }
        WriteAction::Write {
            target_rel,
            full_write,
            regrouped_from,
            unchanged,
            soft_warn,
        } => {
            // a soft cross-group signal is informational only
            if let Some(warn) = soft_warn {
                progress.warning(warn);
            }
            Some((target_rel, full_write, regrouped_from, unchanged))
        }
    }
}

/// Plan every write of an export run against the existing toolbox, in a deterministic order
///
/// Walks the resolved manifest's images then pipelines sorted by `(group, name, version)`, decides
/// each one's placement with [`decide_write`], and emits skip and cross-group warnings. Nothing is
/// written here.
///
/// # Arguments
///
/// * `manifest` - The resolved in-memory manifest (after override, validation, and collisions)
/// * `ctx` - The reconciliation context
/// * `progress` - The progress bar, for skip and soft warnings
#[allow(clippy::too_many_lines)]
fn plan_writes<'a>(
    manifest: &'a ToolboxManifest,
    ctx: &PlanContext<'_>,
    progress: &Bar,
) -> Result<Vec<PlannedWrite<'a>>, Error> {
    let mut planned = Vec::new();
    // gather the configured image versions in a stable order so warnings and prompts are repeatable
    let mut images: Vec<(&ImageRequest, &String, &manifest::ImageVersion)> = manifest
        .images
        .values()
        .flat_map(|image| image.versions.iter())
        .filter_map(|(version, entry)| entry.config.as_ref().map(|c| (c, version, entry)))
        .collect();
    images.sort_by(|a, b| (&a.0.group, &a.0.name, a.1).cmp(&(&b.0.group, &b.0.name, b.1)));
    for (config, version, entry) in images {
        // compare against the form build will embed (a stripped url is re-derived by build)
        let strip_url = if ctx.strip_registry {
            derived_image_url(ctx.settings, &config.name, version)
        } else {
            None
        };
        let current_json =
            crate::utils::canonical_json(&toolbox_image_config(config, strip_url.as_deref()))?;
        let default_rel = normalize_rel_dir(&format!(
            "{}/{}",
            ctx.settings
                .export_image_path
                .as_deref()
                .unwrap_or("images"),
            config.name
        ));
        let explicit_dest = (!entry.dir.is_empty()).then_some(entry.dir.as_str());
        // decide where and how to write this image against the existing toolbox
        let action = decide_write(
            ResourceKind::Image,
            &config.group,
            &config.name,
            version,
            &current_json,
            explicit_dest,
            &default_rel,
            ctx.overwrite,
            ctx.images,
            ctx.occupied,
        );
        if let Some((target_rel, full_write, regrouped_from, unchanged)) =
            accept_action(action, progress)
        {
            planned.push(PlannedWrite {
                kind: ResourceKind::Image,
                group: config.group.clone(),
                name: config.name.clone(),
                version: version.clone(),
                target_rel,
                full_write,
                regrouped_from,
                unchanged,
                explicit_dest: explicit_dest.is_some(),
                entry: PlannedEntry::Image(entry),
            });
        }
    }
    // gather the configured pipeline versions in the same stable order
    let mut pipelines: Vec<(&PipelineRequest, &String, &manifest::PipelineVersion)> = manifest
        .pipelines
        .values()
        .flat_map(|pipeline| pipeline.versions.iter())
        .filter_map(|(version, entry)| entry.config.as_ref().map(|c| (c, version, entry)))
        .collect();
    pipelines.sort_by(|a, b| (&a.0.group, &a.0.name, a.1).cmp(&(&b.0.group, &b.0.name, b.1)));
    for (config, version, entry) in pipelines {
        // a pipeline keeps the version label it already carries in the toolbox
        let version = ctx
            .pipelines
            .existing_label(&config.group, &config.name)
            .unwrap_or_else(|| version.clone());
        let current_json = crate::utils::canonical_json(config)?;
        let default_rel = normalize_rel_dir(&format!(
            "{}/{}",
            ctx.settings
                .export_pipeline_path
                .as_deref()
                .unwrap_or("pipelines"),
            config.name
        ));
        let explicit_dest = (!entry.dir.is_empty()).then_some(entry.dir.as_str());
        // decide placement and reconciliation the same way images do
        let action = decide_write(
            ResourceKind::Pipeline,
            &config.group,
            &config.name,
            &version,
            &current_json,
            explicit_dest,
            &default_rel,
            ctx.overwrite,
            ctx.pipelines,
            ctx.occupied,
        );
        if let Some((target_rel, full_write, regrouped_from, unchanged)) =
            accept_action(action, progress)
        {
            planned.push(PlannedWrite {
                kind: ResourceKind::Pipeline,
                group: config.group.clone(),
                name: config.name.clone(),
                version,
                target_rel,
                full_write,
                regrouped_from,
                unchanged,
                explicit_dest: explicit_dest.is_some(),
                entry: PlannedEntry::Pipeline(entry),
            });
        }
    }
    Ok(planned)
}

// ─── Main Export ─────────────────────────────────────────────────────────────

/// Exports the selected images and pipelines into an on-disk toolbox directory
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to fetch resources
/// * `cmd` - The export args (target directory and resource selection)
/// * `args` - The top-level thorctl args (worker count)
/// * `conf` - The Thorctl config (editor resolution)
#[allow(clippy::too_many_lines)]
pub async fn export(
    thorium: Thorium,
    cmd: &ExportToolbox,
    args: &Args,
    conf: &CtlConf,
) -> Result<(), Error> {
    // --review opens an editor per config, which needs a real terminal; fail before any work
    // rather than hang (or silently skip the review the user asked for)
    exports::require_review_terminal(cmd.review)?;
    // --quiet gets an inert bar so info lines are suppressed while warnings still print
    let progress = Bar::new_or_quiet("toolbox export", "Exporting", BarKind::Timer, args.quiet);
    // when --group is combined with named --pipelines/--images it's only the default group for
    // those names, not a full-group export; say so to avoid the "why didn't it export the whole
    // group?" surprise
    if cmd.group.is_some() && (!cmd.pipelines.is_empty() || !cmd.images.is_empty()) {
        progress.info_anonymous(
            "--group is used only as the default group for the named --pipelines/--images; omit \
             them to export the whole group",
        );
    }
    // resolve the toolbox output directory once: an explicit --output, else the --config dir, else
    // ./toolbox. Announce a defaulted output so it's clear where the toolbox is written and why
    let output = resolve_output(cmd);
    if cmd.output.is_none() {
        if cmd.config.is_some() {
            progress.info_anonymous(format!(
                "No --output set; exporting into the toolbox at '{}' (from --config)",
                output.display()
            ));
        } else {
            progress.info_anonymous(format!(
                "No --output set; creating a new toolbox at '{}'",
                output.display()
            ));
        }
    }
    // detect an existing config.toml at the output root: it makes this an append into an existing
    // toolbox (its settings are the source, and it is preserved unless --overwrite-config)
    let existing_config = existing_config_path(&output);
    // resolve the toolbox-wide settings up front so an unusable combination fails before any fetch
    let settings = resolve_settings(cmd, existing_config.as_deref(), &progress)?;
    // the layout dirs come from a (possibly hand-edited) config.toml, so make sure they stay inside
    // the toolbox before any file is placed under them
    validate_layout_paths(&settings)?;
    // --strip-registry leaves each url for build to derive, which needs a registry to derive from
    if cmd.strip_registry && primary_registry(&settings).is_none() {
        return Err(Error::new(
            "--strip-registry clears each image's url so build derives it from config.toml, but \
             the toolbox config has no registry; pass --registry <REGISTRY> (with \
             --overwrite-config when the output already has a config.toml)",
        ));
    }
    // resolve the editor up front so --review uses a consistent command across all configs
    let editor = resolve_editor(None, conf);
    // load the existing toolbox once (on-disk crawl, else toolbox.json); it is reused both to
    // resolve a no-selection "refresh all" and to reconcile the writes below
    let existing_manifest = load_existing_manifest(&output, &progress).await;
    // decide what to export: an explicit selection wins; otherwise, with no selection, a refresh of
    // every tool already in the toolbox (gated by --overwrite since it rewrites them); otherwise error
    let has_selection = cmd.group.is_some() || !cmd.pipelines.is_empty() || !cmd.images.is_empty();
    let (images, pipelines, fetch_failures) = if has_selection {
        let (images, pipelines) = resolve_resources(&thorium, cmd, args.workers, &progress).await?;
        (images, pipelines, Vec::new())
    } else if let Some(existing) = &existing_manifest {
        if cmd.overwrite {
            let refreshed =
                refresh_existing_resources(&thorium, existing, args.workers, &progress).await?;
            (refreshed.images, refreshed.pipelines, refreshed.failures)
        } else {
            return Err(Error::new(
                "No resources selected. Pass --overwrite to refresh every tool already in the \
                 toolbox, or specify --group/--pipelines/--images.",
            ));
        }
    } else {
        return Err(Error::new(
            "No resources to export. Specify --group, --pipelines, or --images.",
        ));
    };
    progress.info_anonymous(format!(
        "Exporting {} and {} to '{}'{}",
        pluralize(images.len(), "image", "images"),
        pluralize(pipelines.len(), "pipeline", "pipelines"),
        output.display(),
        if cmd.with_images {
            " (bundling container images)"
        } else {
            ""
        },
    ));
    // gather the unique (group, policy name) pairs referenced across all images, keeping
    // one referencing image per pair for error context
    let mut wanted: Vec<(String, String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for image in &images {
        for policy_name in &image.network_policies {
            // first image to reference a given (group, policy) wins as the error-context name
            if seen.insert((image.group.clone(), policy_name.clone())) {
                wanted.push((image.group.clone(), policy_name.clone(), image.name.clone()));
            }
        }
    }
    // fetch existing policies scoped to just the exported images' groups so a name that
    // is ambiguous across the whole instance still resolves uniquely within its group
    // (a global get-by-name 400s on cross-group ambiguity)
    let groups: Vec<String> = images
        .iter()
        .map(|image| image.group.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let index = policies::fetch_existing_in_groups(&thorium, &groups).await?;
    // resolved policies are keyed by (group, name): same-named policies in different groups are
    // distinct definitions, and each image only picks up its own group's
    let mut policies: HashMap<(String, String), NetworkPolicyRequest> = HashMap::new();
    // collect references that resolved to no definition; an export that omits a policy it
    // references produces a structurally-incomplete toolbox, so these drive a non-zero exit
    // (and an aggregated end-of-run summary) rather than being lost as mid-stream warnings
    let mut dangling_policies: Vec<String> = Vec::new();
    for (group, policy_name, image_name) in wanted {
        // look each referenced policy up by its (group, name) identity within the fetched index
        match index.get(&(group.clone(), policy_name.clone())) {
            // found: stash its request form for build_manifest to attach
            Some(policy) => {
                policies.insert((group, policy_name), NetworkPolicyRequest::from(policy));
            }
            // a dangling reference in the source instance isn't fatal to the export, but the
            // toolbox will be missing that definition, so record it and warn
            None => {
                // qualify the policy by the group it was looked up in
                let policy = format!(
                    "{policy_name}{}",
                    utils::policy_suffix(&[group.as_str()], None)
                );
                progress.warning(format!(
                    "Network policy {policy} (referenced by image '{}') was not found; the \
                     exported toolbox won't include its definition, so an import will rely on the \
                     target instance already having it",
                    utils::resource_id(&group, &image_name)
                ));
                dangling_policies.push(policy);
            }
        }
    }
    // prompts are only possible interactively (not --skip-conflicts) AND on a real terminal; the
    // prompt library reads stdin and draws on stderr, so both must be terminals
    let can_prompt =
        !cmd.skip_conflicts && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    // map each explicitly-named resource to its optional `=destpath`, per kind and (group, name),
    // resolved to a directory relative to the toolbox root. A dest that resolves outside the
    // toolbox is a hard error here, before anything is written.
    let dest_overrides = resolve_dest_overrides(cmd, &output)?;
    // build an in-memory manifest and run it through the SAME validation and
    // collision-resolution flow as `toolbox import`, so duplicates/collisions are
    // resolved identically (de-dupe, rename + cascade, or skip) before anything
    // touches disk
    let mut manifest = build_manifest(&settings, &images, &pipelines, &policies, &dest_overrides);
    // snapshot each resource's original group BEFORE any override so collision resolution can
    // tell which members truly collided versus were collapsed into one group by --group-override
    let sources = manifest.capture_source_groups();
    if let Some(group) = &cmd.group_override {
        progress.info_anonymous(format!(
            "Overriding all image/pipeline export groups to '{}'",
            group.bright_yellow()
        ));
        // override_group rewrites every image/pipeline config group and each bundled policy's groups
        manifest = manifest.override_group(group);
    }
    // drop image versions with no config and pipelines that are structurally broken (warning each)
    shared::warn_dropped(&manifest.validate_structural(), &progress);
    // drop pipelines whose order references images not present in their group (warning each)
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // collapse byte-identical duplicates and rename/skip true (group, name) collisions
    collisions::resolve_collisions(&mut manifest, &sources, can_prompt, &progress)?;
    // re-check coherence: collision renames/repointing can re-break a pipeline's group view
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // index the existing toolbox (loaded once above) for append reconciliation: lets the planner
    // skip unchanged resources and their bundle work, update a tool where it already lives, refuse
    // a second copy at a different directory, and refuse a directory another tool owns
    let (existing_images, existing_pipelines) = index_existing(existing_manifest.as_ref());
    let occupied = dir_owners(&existing_images, &existing_pipelines);
    let image_index = ReconcileIndex::new(existing_images);
    let pipeline_index = ReconcileIndex::new(existing_pipelines);
    // plan every write (placement, reconciliation, and warnings) before touching disk
    let planned = plan_writes(
        &manifest,
        &PlanContext {
            settings: &settings,
            images: &image_index,
            pipelines: &pipeline_index,
            occupied: &occupied,
            strip_registry: cmd.strip_registry,
            overwrite: cmd.overwrite,
        },
        &progress,
    )?;
    // refuse a run whose writes would clobber each other or break build, before any file is written
    let claims: Vec<RunClaim<'_>> = planned
        .iter()
        .map(|plan| RunClaim {
            kind: plan.kind,
            group: &plan.group,
            name: &plan.name,
            version: &plan.version,
            target_rel: &plan.target_rel,
        })
        .collect();
    let conflicts = find_run_conflicts(&claims);
    if !conflicts.is_empty() {
        return Err(Error::new(format!(
            "Failed to export: {}. Pass --group-override <group> to merge them into one group \
             (colliding tools are renamed), give each a distinct =dir, or export them in separate \
             runs",
            conflicts.join("; ")
        )));
    }
    // write the planned resources to disk, resolving on-disk conflicts
    let mut resolver = DiskConflictResolver::new(cmd.overwrite, can_prompt, editor.to_string())
        .explicit_skip(cmd.skip_conflicts);
    let mut stopped = false;
    // (image name, container url, tool dir) tarballs to bundle after the config pass; the
    // config writes stay sequential (the resolver prompts), but the heavy container
    // pull/save is run bounded-parallel below. The tool dir is carried so the tarball lands
    // beside the image's manifest, matching where import looks for it.
    let mut bundle_jobs: Vec<(String, Option<String>, PathBuf)> = Vec::new();
    for plan in &planned {
        let kind = plan.kind.as_str();
        let tool_dir = output.join(&plan.target_rel);
        // announce anything other than a plain new write
        if plan.unchanged {
            progress.info_anonymous(format!(
                "Unchanged: {kind} '{}' already current in the toolbox",
                utils::entry_id(Some(&plan.group), &plan.name, &plan.version)
            ));
        } else if let Some(from) = &plan.regrouped_from {
            progress.info_anonymous(format!(
                "Re-grouping {kind} '{}' from '{from}' to '{}' in place at '{}'",
                utils::entry_id(None, &plan.name, &plan.version),
                plan.group,
                plan.target_rel
            ));
        } else if !plan.full_write {
            progress.info_anonymous(format!(
                "Updating {kind} '{}' in place at '{}' (manifest.toml keeps its \
                 toolbox-authored settings)",
                utils::entry_id(Some(&plan.group), &plan.name, &plan.version),
                plan.target_rel
            ));
        }
        let outcome = match plan.entry {
            PlannedEntry::Image(entry) => {
                // the planner only plans configured entries
                let Some(config) = &entry.config else {
                    continue;
                };
                // when a NEW image is pointed at an explicit `=dir` that already holds a Dockerfile,
                // the user is folding this config into an existing build context, so mark its
                // manifest build = true. Only the explicit-dest fresh-write case is auto-detected.
                let build =
                    plan.full_write && plan.explicit_dest && tool_dir.join("Dockerfile").exists();
                if build {
                    progress.info_anonymous(format!(
                        "Found a Dockerfile in '{}'; marking image '{}' build = true",
                        plan.target_rel, plan.name
                    ));
                }
                let (outcome, written) = write_image_entry(
                    &tool_dir,
                    config,
                    &plan.version,
                    ImageWriteOptions {
                        build,
                        mode: plan.mode(),
                        strip_registry: cmd.strip_registry,
                        review: cmd.review,
                    },
                    &entry.network_policies,
                    editor,
                    &mut resolver,
                    &progress,
                )
                .await?;
                // queue the heavy container pull/save for after the sequential config writes; an
                // unchanged image whose tarball is already saved skips the redundant re-bundle
                let tarball_saved = tool_dir.join(format!("{}.tar.gz", written.name)).exists();
                if outcome != WriteOutcome::Quit
                    && cmd.with_images
                    && !(plan.unchanged && tarball_saved)
                {
                    // only K8s images run from a container, so other scalers have nothing to bundle
                    if build::scaler_requires_container_image(written.scaler) {
                        bundle_jobs.push((written.name, written.image, tool_dir));
                    } else {
                        progress.info_anonymous(format!(
                            "Image '{}' uses the {} scaler, which needs no container image; not \
                             bundling a tarball for it",
                            written.name, written.scaler
                        ));
                    }
                }
                outcome
            }
            PlannedEntry::Pipeline(entry) => {
                // the planner only plans configured entries
                let Some(config) = &entry.config else {
                    continue;
                };
                // the resolved image map carries the (possibly renamed) names paired with the
                // versions we exported them under
                let mut image_versions: Vec<(String, String)> = entry
                    .images
                    .iter()
                    .map(|(name, image)| (name.clone(), image.version.clone()))
                    .collect();
                image_versions.sort();
                write_pipeline_entry(
                    &tool_dir,
                    config,
                    &image_versions,
                    plan.mode(),
                    cmd.review,
                    editor,
                    &mut resolver,
                    &progress,
                )
                .await?
            }
        };
        // a Quit at any prompt stops the whole export
        if outcome == WriteOutcome::Quit {
            stopped = true;
            break;
        }
    }
    // a user quit leaves a partially written tree with a stale (or missing) config.toml/toolbox.json
    if stopped {
        progress.refresh("Export stopped early", BarKind::Timer);
        progress.finish();
        return Err(Error::new(format!(
            "Export stopped early: some tool files under '{}' may already be written, but \
             config.toml and toolbox.json were not updated. Re-run the export, or run `thorctl \
             toolbox build -c {}` once config.toml exists",
            output.display(),
            output.join("config.toml").display()
        )));
    }
    // bundle the queued container images in parallel (container pull/save here capture
    // their output rather than streaming it, so concurrency is safe). Bounded by --workers.
    // Bundling is best-effort: a failed pull/save warns and is collected so one bad image
    // doesn't abort the export (save cleans up its own partial archive on failure), but the
    // collected failures drive a non-zero exit at the end so a scripted export can't mistake
    // an incomplete bundle for a complete one.
    let mut bundle_failures: Vec<String> = Vec::new();
    if !bundle_jobs.is_empty() {
        // never spawn more workers than jobs, and never zero (buffer_unordered(0) stalls)
        let workers = std::cmp::min(args.workers, bundle_jobs.len()).max(1);
        progress.refresh("Bundling images", BarKind::Bound(bundle_jobs.len() as u64));
        // pull+save each container concurrently, pairing every result with its image name so a
        // failure can be reported even though the stream completes out of order
        let results: Vec<(String, Result<(), Error>)> = stream::iter(bundle_jobs)
            .map(|(name, url, dir)| {
                let progress = &progress;
                async move {
                    // save the tarball into the same tool directory the manifest was written to,
                    // so import (which reads the recorded per-image dir) finds it
                    let outcome = bundle_image(&dir, &name, url.as_deref(), args.quiet).await;
                    progress.inc(1);
                    (name, outcome)
                }
            })
            .buffer_unordered(workers)
            .collect()
            .await;
        // warn (don't abort) on each failed bundle so the rest of the export still completes
        for (name, outcome) in results {
            if let Err(err) = outcome {
                progress.warning(format!(
                    "Failed to bundle image '{name}': {err}; the toolbox will not include its \
                     tarball"
                ));
                bundle_failures.push(name);
            }
        }
        bundle_failures.sort();
    }
    // config.toml is the toolbox's sticky identity: an existing one is preserved (its settings were
    // the source above) unless --overwrite-config, so an append never clobbers the toolbox's
    // settings. A fresh export creates it (announced). Pinned image urls live in each image's
    // manifest.toml (exported_image_path), so the registry here matters for tools marked buildable
    // and for every image exported with --strip-registry.
    let config_path = output.join("config.toml");
    if existing_config.is_none() || cmd.overwrite_config {
        if existing_config.is_none() {
            progress.info_anonymous(format!(
                "No config.toml at '{}'; creating one",
                config_path.display()
            ));
        }
        let config_toml = render_config_toml(
            &settings.name,
            settings.registry.as_deref(),
            &settings.registries,
            settings.image_path_prefix.as_deref(),
            settings.export_image_path.as_deref(),
            settings.export_pipeline_path.as_deref(),
            settings.bundled_images,
            settings.base_image.as_ref(),
        );
        // create the output root first: a run that wrote no tool files hasn't created it yet
        tokio::fs::create_dir_all(&output).await.map_err(|e| {
            Error::new(format!(
                "Failed to create directory '{}': {e}",
                output.display()
            ))
        })?;
        // written directly (not via the per-file resolver) because the sticky-config rule, not the
        // resolver's overwrite/skip behavior, governs config.toml
        tokio::fs::write(&config_path, config_toml)
            .await
            .map_err(|e| Error::new(format!("Failed to write '{}': {e}", config_path.display())))?;
    }
    // Auto-build toolbox.json, preserving the real image urls captured from Thorium.
    // build walks the tree with synchronous std::fs, so run it off the async runtime.
    let build_cmd = BuildToolbox {
        config: config_path.clone(),
        // leaf comes from the tool name, not image_name: an export pins urls via
        // exported_image_path, so the repo-path leaf is irrelevant here
        use_image_path: false,
        output: Some(output.join("toolbox.json")),
        path: Some(output.clone()),
        // an export records each image's real published url, so no tag suffix is applied
        tag_suffix: None,
    };
    // run the synchronous filesystem walk on a blocking thread; a failure here comes after the tool
    // files and config.toml were written, so say so and point at the rebuild command
    let built = tokio::task::spawn_blocking(move || build::build(&build_cmd))
        .await
        .map_err(|err| Error::new(format!("the build task failed: {err}")))
        .and_then(|result| result);
    if let Err(err) = built {
        progress.refresh("Export finished with errors", BarKind::Timer);
        progress.finish();
        return Err(Error::new(format!(
            "Failed to build toolbox.json: {err}. The tool files and config.toml were already \
             written to '{}'; fix the issue and run `thorctl toolbox build -c {}` to regenerate \
             toolbox.json",
            output.display(),
            config_path.display()
        )));
    }
    // a failed tool fetch, a missing bundled tarball, or an omitted (dangling) policy means the
    // written toolbox is not fully current or self-contained; report that once and exit non-zero so
    // a scripted export -> import handoff doesn't treat it as a success
    let mut problems: Vec<String> = Vec::new();
    if !fetch_failures.is_empty() {
        problems.push(format!(
            "{} could not be refreshed ({})",
            pluralize(fetch_failures.len(), "tool", "tools"),
            fetch_failures.join(", ")
        ));
    }
    if !bundle_failures.is_empty() {
        problems.push(format!(
            "{} missing ({})",
            pluralize(bundle_failures.len(), "image tarball", "image tarballs"),
            bundle_failures.join(", ")
        ));
    }
    if !dangling_policies.is_empty() {
        problems.push(format!(
            "{} not found and omitted ({})",
            pluralize(
                dangling_policies.len(),
                "referenced network policy",
                "referenced network policies"
            ),
            dangling_policies.join(", ")
        ));
    }
    if problems.is_empty() {
        progress.finish();
        println!(
            "\n{} Toolbox exported to '{}'. Import it with: thorctl toolbox import {}",
            "Export complete!".bright_green(),
            output.display(),
            output.join("toolbox.json").display()
        );
        return Ok(());
    }
    progress.refresh("Export finished with errors", BarKind::Timer);
    progress.finish();
    Err(Error::new(format!(
        "Export finished with errors: the toolbox was written to '{}' but is incomplete — {}. \
         Resolve these and re-export before importing",
        output.display(),
        problems.join("; ")
    )))
}

/// Download and save an image's container image file into the toolbox bundle
///
/// Writes `<dir>/<name>.tar.gz` (the image's tool directory, beside its manifest). Only called for
/// images whose scaler needs a container image, so an image without a container url is an error.
///
/// # Arguments
///
/// * `dir` - The image's tool directory (where its manifest was written)
/// * `name` - The exported image name (the tarball's file stem)
/// * `url` - The image's container url, if any
/// * `quiet` - Whether `--quiet` is set, which hides the bundling sub-bar
async fn bundle_image(dir: &Path, name: &str, url: Option<&str>, quiet: bool) -> Result<(), Error> {
    // a K8s image with no url has no container to pull, so its tarball can't be bundled
    let Some(url) = url.filter(|url| !url.is_empty()) else {
        return Err(Error::new(
            "it has no container image url, but its K8s scaler needs a container image",
        ));
    };
    // dedicated sub-bar so concurrent bundles each show their own pull/save progress
    let bar = Bar::new_or_quiet(name, "Bundling image", BarKind::Timer, quiet);
    // pull the container locally first so save has a local image to export, then save the
    // tarball into the image's tool directory (where its manifest was written); import
    // resolves this exact location from the per-image `dir` recorded in toolbox.json
    let tar = dir.join(format!("{name}.tar.gz"));
    let result = match container::pull(url, &bar).await {
        Ok(()) => container::save(url, &tar, &bar).await,
        Err(err) => Err(err),
    };
    // clear the sub-bar on success and failure alike; the caller reports the failure, so a
    // leftover "Bundling image" line would only be noise
    bar.finish_and_clear();
    result
}

#[cfg(test)]
mod tests {
    use super::{
        DirOwner, ExportToolbox, Placement, ReconcileIndex, ResourceKind, RunClaim,
        ToolboxSettings, WriteAction, decide_write, derived_image_url, dir_owners, dirs_by_name,
        disk_image_config, existing_resource_ids, find_run_conflicts, groups_by_name,
        locs_by_name_version, manifest, normalize_description, normalize_rel_dir,
        patch_image_manifest, patch_pipeline_manifest, plan_placement, pluralize,
        resolve_dest_within, resolve_output, toolbox_image_config,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::path::{Path, PathBuf};
    use thorium::models::{ImageRequest, PipelineRequest};

    /// Build an `ExportToolbox` with only the path-relevant fields set; the rest default to a
    /// no-op export so `resolve_output` can be exercised in isolation
    fn export_cmd(output: Option<&str>, config: Option<&str>) -> ExportToolbox {
        ExportToolbox {
            group: None,
            pipelines: Vec::new(),
            images: Vec::new(),
            group_override: None,
            output: output.map(PathBuf::from),
            config: config.map(PathBuf::from),
            name: None,
            registry: None,
            skip_conflicts: false,
            review: false,
            overwrite: false,
            overwrite_config: false,
            with_images: false,
            strip_registry: false,
        }
    }

    /// Build a reconciliation index from `(group, name, version, json, dir)` rows
    fn index(rows: &[(&str, &str, &str, &str, &str)]) -> ReconcileIndex {
        ReconcileIndex::new(
            rows.iter()
                .map(|(group, name, version, json, dir)| {
                    (
                        (
                            (*group).to_string(),
                            (*name).to_string(),
                            (*version).to_string(),
                        ),
                        ((*json).to_string(), (*dir).to_string()),
                    )
                })
                .collect(),
        )
    }

    /// An explicit `--output` always wins, regardless of `--config`
    #[test]
    fn resolve_output_prefers_explicit() {
        let cmd = export_cmd(Some("./dist"), Some("mytb/config.toml"));
        assert_eq!(resolve_output(&cmd), PathBuf::from("./dist"));
    }

    /// With no `--output`, the output anchors on the `--config` directory
    #[test]
    fn resolve_output_anchors_on_config() {
        // a config in a subdir → that subdir
        let cmd = export_cmd(None, Some("mytb/config.toml"));
        assert_eq!(resolve_output(&cmd), PathBuf::from("mytb"));
        // a bare config.toml has an empty parent → the current directory
        let bare = export_cmd(None, Some("config.toml"));
        assert_eq!(resolve_output(&bare), PathBuf::from("."));
    }

    /// With neither `--output` nor `--config`, the default is the create-new `./toolbox`
    #[test]
    fn resolve_output_defaults_to_toolbox() {
        let cmd = export_cmd(None, None);
        assert_eq!(resolve_output(&cmd), PathBuf::from("./toolbox"));
    }

    /// A relative `=dest` is interpreted relative to the toolbox root
    #[test]
    fn resolve_dest_within_relative_is_toolbox_rooted() {
        assert_eq!(
            resolve_dest_within(Path::new("/tb"), "tools/clamav").unwrap(),
            "tools/clamav"
        );
    }

    /// An absolute `=dest` inside the toolbox is re-expressed relative to the root
    #[test]
    fn resolve_dest_within_absolute_inside_is_relativized() {
        assert_eq!(
            resolve_dest_within(Path::new("/tb"), "/tb/pipelines/static/identify-files").unwrap(),
            "pipelines/static/identify-files"
        );
    }

    /// A `..`-bearing dest that normalizes back inside the toolbox is accepted (the explicit-path
    /// case: a path that re-descends into the toolbox)
    #[test]
    fn resolve_dest_within_dotdot_reentering_is_accepted() {
        assert_eq!(
            resolve_dest_within(Path::new("/a/b/tb"), "../tb/pipelines/x").unwrap(),
            "pipelines/x"
        );
    }

    /// A dest that lands outside the toolbox (absolute elsewhere, an escaping `..`, or the root
    /// itself) is rejected
    #[test]
    fn resolve_dest_within_outside_is_rejected() {
        assert!(resolve_dest_within(Path::new("/tb"), "/other/x").is_err());
        assert!(resolve_dest_within(Path::new("/tb"), "../escape").is_err());
        assert!(resolve_dest_within(Path::new("/tb"), "/tb").is_err());
    }

    /// Relative dirs normalize to build's forward-slash form whatever their spelling
    #[test]
    fn normalize_rel_dir_uses_forward_slashes() {
        assert_eq!(normalize_rel_dir("tools\\clamav"), "tools/clamav");
        assert_eq!(normalize_rel_dir("./images/clamav/"), "images/clamav");
        assert_eq!(normalize_rel_dir("images//clamav"), "images/clamav");
    }

    /// Descriptions are trimmed of trailing whitespace like build trims description.md
    #[test]
    fn normalize_description_trims_trailing_whitespace() {
        assert_eq!(
            normalize_description(Some("Scans files\n")),
            Some("Scans files".to_string())
        );
        assert_eq!(
            normalize_description(Some("  keep\tlead")),
            Some("  keep\tlead".to_string())
        );
        assert_eq!(normalize_description(None), None);
    }

    /// Counts pick the singular form only for exactly one
    #[test]
    fn pluralize_agrees_with_count() {
        assert_eq!(pluralize(1, "image", "images"), "1 image");
        assert_eq!(pluralize(0, "image", "images"), "0 images");
        assert_eq!(pluralize(2, "policy", "policies"), "2 policies");
    }

    /// Build settings with an optional registry and prefix for the strip-registry tests
    fn settings(registry: Option<&str>, prefix: Option<&str>) -> ToolboxSettings {
        ToolboxSettings {
            name: "tb".to_string(),
            registry: registry.map(str::to_string),
            registries: Vec::new(),
            image_path_prefix: prefix.map(str::to_string),
            export_image_path: None,
            export_pipeline_path: None,
            bundled_images: false,
            base_image: None,
        }
    }

    /// Stripping clears only a set url, and the comparison form is the url build derives
    #[test]
    fn strip_registry_forms() {
        let mut config = ImageRequest::new("g", "clamav");
        config.image = Some("reg/clamav:1".to_string());
        // a set url is written empty and compared as the derived url
        assert_eq!(disk_image_config(&config, true).image, Some(String::new()));
        let derived =
            derived_image_url(&settings(Some("ghcr.io/org"), Some("tb")), "clamav", "1.0");
        assert_eq!(derived.as_deref(), Some("ghcr.io/org/tb/clamav:1.0"));
        assert_eq!(
            toolbox_image_config(&config, derived.as_deref()).image,
            derived
        );
        // without stripping, both forms keep the url
        assert_eq!(disk_image_config(&config, false).image, config.image);
        assert_eq!(toolbox_image_config(&config, None).image, config.image);
        // an unset url stays unset in both forms
        let bare = ImageRequest::new("g", "ext");
        assert_eq!(disk_image_config(&bare, true).image, None);
        assert_eq!(toolbox_image_config(&bare, Some("r/ext:1")).image, None);
        // no registry → nothing to derive
        assert_eq!(
            derived_image_url(&settings(None, None), "clamav", "1.0"),
            None
        );
    }

    /// `dirs_by_name` collapses the `(group, name, version)` index to `(group, name) → dir`, skipping
    /// empty (legacy) dirs and keeping the first real one
    #[test]
    fn dirs_by_name_collapses_versions_and_skips_empty() {
        let mut index: HashMap<(String, String, String), (String, String)> = HashMap::new();
        index.insert(
            ("g".into(), "a".into(), "1".into()),
            ("{}".into(), "images/a".into()),
        );
        // a legacy entry with no recorded dir is ignored
        index.insert(
            ("g".into(), "b".into(), "1".into()),
            ("{}".into(), String::new()),
        );
        let dirs = dirs_by_name(&index);
        assert_eq!(
            dirs.get(&("g".into(), "a".into())).map(String::as_str),
            Some("images/a")
        );
        assert!(!dirs.contains_key(&("g".into(), "b".into())));
    }

    /// `groups_by_name` maps each tool name to the set of groups it appears under, so a same-named
    /// tool spread across groups (a rename tell-tale) is detectable
    #[test]
    fn groups_by_name_collects_groups_per_name() {
        let mut index: HashMap<(String, String, String), (String, String)> = HashMap::new();
        index.insert(
            ("static1".into(), "clamav".into(), "latest".into()),
            ("{}".into(), "images/clamav".into()),
        );
        index.insert(
            ("static1".into(), "clamav".into(), "1.0".into()),
            ("{}".into(), "images/clamav".into()),
        );
        index.insert(
            ("other".into(), "exiftool".into(), "latest".into()),
            ("{}".into(), "images/exiftool".into()),
        );
        let by_name = groups_by_name(&index);
        // clamav appears under one group (collapsed across its two versions)
        assert_eq!(
            by_name
                .get("clamav")
                .map(|g| g.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["static1".to_string()])
        );
        assert_eq!(
            by_name
                .get("exiftool")
                .map(|g| g.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["other".to_string()])
        );
    }

    /// `locs_by_name_version` maps build's identity `(name, version)` → `(group, dir)`, so a
    /// group-mismatched write can find where the tool already lives regardless of group
    #[test]
    fn locs_by_name_version_maps_build_identity() {
        let mut index: HashMap<(String, String, String), (String, String)> = HashMap::new();
        index.insert(
            ("toolbox-grp".into(), "clamav".into(), "1.0".into()),
            ("{}".into(), "tools/clamav".into()),
        );
        let locs = locs_by_name_version(&index);
        // looked up by (name, version) with no group, it returns the group + dir it lives at
        assert_eq!(
            locs.get(&("clamav".to_string(), "1.0".to_string())),
            Some(&("toolbox-grp".to_string(), "tools/clamav".to_string()))
        );
        // a different version isn't a match (build identity is name+version)
        assert!(!locs.contains_key(&("clamav".to_string(), "2.0".to_string())));
    }

    /// A pipeline's existing label is found by (group, name), preferring latest, then by an
    /// unambiguous name
    #[test]
    fn existing_label_prefers_exact_then_unique_name() {
        let idx = index(&[
            ("g", "triage", "1.0", "{}", "pipelines/triage"),
            ("g", "multi", "2.0", "{}", "pipelines/multi"),
            ("g", "multi", "latest", "{}", "pipelines/multi"),
            ("other", "solo", "3.0", "{}", "pipelines/solo"),
        ]);
        assert_eq!(idx.existing_label("g", "triage").as_deref(), Some("1.0"));
        assert_eq!(idx.existing_label("g", "multi").as_deref(), Some("latest"));
        // a different group falls back to the name's single label
        assert_eq!(idx.existing_label("static", "solo").as_deref(), Some("3.0"));
        assert_eq!(idx.existing_label("g", "missing"), None);
    }

    /// A tool not already in the toolbox is a fresh write at the resolved target (an explicit `=dest`
    /// wins, else the default layout)
    #[test]
    fn plan_placement_new_resource() {
        assert_eq!(
            plan_placement(None, None, None, "{}", "images/a", false),
            Placement::New("images/a".into())
        );
        assert_eq!(
            plan_placement(Some("tools/a"), None, None, "{}", "images/a", false),
            Placement::New("tools/a".into())
        );
    }

    /// An existing tool with a matching config is Unchanged at its own directory; without a
    /// `=dest` the existing dir is reused regardless of the default layout
    #[test]
    fn plan_placement_unchanged_reuses_existing_dir() {
        assert_eq!(
            plan_placement(None, Some("custom/a"), Some("{}"), "{}", "images/a", false),
            Placement::Unchanged("custom/a".into())
        );
    }

    /// An existing tool whose config differs is an Update with `--overwrite`, else SkipDiffers — both
    /// targeting the tool's existing directory
    #[test]
    fn plan_placement_differs_overwrite_vs_skip() {
        assert_eq!(
            plan_placement(
                None,
                Some("custom/a"),
                Some("{\"old\":1}"),
                "{\"new\":1}",
                "images/a",
                true
            ),
            Placement::Update("custom/a".into())
        );
        assert_eq!(
            plan_placement(
                None,
                Some("custom/a"),
                Some("{\"old\":1}"),
                "{\"new\":1}",
                "images/a",
                false
            ),
            Placement::SkipDiffers
        );
        // a different version of an existing tool (no exact match) is a difference too
        assert_eq!(
            plan_placement(None, Some("custom/a"), None, "{}", "images/a", false),
            Placement::SkipDiffers
        );
    }

    /// An explicit `=dest` pointing somewhere other than where the tool already lives is a SkipMove
    /// (relocating would leave a duplicate); the same dir falls through to the normal update path
    #[test]
    fn plan_placement_explicit_move_is_rejected() {
        assert_eq!(
            plan_placement(
                Some("other/a"),
                Some("custom/a"),
                Some("{}"),
                "{}",
                "images/a",
                true
            ),
            Placement::SkipMove("custom/a".into())
        );
        // =dest equal to the existing dir is fine — identical config → Unchanged
        assert_eq!(
            plan_placement(
                Some("custom/a"),
                Some("custom/a"),
                Some("{}"),
                "{}",
                "images/a",
                false
            ),
            Placement::Unchanged("custom/a".into())
        );
        // a backslash spelling of the same dir (a Windows =dest) is the same dir
        assert_eq!(
            plan_placement(
                Some("custom\\a"),
                Some("custom/a"),
                Some("{}"),
                "{}",
                "images/a",
                false
            ),
            Placement::Unchanged("custom/a".into())
        );
    }

    /// A genuinely new tool is a full write at the resolved dir; a same-name/different-group tool at a
    /// different version and directory is still a full write but carries the soft cross-group warning
    #[test]
    fn decide_write_new_and_soft_warn() {
        let empty = ReconcileIndex::default();
        assert_eq!(
            decide_write(
                ResourceKind::Image,
                "g",
                "a",
                "1.0",
                "{}",
                None,
                "images/a",
                false,
                &empty,
                &HashMap::new(),
            ),
            WriteAction::Write {
                target_rel: "images/a".into(),
                full_write: true,
                regrouped_from: None,
                unchanged: false,
                soft_warn: None,
            }
        );
        // exists under another group, at a different version and dir → allowed, with a soft warning
        let idx = index(&[("toolbox-grp", "a", "1.0", "{}", "tools/a")]);
        let WriteAction::Write {
            full_write,
            soft_warn,
            ..
        } = decide_write(
            ResourceKind::Image,
            "static",
            "a",
            "2.0",
            "{}",
            None,
            "images/a",
            false,
            &idx,
            &HashMap::new(),
        )
        else {
            panic!("expected a Write");
        };
        assert!(full_write);
        assert!(soft_warn.is_some());
    }

    /// A new placement into a directory another tool already owns is skipped, not overwritten
    #[test]
    fn decide_write_skips_occupied_dir() {
        let rows = [("toolbox-grp", "clamav", "1.0", "{}", "images/clamav")];
        let idx = index(&rows);
        let occupied = dir_owners(&idx.exact, &HashMap::new());
        // a different group at a different version would land on images/clamav
        assert!(matches!(
            decide_write(
                ResourceKind::Image,
                "static",
                "clamav",
                "2.0",
                "{}",
                None,
                "images/clamav",
                true,
                &idx,
                &occupied,
            ),
            WriteAction::Skip(_)
        ));
        // a pipeline whose dir is owned by an image is skipped too
        let owner = DirOwner {
            kind: ResourceKind::Image,
            group: "g".into(),
            name: "x".into(),
        };
        let occupied = HashMap::from([("shared/x".to_string(), owner)]);
        assert!(matches!(
            decide_write(
                ResourceKind::Pipeline,
                "g",
                "x",
                "latest",
                "{}",
                Some("shared/x"),
                "pipelines/x",
                false,
                &ReconcileIndex::default(),
                &occupied,
            ),
            WriteAction::Skip(_)
        ));
    }

    /// A group-mismatched `(name, version)` already in the toolbox: re-grouped in place with
    /// `--overwrite`, skipped with a warning without it
    #[test]
    fn decide_write_regroups_or_skips_on_collision() {
        let idx = index(&[("toolbox-grp", "a", "1.0", "{}", "tools/a")]);
        let occupied = dir_owners(&idx.exact, &HashMap::new());
        // --overwrite → re-group in place at the existing dir (not a full write; manifest preserved)
        assert_eq!(
            decide_write(
                ResourceKind::Image,
                "static",
                "a",
                "1.0",
                "{}",
                None,
                "images/a",
                true,
                &idx,
                &occupied,
            ),
            WriteAction::Write {
                target_rel: "tools/a".into(),
                full_write: false,
                regrouped_from: Some("toolbox-grp".into()),
                unchanged: false,
                soft_warn: None,
            }
        );
        // no --overwrite → skip (would be a build-breaking duplicate)
        assert!(matches!(
            decide_write(
                ResourceKind::Image,
                "static",
                "a",
                "1.0",
                "{}",
                None,
                "images/a",
                false,
                &idx,
                &occupied,
            ),
            WriteAction::Skip(_)
        ));
    }

    /// An exact match drives Unchanged (identical) or Update (differs, with --overwrite), each at the
    /// tool's existing directory and never a full manifest rewrite
    #[test]
    fn decide_write_unchanged_and_update() {
        let idx = index(&[("g", "a", "1.0", "{}", "images/a")]);
        let occupied = dir_owners(&idx.exact, &HashMap::new());
        // matching at the same (group,name) dir → Unchanged
        assert_eq!(
            decide_write(
                ResourceKind::Image,
                "g",
                "a",
                "1.0",
                "{}",
                None,
                "images/a",
                false,
                &idx,
                &occupied,
            ),
            WriteAction::Write {
                target_rel: "images/a".into(),
                full_write: false,
                regrouped_from: None,
                unchanged: true,
                soft_warn: None,
            }
        );
        // differs + --overwrite → Update in place
        assert_eq!(
            decide_write(
                ResourceKind::Image,
                "g",
                "a",
                "1.0",
                "{\"new\":1}",
                None,
                "images/a",
                true,
                &idx,
                &occupied,
            ),
            WriteAction::Write {
                target_rel: "images/a".into(),
                full_write: false,
                regrouped_from: None,
                unchanged: false,
                soft_warn: None,
            }
        );
    }

    /// Same-named tools from different groups in one run conflict on identity or directory, while an
    /// image and pipeline in their own default dirs do not
    #[test]
    fn find_run_conflicts_detects_identity_and_dir_clashes() {
        let claim = |kind, group, name, version, target_rel| RunClaim {
            kind,
            group,
            name,
            version,
            target_rel,
        };
        // same name and version from two groups → identity clash (reported once)
        let conflicts = find_run_conflicts(&[
            claim(ResourceKind::Image, "a", "yara", "latest", "images/yara"),
            claim(ResourceKind::Image, "b", "yara", "latest", "images/yara"),
        ]);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("share the toolbox identity"));
        // same name at different versions → still one directory
        let conflicts = find_run_conflicts(&[
            claim(ResourceKind::Image, "a", "yara", "1.0", "images/yara"),
            claim(ResourceKind::Image, "b", "yara", "2.0", "images/yara/"),
        ]);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("would both be written"));
        // a same-named image and pipeline in their own default dirs are fine
        assert_eq!(
            find_run_conflicts(&[
                claim(
                    ResourceKind::Image,
                    "g",
                    "clamav",
                    "latest",
                    "images/clamav"
                ),
                claim(
                    ResourceKind::Pipeline,
                    "g",
                    "clamav",
                    "latest",
                    "pipelines/clamav"
                ),
            ]),
            Vec::<String>::new()
        );
    }

    /// Patching an image manifest refreshes the export-owned keys and keeps everything else
    #[test]
    fn patch_image_manifest_updates_owned_keys_only() {
        let existing = "name = \"clamav\"\n\
                        type = \"image\"\n\
                        config_from = \"clamav.json\"\n\
                        # a hand-written note\n\
                        version = \"1.0\"\n\
                        exported_image_path = \"reg/clamav:1\"\n\
                        network_policies_from = [\n  \"old.policy.json\",\n  \"https://example.com/p.json\",\n]\n\
                        build = true\n\
                        \n\
                        [base_image]\n\
                        image = \"debian\"\n";
        let patched = patch_image_manifest(
            existing,
            "2.0",
            Some("reg/clamav:2"),
            &["egress.policy.json".to_string()],
        )
        .unwrap();
        let table: toml::Table = patched.parse().unwrap();
        assert_eq!(table["version"].as_str(), Some("2.0"));
        assert_eq!(table["exported_image_path"].as_str(), Some("reg/clamav:2"));
        // local policy files are replaced; URL references are kept
        assert_eq!(
            table["network_policies_from"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["egress.policy.json", "https://example.com/p.json"]
        );
        // toolbox-authored settings and comments survive
        assert_eq!(table["build"].as_bool(), Some(true));
        assert_eq!(table["base_image"]["image"].as_str(), Some("debian"));
        assert!(patched.contains("# a hand-written note"));
        // removing the pin and adding a missing key both work
        let unpinned = patch_image_manifest(
            "name = \"x\"\ntype = \"image\"\nexported_image_path = \"a\"\n",
            "latest",
            None,
            &[],
        )
        .unwrap();
        let table: toml::Table = unpinned.parse().unwrap();
        assert!(!table.contains_key("exported_image_path"));
        assert_eq!(table["version"].as_str(), Some("latest"));
    }

    /// Patching a pipeline manifest swaps its image tables and keeps its label and description
    #[test]
    fn patch_pipeline_manifest_keeps_label_and_description() {
        let existing = "name = \"triage\"\n\
                        type = \"pipeline\"\n\
                        version = \"1.0\"\n\
                        description = \"\"\"\nTriage flow\n[not a table]\n\"\"\"\n\
                        config_from = \"triage.json\"\n\
                        \n\
                        [images.old]\n\
                        version = \"latest\"\n";
        let patched = patch_pipeline_manifest(
            existing,
            "triage",
            &[("clamav".to_string(), "2.0".to_string())],
        )
        .unwrap();
        let table: toml::Table = patched.parse().unwrap();
        assert_eq!(table["version"].as_str(), Some("1.0"));
        assert!(
            table["description"]
                .as_str()
                .unwrap()
                .contains("Triage flow")
        );
        let images = table["images"].as_table().unwrap();
        assert!(!images.contains_key("old"));
        assert_eq!(images["clamav"]["version"].as_str(), Some("2.0"));
    }

    /// Build a minimal toolbox manifest from `(group, name, versions)` image specs and
    /// `(group, name)` pipeline specs, for the enumeration test
    fn manifest_with(
        images: &[(&str, &str, &[&str])],
        pipelines: &[(&str, &str)],
    ) -> manifest::ToolboxManifest {
        let mut image_map = HashMap::new();
        for (group, name, versions) in images {
            let versions = versions
                .iter()
                .map(|v| {
                    (
                        (*v).to_string(),
                        manifest::ImageVersion {
                            dir: String::new(),
                            build_path: "./".to_string(),
                            config_from: None,
                            config: Some(ImageRequest::new(*group, *name)),
                            network_policies_from: Vec::new(),
                            network_policies: Vec::new(),
                        },
                    )
                })
                .collect();
            image_map.insert((*name).to_string(), manifest::ImageManifest { versions });
        }
        let mut pipeline_map = HashMap::new();
        for (group, name) in pipelines {
            let versions = HashMap::from([(
                "latest".to_string(),
                manifest::PipelineVersion {
                    dir: String::new(),
                    description: String::new(),
                    images: HashMap::new(),
                    config_from: None,
                    config: Some(PipelineRequest::new(*group, *name, serde_json::json!([]))),
                },
            )]);
            pipeline_map.insert((*name).to_string(), manifest::PipelineManifest { versions });
        }
        manifest::ToolboxManifest {
            name: "tb".to_string(),
            registry: None,
            pipelines: pipeline_map,
            images: image_map,
            bundled_images: false,
            image_path_prefix: None,
        }
    }

    /// `existing_resource_ids` lists every tool's `(group, name)` once, sorted, deduping across
    /// versions and covering both images and pipelines (the refresh-all enumeration)
    #[test]
    fn existing_resource_ids_dedups_and_covers_both() {
        let m = manifest_with(
            &[
                ("static", "exiftool", &["latest"]),
                ("static", "clamav", &["1.0", "latest"]),
            ],
            &[("static", "triage")],
        );
        let (images, pipelines) = existing_resource_ids(&m);
        // clamav has two versions but is enumerated once; exiftool once; the list is sorted
        assert_eq!(
            images,
            vec![
                ("static".to_string(), "clamav".to_string()),
                ("static".to_string(), "exiftool".to_string()),
            ]
        );
        assert_eq!(
            pipelines,
            vec![("static".to_string(), "triage".to_string())]
        );
    }

    /// `ReconcileIndex` collects every group a name appears under
    #[test]
    fn reconcile_index_collects_groups() {
        let idx = index(&[("a", "x", "1", "{}", "d1"), ("b", "x", "1", "{}", "d2")]);
        let expected: BTreeSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        assert_eq!(idx.groups.get("x"), Some(&expected));
    }

    /// A new toolbox gets the default name unless `--name` is set, and `--overwrite-config`
    /// applies `--name` over an existing config.toml while a kept one keeps its name
    #[test]
    fn resolve_settings_applies_name() {
        use super::{Bar, BarKind, resolve_settings};
        let progress = Bar::new_or_quiet("test", "", BarKind::Timer, true);
        // a new toolbox with no --name uses the default
        let mut cmd = export_cmd(Some("out"), None);
        let settings = resolve_settings(&cmd, None, &progress).unwrap();
        assert_eq!(settings.name, "My Toolbox");
        // write an existing config.toml to reuse
        let dir = std::env::temp_dir().join(format!("thorctl-name-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "name = \"Kept\"\n").unwrap();
        // a kept config.toml keeps its name even when --name differs
        cmd.name = Some("Flag".to_string());
        let kept = resolve_settings(&cmd, Some(&path), &progress).unwrap();
        assert_eq!(kept.name, "Kept");
        // --overwrite-config applies --name over the existing config
        cmd.overwrite_config = true;
        let overwritten = resolve_settings(&cmd, Some(&path), &progress).unwrap();
        assert_eq!(overwritten.name, "Flag");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
