//! Main entry point for toolbox imports
//!
//! Orchestrates the import workflow: loading and validating the manifest, resolving
//! `(group, name)` collisions, categorizing resources against the instance, confirming
//! with the user, then applying — creating missing groups and network policies, creating
//! new resources, and merging existing ones, pushing a bundled container image only for an
//! image that is created or updated — with every applied change journaled so a partial
//! import can be rolled back.

use colored::Colorize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use thorium::models::{ImageRequest, PipelineRequest};
use thorium::{CtlConf, Error, Thorium};

use super::manifest::ToolboxManifest;
use super::{build, collisions, policies, shared};
use crate::args::toolbox::{ImportToolbox, ManifestLocation};
use crate::handlers::container;
use crate::handlers::imports::categorize;
use crate::handlers::imports::kind::{ImageKind, PipelineKind};
use crate::handlers::imports::rollback::Journal;
use crate::handlers::imports::{
    self, ApplyCtx, ApplyOutcome, ConflictMode, ImportOutcome, ImportPlan, create,
};
use crate::handlers::progress::{Bar, BarKind};

/// Flatten a toolbox manifest's images into (name, version, request) tuples
/// for categorization, sorted by name then version so the plan order is stable
///
/// # Arguments
///
/// * `manifest` - The toolbox manifest to flatten
pub(super) fn flatten_manifest_images(
    manifest: &ToolboxManifest,
) -> Vec<(String, Option<String>, ImageRequest)> {
    // expand each image into one tuple per version, skipping versions with no
    // embedded config (build-only entries carry tags but no request to import)
    let mut images: Vec<(String, Option<String>, ImageRequest)> = manifest
        .images
        .iter()
        .flat_map(|(image_name, image_manifest)| {
            image_manifest
                .versions
                .iter()
                .filter_map(move |(version_name, version)| {
                    version.config.as_ref().map(|config| {
                        (
                            image_name.clone(),
                            Some(version_name.clone()),
                            config.clone(),
                        )
                    })
                })
        })
        .collect();
    // the manifest is a HashMap, so order the tuples by (name, version) for a stable plan
    images.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    images
}

/// Flatten a toolbox manifest's pipelines into (name, version, request) tuples
/// for categorization, sorted by name then version so the plan order is stable
///
/// # Arguments
///
/// * `manifest` - The toolbox manifest to flatten
pub(super) fn flatten_manifest_pipelines(
    manifest: &ToolboxManifest,
) -> Vec<(String, Option<String>, PipelineRequest)> {
    // expand each pipeline into one tuple per version, skipping versions with no
    // embedded config (a version dropped by validation has no request to import)
    let mut pipelines: Vec<(String, Option<String>, PipelineRequest)> = manifest
        .pipelines
        .iter()
        .flat_map(|(pipeline_name, pipeline_manifest)| {
            pipeline_manifest
                .versions
                .iter()
                .filter_map(move |(version_name, version)| {
                    version.config.as_ref().map(|config| {
                        (
                            pipeline_name.clone(),
                            Some(version_name.clone()),
                            config.clone(),
                        )
                    })
                })
        })
        .collect();
    // the manifest is a HashMap, so order the tuples by (name, version) for a stable plan
    pipelines.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    pipelines
}

// ─── Bundled Image Handling ──────────────────────────────────────────────────

/// A container image bundled in the toolbox that must be pushed to the target registry
struct BundledPush {
    /// The image's display identity (`group/name@version`), used for logging
    label: String,
    /// The path to the image's `.tar.gz` archive on disk
    tarball: PathBuf,
    /// The original image url to retag from (after the container load)
    source: String,
    /// The new registry url to tag and push to
    target: String,
}

/// Extract the tag from an image url, defaulting to "latest"
///
/// The tag is the substring after the last `:` that follows the last `/`, so registry
/// ports (e.g. `registry.local:5000/img`) are not mistaken for tags. A digest suffix
/// (`@sha256:...`) is not a usable push tag, so a digest-pinned reference falls back to
/// "latest" rather than treating the digest hex as the tag.
///
/// # Arguments
///
/// * `image_url` - The image url to extract a tag from
fn parse_tag(image_url: &str) -> &str {
    // isolate the final path segment so a registry port isn't read as a tag
    let last_segment = image_url.rsplit('/').next().unwrap_or(image_url);
    // drop any digest suffix; the repo portion before `@` is where a tag would live
    let without_digest = last_segment
        .split_once('@')
        .map_or(last_segment, |(repo, _digest)| repo);
    // the substring after the last `:` is the tag; a missing or empty tag (e.g. a
    // digest-only or bare reference) falls back to "latest" rather than guessing
    match without_digest.rsplit_once(':') {
        Some((_, tag)) if !tag.is_empty() => tag,
        _ => "latest",
    }
}

/// The non-interactive registry base path for bundled images, if one is set
///
/// Resolution order: the `--image-path-prefix` flag, then the prefix recorded in the
/// toolbox manifest; empty values count as unset. Both `toolbox import` and
/// `toolbox diff` resolve the prefix through here.
///
/// # Arguments
///
/// * `flag` - The `--image-path-prefix` flag, if given
/// * `manifest` - The toolbox manifest (provides a recorded fallback prefix)
pub(super) fn configured_image_path_prefix<'a>(
    flag: Option<&'a str>,
    manifest: &'a ToolboxManifest,
) -> Option<&'a str> {
    // an explicit flag wins, then the manifest's recorded prefix
    flag.filter(|prefix| !prefix.is_empty()).or_else(|| {
        manifest
            .image_path_prefix
            .as_deref()
            .filter(|prefix| !prefix.is_empty())
    })
}

/// A bundled image's container url rewritten to its target registry location
#[derive(Debug, PartialEq, Eq)]
pub(super) struct BundledRewrite {
    /// The index of the rewritten image in the categorized image list
    pub(super) index: usize,
    /// The image's original container url
    pub(super) source: String,
    /// The url the image now points at (`<prefix>/<group>/<name>:<tag>`)
    pub(super) target: String,
}

/// The result of rewriting a toolbox's bundled image urls
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct BundledRewrites {
    /// The images whose url was rewritten to their target registry location
    pub(super) rewrites: Vec<BundledRewrite>,
    /// The indexes of K8s images that carry no container url to rewrite
    pub(super) missing_url: Vec<usize>,
    /// The indexes of images whose scaler needs no container image, so nothing is bundled
    pub(super) no_container: Vec<usize>,
}

/// The target registry url a bundled image is pushed to and stored as
///
/// Returns `None` when the image carries no container url, since there is nothing
/// to retag.
///
/// # Arguments
///
/// * `prefix` - The registry base path (trailing slashes are ignored)
/// * `group` - The group the image is imported into
/// * `name` - The image's name
/// * `url` - The image's current container url, if any
fn bundled_target(prefix: &str, group: &str, name: &str, url: Option<&str>) -> Option<String> {
    // an image with no container url has nothing to bundle or retag
    let source = url.filter(|url| !url.is_empty())?;
    // keep the original tag under the new registry path
    let prefix = prefix.trim_end_matches('/');
    Some(format!("{prefix}/{group}/{name}:{}", parse_tag(source)))
}

/// Rewrite each bundled image's container url in place to its target registry location
///
/// This is the single rewrite `toolbox import` and `toolbox diff` share, so a diff
/// compares exactly the url an import would store. Images whose scaler needs no
/// container image (anything but K8s) are never bundled, so they are left untouched
/// and reported in `no_container`. K8s images with no container url are left
/// untouched and reported in `missing_url` so the caller can decide whether to warn.
///
/// # Arguments
///
/// * `prefix` - The registry base path bundled images are pushed under
/// * `images` - The categorized images whose `request.image` urls are rewritten
pub(super) fn rewrite_bundled_urls(
    prefix: &str,
    images: &mut [categorize::CategorizedImage],
) -> BundledRewrites {
    // track the rewritten images and the ones left untouched
    let mut rewrites = BundledRewrites::default();
    for (index, img) in images.iter_mut().enumerate() {
        // images that run without a container have no bundled tarball to push
        if !build::scaler_requires_container_image(img.request.scaler) {
            rewrites.no_container.push(index);
            continue;
        }
        // compute the target url, noting images that have no url to rewrite
        let Some(target) = bundled_target(
            prefix,
            &img.request.group,
            &img.request.name,
            img.request.image.as_deref(),
        ) else {
            rewrites.missing_url.push(index);
            continue;
        };
        // point the image at its new registry location, keeping the original for the push
        let source = img
            .request
            .image
            .replace(target.clone())
            .unwrap_or_default();
        rewrites.rewrites.push(BundledRewrite {
            index,
            source,
            target,
        });
    }
    rewrites
}

/// Determine the registry base path bundled images should be pushed under
///
/// Resolution order: the `--image-path-prefix` flag, then the prefix recorded in the
/// toolbox manifest, then an interactive prompt. Errors when neither is set and the
/// session can't prompt (`--overwrite`, `--skip-conflicts`, or no terminal).
///
/// # Arguments
///
/// * `cmd` - The import command (provides the `--image-path-prefix` flag)
/// * `manifest` - The toolbox manifest (provides a recorded fallback prefix)
/// * `can_prompt` - Whether the session can ask for the prefix (interactive mode with a terminal)
/// * `progress` - The progress bar, suspended while prompting
fn resolve_image_path_prefix(
    cmd: &ImportToolbox,
    manifest: &ToolboxManifest,
    can_prompt: bool,
    progress: &Bar,
) -> Result<String, Error> {
    // an explicit flag wins, then the prefix recorded in the toolbox
    if let Some(prefix) = configured_image_path_prefix(cmd.image_path_prefix.as_deref(), manifest) {
        return Ok(prefix.to_string());
    }
    // we can't ask for the prefix when running non-interactively (--overwrite,
    // --skip-conflicts, or no terminal)
    if !can_prompt {
        return Err(Error::new(
            "This toolbox bundles container images; pass --image-path-prefix <registry-base> \
             to choose where they are pushed",
        ));
    }
    // prompt for a target registry base path
    progress.suspend(|| {
        dialoguer::Input::<String>::new()
            .with_prompt(
                "Target registry base path (e.g. registry.local/base) to push bundled images to",
            )
            .interact_text()
            .map_err(|e| Error::new(format!("Failed to read prefix input: {e}")))
    })
}

/// Prepare bundled images for import without performing any container work yet
///
/// Resolves the target registry prefix, computes each image's new url
/// (`<prefix>/<group>/<name>:<tag>`), rewrites the request to point there (so the
/// confirmation reflects the final location), and returns the work needed to push
/// each archive, keyed by the image's label. The actual load/tag/push is deferred
/// until the image is about to be created or updated (see [`push_bundled_images`]).
///
/// # Arguments
///
/// * `cmd` - The import command
/// * `manifest` - The toolbox manifest
/// * `images` - The categorized images, whose `request.image` urls are rewritten in place
/// * `renames` - New manifest key to original key for collision-renamed images, used
///   to resolve a tarball saved on disk under the original name
/// * `can_prompt` - Whether the session can ask for a missing registry prefix
/// * `progress` - The progress bar
fn prepare_bundled_images(
    cmd: &ImportToolbox,
    manifest: &ToolboxManifest,
    images: &mut [categorize::CategorizedImage],
    renames: &HashMap<String, String>,
    can_prompt: bool,
    progress: &Bar,
) -> Result<HashMap<String, BundledPush>, Error> {
    // bundled toolboxes carry tarballs on disk, so the manifest must be a local path
    let base_dir = match &cmd.manifest {
        ManifestLocation::Path(path) => path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        ManifestLocation::Url(_) => {
            return Err(Error::new(
                "This toolbox bundles container images and must be imported from a local path, not \
                 a URL; download the toolbox directory (including its images/ tarballs) and import \
                 the local toolbox.json",
            ));
        }
    };
    // resolve where bundled images go and rewrite every image url to point there
    let prefix = resolve_image_path_prefix(cmd, manifest, can_prompt, progress)?;
    let rewritten = rewrite_bundled_urls(&prefix, images);
    // an image whose scaler runs without a container has no tarball, which is expected
    for index in rewritten.no_container {
        progress.info_anonymous(format!(
            "Bundled toolbox image '{}' uses the {} scaler, which needs no container image; \
             nothing to push for it",
            images[index].name, images[index].request.scaler
        ));
    }
    // a K8s image with no container url has nothing to bundle/retag; warn so it isn't a
    // silent gap (the image is still created pointing at whatever url it carries)
    for index in rewritten.missing_url {
        progress.warning(format!(
            "Bundled toolbox image '{}' has no container url; nothing to push for it",
            images[index].name
        ));
    }
    // resolve each rewritten image's on-disk archive so it can be pushed after confirmation
    let mut pushes = HashMap::with_capacity(rewritten.rewrites.len());
    for rewrite in rewritten.rewrites {
        let img = &images[rewrite.index];
        // a collision rename re-keys the image but leaves its tarball saved under the original
        // on-disk name, so the archive file is named for that original key. `img.name` is the
        // (possibly renamed) manifest key; `renames` maps it back to the original key.
        let source_name = renames
            .get(&img.name)
            .map_or(img.name.as_str(), String::as_str);
        // the tool's directory is recorded per-image in toolbox.json (built for all images), so the
        // tarball is found wherever export placed it (configured layout or a `=dir`). A collision
        // rename moves the whole version entry (its `dir` included) under the new key in
        // `manifest.images` (see `rename_image_member`), so the dir is looked up by the current key
        // `img.name`. The on-disk tarball file, however, is still named for the original key, which is
        // why `source_name` (above) is recovered from `renames`. An empty dir means an older toolbox
        // that predates the field — fall back to the historical `images/<name>` layout.
        let dir = manifest
            .images
            .get(&img.name)
            .zip(img.version.as_ref())
            .and_then(|(image_manifest, version)| image_manifest.versions.get(version))
            .map_or("", |version| version.dir.as_str());
        let tarball = if dir.is_empty() {
            base_dir
                .join("images")
                .join(source_name)
                .join(format!("{source_name}.tar.gz"))
        } else {
            base_dir.join(dir).join(format!("{source_name}.tar.gz"))
        };
        // queue the archive to be loaded, retagged, and pushed after confirmation
        pushes.insert(
            img.label(),
            BundledPush {
                label: img.label(),
                tarball,
                source: rewrite.source,
                target: rewrite.target,
            },
        );
    }
    Ok(pushes)
}

/// Load, retag, and push one bundled image to the target registry
///
/// # Arguments
///
/// * `push` - The bundled image to push, as prepared by [`prepare_bundled_images`]
/// * `progress` - The progress bar to report container status through
async fn push_bundled_image(push: &BundledPush, progress: &Bar) -> Result<(), Error> {
    // make sure the archive exists before trying to load it; a stat error is surfaced
    // rather than masked as "missing"
    let exists = tokio::fs::try_exists(&push.tarball)
        .await
        .map_err(|e| Error::new(format!("failed to stat '{}': {e}", push.tarball.display())))?;
    if !exists {
        return Err(Error::new(format!(
            "bundled image archive not found at '{}' (was the toolbox exported with \
             --with-images and copied whole?)",
            push.tarball.display()
        )));
    }
    // load the archive into the local container runtime
    container::load(&push.tarball, progress).await?;
    // retag from the loaded archive's tag (`source`) to the target; surface the expected
    // source tag so a save/load naming mismatch is debuggable
    container::tag(&push.source, &push.target, progress)
        .await
        .map_err(|e| {
            Error::new(format!(
                "tagging '{}' -> '{}' failed (does the loaded archive contain '{}'?): {e}",
                push.source, push.target, push.source
            ))
        })?;
    // push the retagged container to the target registry
    container::push(&push.target, progress).await
}

/// When a bundled image's container is pushed relative to its create or update
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushTiming {
    /// Pushed right before the image is created or updated; a failed push skips that
    Before,
    /// Pushed right after an interactive merge applied an update to the image
    AfterUpdate,
}

/// Push the bundled containers of a set of images, best-effort and one at a time
///
/// Images with no bundled container (no url, or a scaler that runs without one) need
/// no push and are always returned as ready. A failed push is warned about and
/// collected as a failure, and that image is left out of the returned list so its
/// create or update is skipped. Kept sequential on purpose: container load/push
/// report their own progress, and parallel container output would interleave.
///
/// # Arguments
///
/// * `pushes` - The bundled images to push, keyed by image label
/// * `images` - The images about to be created or updated (or just updated)
/// * `timing` - Whether the push happens before or after the image is written
/// * `progress` - The progress bar to update as images are pushed
async fn push_bundled_images<'b>(
    pushes: &HashMap<String, BundledPush>,
    images: Vec<&'b categorize::CategorizedImage>,
    timing: PushTiming,
    progress: &Bar,
) -> (Vec<&'b categorize::CategorizedImage>, Vec<String>) {
    // count the images that actually carry a bundled container
    let to_push = images
        .iter()
        .filter(|img| pushes.contains_key(&img.label()))
        .count();
    if to_push > 0 {
        progress.refresh("Pushing bundled images", BarKind::Bound(to_push as u64));
    }
    let mut ready = Vec::with_capacity(images.len());
    let mut failures = Vec::new();
    for img in images {
        // images without a bundled container are ready as-is
        let Some(push) = pushes.get(&img.label()) else {
            ready.push(img);
            continue;
        };
        // push this image's container independently so one failure doesn't stop the rest
        match push_bundled_image(push, progress).await {
            Ok(()) => ready.push(img),
            Err(err) => {
                // say what the failed push means for this image's create/update
                let consequence = match timing {
                    PushTiming::Before => "skipping its create/update".to_string(),
                    PushTiming::AfterUpdate => format!(
                        "its Thorium image was updated but won't run until its container is \
                         pushed to '{}'",
                        push.target
                    ),
                };
                progress.warning(format!(
                    "Failed to push bundled image '{}': {err}; {consequence}",
                    push.label
                ));
                failures.push(format!("{} (bundled image push)", push.label));
            }
        }
        progress.inc(1);
    }
    (ready, failures)
}

/// Apply the existing images, pushing bundled containers only for images that change
///
/// Mirrors `images import`: force-updated images are pushed right before their
/// update, interactively merged images right after the user applied an update, and
/// skipped or unchanged images are never pushed.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor in interactive merges)
/// * `ctx` - The shared apply settings
/// * `existing` - The existing images to apply
/// * `pushes` - The bundled images to push, keyed by image label
/// * `progress` - The progress bar
/// * `journal` - The journal recording applied changes for rollback
async fn apply_existing_images(
    thorium: &Thorium,
    conf: &CtlConf,
    ctx: &ApplyCtx<'_>,
    existing: Vec<&categorize::CategorizedImage>,
    pushes: &HashMap<String, BundledPush>,
    progress: &Bar,
    journal: &Journal,
) -> Result<ApplyOutcome, Error> {
    match ctx.mode {
        ConflictMode::Force => {
            // push only the images that will actually be updated, then force-update
            let (changing, unchanged): (Vec<_>, Vec<_>) = existing
                .into_iter()
                .partition(|img| imports::image_would_change(img));
            let (ready, mut failures) =
                push_bundled_images(pushes, changing, PushTiming::Before, progress).await;
            let to_update = ready.into_iter().chain(unchanged).collect();
            let applied = Box::pin(imports::apply_existing::<ImageKind>(
                thorium,
                conf,
                to_update,
                ctx.mode,
                ctx.editor,
                ctx.can_prompt,
                ctx.workers,
                progress,
                journal,
            ))
            .await?;
            failures.extend(applied.failures);
            Ok(ApplyOutcome {
                outcome: applied.outcome,
                failures,
            })
        }
        ConflictMode::Interactive if ctx.can_prompt => {
            // merge one image at a time so its container is pushed only once the user has
            // chosen to apply an update to it
            let mut failures = Vec::new();
            for img in existing {
                let applied_before = journal.len();
                let applied = Box::pin(imports::apply_existing::<ImageKind>(
                    thorium,
                    conf,
                    vec![img],
                    ctx.mode,
                    ctx.editor,
                    ctx.can_prompt,
                    ctx.workers,
                    progress,
                    journal,
                ))
                .await?;
                failures.extend(applied.failures);
                // the journal grows exactly when an update was applied to this image
                if journal.len() > applied_before {
                    let (_, push_failures) =
                        push_bundled_images(pushes, vec![img], PushTiming::AfterUpdate, progress)
                            .await;
                    failures.extend(push_failures);
                }
                // a Quit stops the remaining images
                if applied.outcome == ImportOutcome::Quit {
                    return Ok(ApplyOutcome {
                        outcome: ImportOutcome::Quit,
                        failures,
                    });
                }
            }
            Ok(ApplyOutcome {
                outcome: ImportOutcome::Completed,
                failures,
            })
        }
        _ => {
            // skip-conflicts (or no terminal) never updates existing images, so no pushes
            Box::pin(imports::apply_existing::<ImageKind>(
                thorium,
                conf,
                existing,
                ctx.mode,
                ctx.editor,
                ctx.can_prompt,
                ctx.workers,
                progress,
                journal,
            ))
            .await
        }
    }
}

// ─── Apply Phase ─────────────────────────────────────────────────────────────

/// Apply a categorized toolbox import: create groups/policies, create new
/// resources, then resolve existing ones per the conflict mode
///
/// Bundled containers are pushed only for images that are created or updated:
/// new images right before they are created, and existing images as described in
/// [`apply_existing_images`]. A failed push skips that image's create or update
/// and is collected as a failure.
///
/// Returns how the apply phase ended without short-circuiting on a Quit, so the
/// caller can settle the journal (offer/apply rollback) afterwards. A Quit in the
/// image merge stops the pipeline pass too, so the rollback offer covers both.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor in interactive merges)
/// * `ctx` - The shared apply settings (mode, editor, prompting, workers)
/// * `plan` - The categorized resources and missing groups to apply
/// * `policy_plan` - The categorized network policies to create/warn about
/// * `bundled_pushes` - Bundled container images to push, keyed by image label
/// * `progress` - The progress bar
/// * `journal` - The journal recording applied changes for rollback
#[allow(clippy::too_many_arguments)]
async fn apply_resources(
    thorium: &Thorium,
    conf: &CtlConf,
    ctx: &ApplyCtx<'_>,
    plan: ImportPlan<'_>,
    policy_plan: &policies::PolicyPlan,
    bundled_pushes: &HashMap<String, BundledPush>,
    progress: &Bar,
    journal: &Journal,
) -> Result<ApplyOutcome, Error> {
    // create any missing groups
    if !plan.missing_groups.is_empty() {
        progress.refresh(
            "Creating groups",
            BarKind::Bound(plan.missing_groups.len() as u64),
        );
        imports::create_groups(
            thorium,
            plan.missing_groups.clone(),
            ctx.workers,
            progress,
            journal,
        )
        .await?;
    }
    // create missing network policies before the images that reference them
    policies::create_policies(thorium, &policy_plan.new, progress, journal).await?;
    // apply planned policy updates before those images too (empty unless
    // --update-network-policy is set)
    policies::update_policies(thorium, &policy_plan.updates, progress, journal).await?;
    // existing-but-different policies left in place are surfaced (empty when updating)
    policies::warn_mismatched(policy_plan, progress);
    // push the new images' bundled containers, then create the images whose push worked
    let (new_images, mut failures) = push_bundled_images(
        bundled_pushes,
        plan.new_images,
        PushTiming::Before,
        progress,
    )
    .await;
    // import new resources, collecting per-resource failures so one bad image/pipeline
    // doesn't abort the rest (a pipeline whose image failed will fail too, and is
    // collected the same way)
    failures.extend(
        create::import_new_images(thorium, new_images, ctx.workers, progress, journal).await,
    );
    failures.extend(
        create::import_new_pipelines(thorium, plan.new_pipelines, ctx.workers, progress, journal)
            .await,
    );
    // handle existing resources via the shared dispatch; a Quit in the image pass
    // stops the pipeline pass too so the rollback offer covers everything. Note the asymmetry:
    // this pass is fail-fast — an interactive merge apply error propagates (via `?`) to settle
    // the journal and offer rollback — whereas the create passes above collect per-resource
    // failures and keep going.
    let images_applied = apply_existing_images(
        thorium,
        conf,
        ctx,
        plan.existing_images,
        bundled_pushes,
        progress,
        journal,
    )
    .await?;
    failures.extend(images_applied.failures);
    if images_applied.outcome == ImportOutcome::Quit {
        return Ok(ApplyOutcome {
            outcome: ImportOutcome::Quit,
            failures,
        });
    }
    let pipelines_applied = imports::apply_existing::<PipelineKind>(
        thorium,
        conf,
        plan.existing_pipelines,
        ctx.mode,
        ctx.editor,
        ctx.can_prompt,
        ctx.workers,
        progress,
        journal,
    )
    .await?;
    failures.extend(pipelines_applied.failures);
    Ok(ApplyOutcome {
        outcome: pipelines_applied.outcome,
        failures,
    })
}

// ─── Main Import Entry Point ─────────────────────────────────────────────────

/// Import a toolbox into Thorium by the given manifest file.
///
/// When images or pipelines already exist, the user is prompted interactively
/// to Edit (merge editor), Skip, Apply (accept incoming), or Quit for each
/// changed resource. `--overwrite` skips the editor and auto-applies all changes;
/// `--skip-conflicts` creates only new resources and leaves differing existing
/// ones untouched with a warning.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The toolbox import command that was run
/// * `workers` - Max concurrent API actions in the apply phase (the global `--workers`)
#[allow(clippy::too_many_lines)]
pub async fn import(
    thorium: Thorium,
    conf: CtlConf,
    cmd: &ImportToolbox,
    workers: usize,
) -> Result<(), Error> {
    // get the manifest from the location along with a progress bar
    let (mut manifest, progress) =
        shared::get_manifest_named(&cmd.manifest, "toolbox import").await?;
    // resolve any URL-based configs before validation; any unreachable url aborts here,
    // before anything is changed in Thorium
    shared::resolve_manifest_configs(&mut manifest, &progress).await?;
    // 1) structural validation, BEFORE any override: drop intrinsically-invalid
    //    image/pipeline versions (unresolved configs, references to images absent
    //    from the manifest), warning about each
    shared::warn_dropped(&manifest.validate_structural(), &progress);
    // 2) snapshot each entry's pre-override group so collision resolution can
    //    disambiguate which pipeline wanted which image variant
    let source_groups = manifest.capture_source_groups();
    // 3) apply the group override
    if let Some(group_override) = &cmd.group_override {
        progress.info_anonymous(format!(
            "Forcing all images and pipelines into group '{}'",
            group_override.bright_yellow()
        ));
        manifest = manifest.override_group(group_override);
    }
    // 4) group-coherence validation, AFTER the override: drop pipelines whose order
    //    references images not present in their (final) group
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // resolve how existing resources will be handled up front so the
    // confirmation screen can describe it accurately
    let mode = ConflictMode::from_flags(cmd.overwrite, cmd.skip_conflicts);
    // a session can prompt only in the interactive default (--overwrite/--skip-conflicts
    // are explicit non-interactive opt-outs) and only with a real terminal. dialoguer reads
    // keys from stdin but draws its prompts on stderr, so both must be terminals or a prompt
    // would fail midway (e.g. `2>&1 | tee log`). This gates collision prompts, the merge
    // editor, the plan confirmation, and rollback.
    let can_prompt = mode == ConflictMode::Interactive
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stderr());
    // 5) detect & resolve (group, name) collisions the override (or a hand-authored
    //    manifest) introduced: rename + repoint interactively, else skip + warn,
    //    so the import never silently overwrites one resource with another
    let image_renames =
        collisions::resolve_collisions(&mut manifest, &source_groups, can_prompt, &progress)?;
    // 6) re-check coherence after any renames repointed pipelines
    shared::warn_dropped(&manifest.validate_group_coherence(), &progress);
    // get all the groups the (surviving) manifest expects to exist; computing this
    // after dropping/resolution avoids creating groups only needed by skipped resources
    let manifest_groups = if let Some(group_override) = &cmd.group_override {
        HashSet::from([group_override.clone()])
    } else {
        manifest.groups()
    };
    // categorize images and pipelines by checking what already exists in Thorium
    let mut images =
        categorize::categorize_images(&thorium, flatten_manifest_images(&manifest), &progress)
            .await?;
    let pipelines = categorize::categorize_pipelines(
        &thorium,
        flatten_manifest_pipelines(&manifest),
        &progress,
    )
    .await?;
    // if this toolbox bundles container images, resolve where they'll be pushed and rewrite
    // each image url now so the confirmation reflects the final location; the actual
    // load/tag/push is deferred until after the user confirms
    let bundled_pushes = if manifest.bundled_images {
        prepare_bundled_images(
            cmd,
            &manifest,
            &mut images,
            &image_renames,
            can_prompt,
            &progress,
        )?
    } else {
        // --image-path-prefix only affects bundled toolboxes; warn so it isn't a silent no-op
        if cmd.image_path_prefix.is_some() {
            progress.warning(
                "--image-path-prefix has no effect: this toolbox does not bundle container images",
            );
        }
        HashMap::new()
    };
    // collect the bundled network policies and check them against the target
    let policy_plan = policies::categorize_policies(
        &thorium,
        policies::collect_policies(&manifest, cmd.group_override.as_deref(), &progress),
        cmd.update_network_policy,
        &progress,
    )
    .await?;
    // check which groups are missing
    let missing_groups = imports::get_missing_groups(&thorium, manifest_groups.clone())
        .await
        .map_err(|err| Error::new(format!("Failed to retrieve missing groups: {err}")))?;
    // partition into new vs existing for the confirmation summary
    let plan = ImportPlan::new(&images, &pipelines, missing_groups);
    // confirm before an interactive merge OR before creating anything sensitive:
    // network policies are cluster security state and groups are access boundaries,
    // so neither should appear silently even in an otherwise clean import. This only
    // fires when `can_prompt` holds (interactive mode + TTY); force, skip-conflicts,
    // and non-TTY sessions never prompt.
    if can_prompt
        && (plan.has_conflicts()
            || !policy_plan.new.is_empty()
            || !policy_plan.updates.is_empty()
            || !policy_plan.mismatched.is_empty()
            || !plan.missing_groups.is_empty())
    {
        // the username is only shown in the prompt, so a failed lookup falls back to a placeholder
        let username = imports::current_username(&thorium, &progress).await;
        let confirmed = progress.suspend(|| {
            policies::print_plan(&policy_plan);
            imports::confirm_import(&conf, &plan, &username, mode)
        })?;
        // declining leaves Thorium untouched; say so and close the bar instead of exiting silently
        if !confirmed {
            progress.refresh("Import cancelled; nothing was changed", BarKind::Timer);
            progress.finish();
            return Ok(());
        }
    }
    // journal every applied change so a partial import can be rolled back
    let journal = Journal::new();
    // run the apply phase, capturing how it ended without short-circuiting so
    // the journal can be settled (rollback offered/applied) afterwards
    let ctx = ApplyCtx {
        mode,
        editor: cmd.editor.as_deref(),
        can_prompt,
        workers,
    };
    let result = apply_resources(
        &thorium,
        &conf,
        &ctx,
        plan,
        &policy_plan,
        &bundled_pushes,
        &progress,
        &journal,
    )
    .await;
    // per-resource failures are kept (not rolled back) and reported below; settle the
    // journal only on the outcome/error so rollback still covers a Quit or fatal error
    let (settle_input, failures) = match result {
        Ok(applied) => (Ok(applied.outcome), applied.failures),
        Err(err) => (Err(err), Vec::new()),
    };
    // only an interactive session can be asked about rollback; without a terminal
    // (CI, pipes) or in a non-interactive mode, nobody can answer, so settle_journal
    // falls back to --rollback-on-failure. The same `can_prompt` that gated the
    // confirmation is reused here so the rollback offer matches the session.
    let outcome = imports::settle_journal(
        &thorium,
        &progress,
        journal,
        settle_input,
        can_prompt,
        cmd.rollback_on_failure,
    )
    .await?;
    // print the final banner and fail if any resource failed to import
    imports::finish_import(&progress, outcome, &failures)
}

/// Unit tests for tag parsing, the one piece of bundled-image url handling that
/// is pure and testable without a registry or container runtime
#[cfg(test)]
mod tests {
    use super::*;
    /// A plain `name:tag` reference returns its tag
    #[test]
    fn parse_tag_reads_explicit_tag() {
        assert_eq!(parse_tag("registry.local/group/img:v1.2"), "v1.2");
    }
    /// A reference with no tag defaults to "latest"
    #[test]
    fn parse_tag_defaults_to_latest() {
        assert_eq!(parse_tag("registry.local/group/img"), "latest");
    }
    /// A registry port in the host is not mistaken for the tag
    #[test]
    fn parse_tag_ignores_registry_port() {
        assert_eq!(parse_tag("registry.local:5000/img"), "latest");
        assert_eq!(parse_tag("registry.local:5000/img:v2"), "v2");
    }
    /// A digest-pinned reference has no usable tag, so it falls back to "latest"
    /// rather than treating the digest hex as the tag
    #[test]
    fn parse_tag_digest_falls_back_to_latest() {
        assert_eq!(
            parse_tag("registry.local/img@sha256:abc123def456"),
            "latest"
        );
        assert_eq!(parse_tag("registry.local/img@sha256"), "latest");
    }
    /// The bundled target url keeps the original tag under `<prefix>/<group>/<name>`,
    /// ignores trailing prefix slashes, and is absent for images with no url
    #[test]
    fn bundled_target_builds_registry_url() {
        assert_eq!(
            bundled_target("reg.local/base/", "g", "tool", Some("docker.io/x/tool:v3")),
            Some("reg.local/base/g/tool:v3".to_string())
        );
        assert_eq!(
            bundled_target("reg.local:5000", "g", "tool", Some("tool")),
            Some("reg.local:5000/g/tool:latest".to_string())
        );
        assert_eq!(bundled_target("reg.local", "g", "tool", None), None);
        assert_eq!(bundled_target("reg.local", "g", "tool", Some("")), None);
    }
    /// Rewriting updates each K8s image's url in place and reports url-less and
    /// container-less images
    #[test]
    fn rewrite_bundled_urls_rewrites_in_place() {
        // build one image with a url and one without
        let image = |name: &str, url: Option<&str>| {
            let mut request = ImageRequest::new("g", name);
            request.image = url.map(str::to_string);
            categorize::CategorizedImage {
                name: name.to_string(),
                version: Some("1.0".to_string()),
                request,
                existing: None,
            }
        };
        let mut bare_metal = image("c", Some("hub/c:v1"));
        bare_metal.request.scaler = thorium::models::ImageScaler::BareMetal;
        let mut images = vec![image("a", Some("hub/a:v1")), image("b", None), bare_metal];
        let rewritten = rewrite_bundled_urls("reg/base", &mut images);
        assert_eq!(
            rewritten.rewrites,
            vec![BundledRewrite {
                index: 0,
                source: "hub/a:v1".to_string(),
                target: "reg/base/g/a:v1".to_string(),
            }]
        );
        assert_eq!(rewritten.missing_url, vec![1]);
        // a non-K8s image is never bundled, so its url is left as-is
        assert_eq!(rewritten.no_container, vec![2]);
        assert_eq!(images[0].request.image.as_deref(), Some("reg/base/g/a:v1"));
        assert_eq!(images[1].request.image, None);
        assert_eq!(images[2].request.image.as_deref(), Some("hub/c:v1"));
    }
    /// Only a non-empty flag or recorded prefix counts as configured, flag first
    #[test]
    fn configured_prefix_prefers_flag() {
        let mut manifest: ToolboxManifest = serde_json::from_value(serde_json::json!({
            "name": "t",
            "pipelines": {},
            "images": {},
        }))
        .unwrap();
        assert_eq!(configured_image_path_prefix(None, &manifest), None);
        manifest.image_path_prefix = Some("recorded".to_string());
        assert_eq!(
            configured_image_path_prefix(Some(""), &manifest),
            Some("recorded")
        );
        assert_eq!(
            configured_image_path_prefix(Some("flag"), &manifest),
            Some("flag")
        );
    }
    /// A reference carrying both a tag and a digest keeps the real tag
    #[test]
    fn parse_tag_tag_and_digest_keeps_tag() {
        assert_eq!(parse_tag("registry.local/img:v1.2@sha256:abc123"), "v1.2");
    }
}
