//! Image import support for thorctl
//!
//! Imports image configs from an on-disk export directory through the shared
//! conflict engine: new images are created, existing ones are merged
//! interactively, force-updated, or skipped with a field-level warning
//! depending on the conflict mode. Every applied change is journaled so a
//! partial import can be rolled back by the caller.
//!
//! Registry rewrites (`--registry`, `--registry-override`) are applied to the
//! requests before they are categorized, so the confirmation screen and the
//! conflict checks see the urls that will actually be stored. Container tarballs
//! are only loaded and pushed for images that are actually created or updated.

use futures::{StreamExt, stream};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thorium::models::{ImageRequest, ImageScaler, ImageUpdate};
use thorium::{CtlConf, Error, Thorium};

use crate::args::images::ImportImages;
use crate::handlers::container;
use crate::handlers::imports::kind::ImageKind;
use crate::handlers::imports::rollback::Journal;
use crate::handlers::imports::{
    self, ApplyCtx, ApplyOutcome, ConflictMode, ImportOutcome, categorize, create,
};
use crate::handlers::progress::{Bar, BarKind};

/// The options that drive an image import pass
///
/// Borrowed from the `images import` command directly, or assembled by
/// `pipelines import` for its image pass.
pub struct ImageImportOpts<'a> {
    /// The directory holding `images/<name>.json` configs (and tarballs)
    pub import_dir: &'a Path,
    /// The group to import the images into
    pub group: &'a str,
    /// The registry to retag and push loaded container images to
    pub registry: Option<&'a str>,
    /// The registry to override image urls with in Thorium
    pub registry_override: Option<&'a str>,
    /// Skip container load/retag/push entirely
    pub skip_push: bool,
    /// Only update existing images' registry urls instead of full conflict handling
    pub migrate_registry: bool,
    /// How to handle images that already exist
    pub mode: ConflictMode,
    /// The editor override for interactive merges
    pub editor: Option<&'a str>,
    /// Whether the session has a terminal (the prerequisite for any interactive
    /// prompt; the mode decides whether a prompt is actually wanted)
    pub is_terminal: bool,
    /// Max concurrent API actions in the apply phase (the global `--workers`)
    pub workers: usize,
}

impl<'a> ImageImportOpts<'a> {
    /// Build image import options from the `images import` command
    ///
    /// # Arguments
    ///
    /// * `cmd` - The import command to pull options from
    /// * `workers` - Max concurrent API actions in the apply phase (`--workers`)
    pub fn from_cmd(cmd: &'a ImportImages, workers: usize) -> Self {
        Self {
            import_dir: &cmd.import,
            group: &cmd.group,
            registry: cmd.registry.as_deref(),
            registry_override: cmd.registry_override.as_deref(),
            skip_push: cmd.skip_push,
            migrate_registry: cmd.migrate_registry,
            mode: ConflictMode::from_flags(cmd.overwrite, cmd.skip_conflicts),
            editor: cmd.editor.as_deref(),
            is_terminal: imports::is_interactive_terminal(),
            workers,
        }
    }
}

/// Whether the first component of an image reference names a registry host
///
/// Follows docker's reference rule: the first `/`-separated component is a
/// registry only if it contains a `.` or a `:` or is exactly `localhost`;
/// otherwise it is a Docker Hub namespace (e.g. `myorg` in `myorg/tool:1.0`).
///
/// # Arguments
///
/// * `component` - The first `/`-separated component of an image reference
fn is_registry_host(component: &str) -> bool {
    component.contains('.') || component.contains(':') || component == "localhost"
}

/// Rewrite an image url onto a new registry, keeping its full repository path
///
/// Only a leading registry host is replaced; Docker Hub style references keep
/// their namespace (`myorg/tool:1.0` becomes `<registry>/myorg/tool:1.0`). A
/// trailing `/` on `registry` is ignored, and an empty registry leaves the url
/// unchanged.
///
/// # Arguments
///
/// * `url` - The image url to rewrite
/// * `registry` - The registry to rewrite onto
fn rebase_image_url(url: &str, registry: &str) -> String {
    // tolerate a trailing slash on the registry argument
    let registry = registry.trim_end_matches('/');
    if registry.is_empty() {
        return url.to_string();
    }
    // drop the leading registry host, if the url has one
    let path = match url.split_once('/') {
        Some((first, rest)) if is_registry_host(first) => rest,
        _ => url,
    };
    format!("{registry}/{path}")
}

/// A container tarball to load and push for one image
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerPush {
    /// The tarball in the export directory to load
    tarball: PathBuf,
    /// The reference the tarball was saved with (the url in the exported config)
    source: String,
    /// The reference to push, after any `--registry` retag
    target: String,
}

/// How the container side of one image is handled by an import
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerPlan {
    /// The tarball to load and push, when this image's container is pushed
    push: Option<ContainerPush>,
    /// The url Thorium will store for this image
    stored_url: Option<String>,
    /// Why `--registry` can't be applied to this image, when it was given but no
    /// push happens
    registry_skipped: Option<&'static str>,
}

/// Decide how an image's container is handled and which url Thorium stores
///
/// A K8s-scaled image with a url and a tarball in the export is loaded and
/// pushed (unless `--skip-push`), retagged onto `--registry` if one was given,
/// and the pushed reference becomes the stored url. For every other image
/// `--registry` can't apply, so the reason is reported and the url is kept.
/// `--registry-override` then rewrites the stored url for every image.
///
/// # Arguments
///
/// * `opts` - The image import options
/// * `request` - The image request as loaded from the export
/// * `tarball` - The path the image's tarball would have in the export
/// * `tar_exists` - Whether that tarball exists
fn plan_container(
    opts: &ImageImportOpts<'_>,
    request: &ImageRequest,
    tarball: PathBuf,
    tar_exists: bool,
) -> ContainerPlan {
    // work out whether this image's container is pushed, and why not otherwise
    let not_pushed_reason = if opts.skip_push {
        Some("--skip-push was given")
    } else if request.scaler != ImageScaler::K8s {
        Some("it is not K8s-scaled, so its container is not imported")
    } else if request.image.is_none() {
        Some("it has no image url")
    } else if !tar_exists {
        Some("the export has no container archive for it (config-only)")
    } else {
        None
    };
    // build the push (and the url it produces) when the container is pushed
    let (push, stored_url, registry_skipped) = match (not_pushed_reason, &request.image) {
        (None, Some(source)) => {
            // retag onto --registry when given; otherwise push back to the source
            let target = opts.registry.map_or_else(
                || source.clone(),
                |registry| rebase_image_url(source, registry),
            );
            let push = ContainerPush {
                tarball,
                source: source.clone(),
                target: target.clone(),
            };
            (Some(push), Some(target), None)
        }
        (reason, url) => (
            None,
            url.clone(),
            reason.filter(|_| opts.registry.is_some()),
        ),
    };
    // the stored url override applies to every image
    let stored_url = match (stored_url, opts.registry_override) {
        (Some(url), Some(registry)) => Some(rebase_image_url(&url, registry)),
        (url, _) => url,
    };
    ContainerPlan {
        push,
        stored_url,
        registry_skipped,
    }
}

/// Load an image's request from the export and plan its container handling
///
/// # Arguments
///
/// * `opts` - The image import options
/// * `name` - The name of the image whose config and tarball to inspect
async fn load_container_plan(
    opts: &ImageImportOpts<'_>,
    name: &str,
) -> Result<(ImageRequest, ContainerPlan), Error> {
    // load the request as exported
    let request = categorize::load_request::<ImageKind>(opts.import_dir, opts.group, name).await?;
    // a real stat error (e.g. bad permissions) is surfaced, not hidden as "absent"
    let tarball = opts
        .import_dir
        .join("images")
        .join(format!("{name}.tar.gz"));
    let tar_exists = tokio::fs::try_exists(&tarball)
        .await
        .map_err(|e| Error::new(format!("Failed to stat '{}': {e}", tarball.display())))?;
    // decide how the container is handled
    let plan = plan_container(opts, &request, tarball, tar_exists);
    Ok((request, plan))
}

/// Load, registry-rewrite, and categorize the named images from an export directory
///
/// The urls are rewritten here, before categorizing, so the confirmation screen
/// and the conflict checks see the urls that will actually be stored. Names are
/// sorted and de-duplicated so the plan and prompts follow a stable order.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `opts` - The image import options
/// * `names` - The names of the images to import
/// * `progress` - The progress bar
pub async fn categorize_from_disk(
    thorium: &Thorium,
    opts: &ImageImportOpts<'_>,
    names: &[String],
    progress: &Bar,
) -> Result<Vec<categorize::CategorizedImage>, Error> {
    // process names in a stable order, once each
    let mut names = names.to_vec();
    names.sort_unstable();
    names.dedup();
    // load every request up front so malformed configs fail before any changes
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        let (mut request, plan) = load_container_plan(opts, &name).await?;
        // make it visible when --registry can't apply to this image
        if let Some(reason) = plan.registry_skipped {
            progress.warning(format!(
                "--registry not applied to image '{name}': {reason}; its stored url is kept"
            ));
        } else if plan.push.is_none()
            && request.scaler == ImageScaler::K8s
            && !opts.skip_push
            && request.image.is_some()
        {
            progress.info(format!(
                "No image archive for '{name}' (config-only); skipping container load/push"
            ));
        }
        // store the url the import will actually set
        request.image = plan.stored_url;
        items.push((name, None, request));
    }
    categorize::categorize_images(thorium, items, progress).await
}

/// Load, retag, and push one image's container if its plan calls for it
///
/// # Arguments
///
/// * `opts` - The image import options
/// * `name` - The name of the image whose container to push
/// * `bar` - The progress bar to update with container status
async fn push_container(opts: &ImageImportOpts<'_>, name: &str, bar: &Bar) -> Result<(), Error> {
    // re-derive the plan from the export so the push matches what was categorized
    let (_, plan) = load_container_plan(opts, name).await?;
    let Some(push) = plan.push else {
        return Ok(());
    };
    // load the tarball into the local container runtime
    container::load(&push.tarball, bar).await?;
    // retag onto the target registry when it differs from the saved reference
    if push.target != push.source {
        container::tag(&push.source, &push.target, bar).await?;
    }
    // push to the target registry
    container::push(&push.target, bar).await
}

/// Push the containers for a set of images, returning the images whose push
/// succeeded and the failure labels of the rest
///
/// Kept sequential on purpose: container load/push stream their own progress to
/// the terminal, and parallel container output would interleave into noise.
///
/// # Arguments
///
/// * `opts` - The image import options
/// * `images` - The images whose containers to push
/// * `progress` - The progress bar to report through
async fn push_containers<'b>(
    opts: &ImageImportOpts<'_>,
    images: Vec<&'b categorize::CategorizedImage>,
    progress: &Bar,
) -> (Vec<&'b categorize::CategorizedImage>, Vec<String>) {
    let mut pushed = Vec::with_capacity(images.len());
    let mut failures = Vec::new();
    for img in images {
        // a failed push skips this image's create/update but not the others
        match push_container(opts, &img.name, progress).await {
            Ok(()) => pushed.push(img),
            Err(err) => {
                progress.warning(format!(
                    "Failed to push the container for image '{}': {err}",
                    img.name
                ));
                failures.push(format!("{} (container push)", create::failure_label(img)));
            }
        }
    }
    (pushed, failures)
}

/// The result of an image apply pass
pub struct ImagesApplied {
    /// How the pass ended and the per-image failures it collected
    pub applied: ApplyOutcome,
    /// The names of images that don't exist in Thorium because their create
    /// (or container push) failed; pipelines referencing them can't be created
    pub missing: HashSet<String>,
}

/// Collect the names of new images whose create failed
///
/// # Arguments
///
/// * `new` - The new images that were attempted
/// * `failures` - The failure labels returned by the create pass
fn failed_names(new: &[&categorize::CategorizedImage], failures: &[String]) -> HashSet<String> {
    new.iter()
        .filter(|img| failures.contains(&create::failure_label(*img)))
        .map(|img| img.request.name.clone())
        .collect()
}

/// Apply the categorized images to Thorium according to the conflict mode
///
/// Containers are pushed only for images that are actually created or updated:
/// new images are pushed right before they are created, force-updated images
/// right before their update, and interactively merged images right after the
/// user chose to apply an update to them. Skipped and unchanged images are never
/// pushed. A failed push is collected as a failure and the image's create or
/// force-update is skipped.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor)
/// * `opts` - The image import options
/// * `ctx` - The shared apply settings (mode, editor, prompting, workers)
/// * `images` - The categorized images to apply
/// * `progress` - The progress bar
/// * `journal` - The journal to record applied changes in
pub async fn apply_images(
    thorium: &Thorium,
    conf: &CtlConf,
    opts: &ImageImportOpts<'_>,
    ctx: &ApplyCtx<'_>,
    images: &[categorize::CategorizedImage],
    progress: &Bar,
    journal: &Journal,
) -> Result<ImagesApplied, Error> {
    // --migrate-registry is a targeted operation: update existing images'
    // urls (or create missing ones) and skip normal conflict handling
    if opts.migrate_registry {
        return Ok(migrate_registries(thorium, opts, ctx, images, progress, journal).await);
    }
    let plan = imports::ImportPlan::new(images, &[], Vec::new());
    // push the new images' containers, then create the images whose push worked
    let (new_images, mut failures) = push_containers(opts, plan.new_images, progress).await;
    let mut missing: HashSet<String> = images
        .iter()
        .filter(|img| img.existing.is_none())
        .filter(|img| !new_images.iter().any(|ok| ok.name == img.name))
        .map(|img| img.request.name.clone())
        .collect();
    let create_failures =
        create::import_new_images(thorium, new_images.clone(), ctx.workers, progress, journal)
            .await;
    missing.extend(failed_names(&new_images, &create_failures));
    failures.extend(create_failures);
    // handle existing images according to the conflict mode
    let outcome = match ctx.mode {
        ConflictMode::Force => {
            // push only the images that will actually be updated, then force-update
            let (changing, unchanged): (Vec<_>, Vec<_>) = plan
                .existing_images
                .into_iter()
                .partition(|img| imports::image_would_change(img));
            let (pushed, push_failures) = push_containers(opts, changing, progress).await;
            failures.extend(push_failures);
            let to_update = pushed.into_iter().chain(unchanged).collect();
            let existing = Box::pin(imports::apply_existing::<ImageKind>(
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
            failures.extend(existing.failures);
            existing.outcome
        }
        ConflictMode::Interactive if ctx.can_prompt => {
            // merge one image at a time so its container is pushed only once the user
            // has chosen to apply an update to it
            let mut outcome = ImportOutcome::Completed;
            for img in plan.existing_images {
                let applied_before = journal.len();
                let existing = Box::pin(imports::apply_existing::<ImageKind>(
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
                failures.extend(existing.failures);
                // the journal grows exactly when an update was applied to this image
                if journal.len() > applied_before {
                    let (_, push_failures) = push_containers(opts, vec![img], progress).await;
                    failures.extend(push_failures);
                }
                // a Quit stops the remaining images
                if existing.outcome == ImportOutcome::Quit {
                    outcome = ImportOutcome::Quit;
                    break;
                }
            }
            outcome
        }
        _ => {
            // skip-conflicts (or no terminal) never updates existing images, so no pushes
            let existing = Box::pin(imports::apply_existing::<ImageKind>(
                thorium,
                conf,
                plan.existing_images,
                ctx.mode,
                ctx.editor,
                ctx.can_prompt,
                ctx.workers,
                progress,
                journal,
            ))
            .await?;
            failures.extend(existing.failures);
            existing.outcome
        }
    };
    Ok(ImagesApplied {
        applied: ApplyOutcome { outcome, failures },
        missing,
    })
}

/// Update existing images' registry urls, creating any that are missing
///
/// `--migrate-registry` is intentionally narrow: on an image that already exists
/// it touches only the stored image url, leaving every other field as-is, and
/// images whose url already matches are left alone. Containers are pushed only
/// for images that are created or migrated, and per-image failures are collected
/// like a normal import.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `opts` - The image import options
/// * `ctx` - The shared apply settings (for the worker count)
/// * `images` - The categorized images to migrate
/// * `progress` - The progress bar
/// * `journal` - The journal to record applied changes in
async fn migrate_registries(
    thorium: &Thorium,
    opts: &ImageImportOpts<'_>,
    ctx: &ApplyCtx<'_>,
    images: &[categorize::CategorizedImage],
    progress: &Bar,
    journal: &Journal,
) -> ImagesApplied {
    // split images into ones to create, ones to migrate, and ones left alone
    let mut to_create = Vec::new();
    let mut to_migrate = Vec::new();
    for img in images {
        match (&img.existing, &img.request.image) {
            // the image doesn't exist yet, so create it like a normal import
            (None, _) => to_create.push(img),
            // the image exists but the incoming config has no url to apply
            (Some(_), None) => progress.info(format!(
                "Image '{}' has no image url to migrate; skipping",
                img.name
            )),
            // the image exists and its url differs, so migrate it
            (Some(_), Some(_)) if imports::image_url_would_change(img) => to_migrate.push(img),
            // the image already has the target url
            (Some(_), Some(_)) => {}
        }
    }
    // push containers for the images that will be touched, then create the new ones
    let (to_create, mut failures) = push_containers(opts, to_create, progress).await;
    let mut missing: HashSet<String> = images
        .iter()
        .filter(|img| img.existing.is_none())
        .filter(|img| !to_create.iter().any(|ok| ok.name == img.name))
        .map(|img| img.request.name.clone())
        .collect();
    let create_failures =
        create::import_new_images(thorium, to_create.clone(), ctx.workers, progress, journal).await;
    missing.extend(failed_names(&to_create, &create_failures));
    failures.extend(create_failures);
    let (to_migrate, push_failures) = push_containers(opts, to_migrate, progress).await;
    failures.extend(push_failures);
    // update the urls concurrently, bounded by the worker count
    if !to_migrate.is_empty() {
        progress.refresh(
            "Migrating image urls",
            BarKind::Bound(to_migrate.len() as u64),
        );
        let results: Vec<(String, Result<(), Error>)> = stream::iter(to_migrate)
            .map(|img| async move {
                let label = create::failure_label(img);
                let result = migrate_one(thorium, img, journal).await;
                progress.inc(1);
                (label, result)
            })
            .buffer_unordered(ctx.workers.max(1))
            .collect()
            .await;
        // warn for and collect each failed migration
        for (label, result) in results {
            if let Err(err) = result {
                progress.warning(format!("Failed to migrate {label}: {err}"));
                failures.push(label);
            }
        }
    }
    ImagesApplied {
        applied: ApplyOutcome {
            outcome: ImportOutcome::Completed,
            failures,
        },
        missing,
    }
}

/// Migrate one existing image's stored url and journal its prior state
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `img` - The categorized image to migrate (must exist and carry a url)
/// * `journal` - The journal to record the update in
async fn migrate_one(
    thorium: &Thorium,
    img: &categorize::CategorizedImage,
    journal: &Journal,
) -> Result<(), Error> {
    // only existing images with a url reach here; bind defensively anyway
    let (Some(existing), Some(url)) = (&img.existing, &img.request.image) else {
        return Ok(());
    };
    // update only the url
    let update = ImageUpdate::default().image(url);
    thorium
        .images
        .update(&img.request.group, &img.request.name, &update)
        .await?;
    // snapshot the prior state so the migration can be rolled back
    journal.updated_image(existing.clone());
    Ok(())
}

/// Unit tests for registry rewriting and container planning
#[cfg(test)]
mod tests {
    use super::*;

    /// Build import options with the given registry flags
    ///
    /// # Arguments
    ///
    /// * `registry` - The `--registry` value
    /// * `registry_override` - The `--registry-override` value
    /// * `skip_push` - The `--skip-push` flag
    fn opts(
        registry: Option<&'static str>,
        registry_override: Option<&'static str>,
        skip_push: bool,
    ) -> ImageImportOpts<'static> {
        ImageImportOpts {
            import_dir: Path::new("/export"),
            group: "g",
            registry,
            registry_override,
            skip_push,
            migrate_registry: false,
            mode: ConflictMode::Interactive,
            editor: None,
            is_terminal: false,
            workers: 1,
        }
    }

    /// Build a K8s image request with the given url
    ///
    /// # Arguments
    ///
    /// * `url` - The image url
    fn request(url: &str) -> ImageRequest {
        let mut req = ImageRequest::new("g", "tool");
        req.scaler = ImageScaler::K8s;
        req.image = Some(url.to_string());
        req
    }

    /// A leading registry host is replaced
    #[test]
    fn rebase_replaces_registry_host() {
        assert_eq!(
            rebase_image_url("old.reg:5000/tools/scan:1.0", "new.reg"),
            "new.reg/tools/scan:1.0"
        );
        assert_eq!(
            rebase_image_url("localhost/scan:1.0", "new.reg"),
            "new.reg/scan:1.0"
        );
        assert_eq!(
            rebase_image_url("docker.io/library/nginx", "new.reg"),
            "new.reg/library/nginx"
        );
    }

    /// Docker Hub style references keep their namespace
    #[test]
    fn rebase_keeps_docker_hub_namespace() {
        assert_eq!(
            rebase_image_url("myorg/scanner:1.2", "reg.local:5000"),
            "reg.local:5000/myorg/scanner:1.2"
        );
        assert_eq!(rebase_image_url("nginx:1", "reg"), "reg/nginx:1");
        assert_eq!(
            rebase_image_url("nginx@sha256:abc", "reg"),
            "reg/nginx@sha256:abc"
        );
    }

    /// Trailing slashes on the registry are ignored and an empty registry is a no-op
    #[test]
    fn rebase_trims_registry() {
        assert_eq!(rebase_image_url("a.b/c:1", "reg.local/"), "reg.local/c:1");
        assert_eq!(rebase_image_url("a.b/c:1", "/"), "a.b/c:1");
    }

    /// A pushed image is retagged onto --registry and stores the pushed url
    #[test]
    fn plan_pushes_to_registry() {
        let plan = plan_container(
            &opts(Some("new.reg"), None, false),
            &request("old.reg/scan:1"),
            PathBuf::from("t.tar.gz"),
            true,
        );
        let push = plan.push.expect("image should be pushed");
        assert_eq!(push.source, "old.reg/scan:1");
        assert_eq!(push.target, "new.reg/scan:1");
        assert_eq!(plan.stored_url.as_deref(), Some("new.reg/scan:1"));
        assert_eq!(plan.registry_skipped, None);
    }

    /// Without a tarball --registry can't apply, which is reported
    #[test]
    fn plan_reports_registry_without_tarball() {
        let plan = plan_container(
            &opts(Some("new.reg"), None, false),
            &request("old.reg/scan:1"),
            PathBuf::from("t.tar.gz"),
            false,
        );
        assert_eq!(plan.push, None);
        assert_eq!(plan.stored_url.as_deref(), Some("old.reg/scan:1"));
        assert!(plan.registry_skipped.is_some());
    }

    /// --skip-push never pushes, reports --registry, and still applies the override
    #[test]
    fn plan_skip_push_applies_override() {
        let plan = plan_container(
            &opts(Some("new.reg"), Some("mirror.reg"), true),
            &request("old.reg/scan:1"),
            PathBuf::from("t.tar.gz"),
            true,
        );
        assert_eq!(plan.push, None);
        assert_eq!(plan.stored_url.as_deref(), Some("mirror.reg/scan:1"));
        assert_eq!(plan.registry_skipped, Some("--skip-push was given"));
    }

    /// Without --registry nothing is reported and the source is pushed back as-is
    #[test]
    fn plan_without_registry_pushes_source() {
        let plan = plan_container(
            &opts(None, None, false),
            &request("old.reg/scan:1"),
            PathBuf::from("t.tar.gz"),
            true,
        );
        let push = plan.push.expect("image should be pushed");
        assert_eq!(push.target, push.source);
        assert_eq!(plan.registry_skipped, None);
    }
}
