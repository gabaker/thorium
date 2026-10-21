//! Resource categorization for imports
//!
//! Checks which incoming images and pipelines already exist in Thorium,
//! categorizing them for downstream handling (create vs update). The inputs
//! are plain request lists so every import flow (toolbox manifests, on-disk
//! export directories) can share this step.

use futures::{StreamExt, TryStreamExt, stream};
use http::StatusCode;
use std::collections::HashSet;
use std::path::Path;
use thorium::models::{ImageRequest, PipelineRequest};
use thorium::{Error, Thorium};
use tokio::sync::OnceCell;

use super::kind::{ImageKind, ImportKind, PipelineKind};
use crate::handlers::progress::{Bar, BarKind};
use crate::utils;

/// Load a request from `<import_dir>/<noun>s/<name>.json` and point it at `group`
///
/// Shared by the `images import` and `pipelines import` on-disk loaders. The
/// file stem is the name every on-disk lookup uses (pipeline orders reference
/// images by it), so a config whose `name` field disagrees with its file name
/// is rejected rather than silently importing under a different name.
///
/// # Arguments
///
/// * `import_dir` - The export directory root
/// * `group` - The group the import targets
/// * `name` - The resource name whose `<name>.json` config to read
pub async fn load_request<K: ImportKind>(
    import_dir: &Path,
    group: &str,
    name: &str,
) -> Result<K::Request, Error> {
    // build the path to this resource's config
    let file_path = import_dir
        .join(format!("{}s", K::NOUN))
        .join(format!("{name}.json"));
    // read the raw config from disk
    let data = tokio::fs::read_to_string(&file_path).await.map_err(|err| {
        Error::new(format!(
            "Failed to read {} config '{}': {err}",
            K::NOUN,
            file_path.display()
        ))
    })?;
    // parse the config into a request
    let mut req: K::Request = serde_json::from_str(&data).map_err(|err| {
        Error::new(format!(
            "Failed to parse {} config '{}': {err}",
            K::NOUN,
            file_path.display()
        ))
    })?;
    // the file stem and the config's own name must agree, or labels, lookups and the
    // resource actually created/updated would refer to different things
    if K::name(&req) != name {
        return Err(Error::new(format!(
            "The {noun} config '{path}' is named '{inner}' but its file name implies '{name}'; \
             rename the file to '{inner}.json' or set its name to '{name}'",
            noun = K::NOUN,
            path = file_path.display(),
            inner = K::name(&req),
        )));
    }
    // imports always land in the group given on the command line
    K::set_group(&mut req, group);
    Ok(req)
}

/// An incoming resource categorized by whether it already exists in Thorium
pub struct Categorized<K: ImportKind> {
    /// The display name for this resource (the manifest name for toolboxes)
    pub name: String,
    /// The manifest version of this resource, or `None` for a versionless
    /// (on-disk export) resource
    pub version: Option<String>,
    /// The incoming request
    pub request: K::Request,
    /// The existing resource in Thorium, if any
    pub existing: Option<K::Existing>,
}

impl<K: ImportKind> Categorized<K> {
    /// Render this resource's identity for display
    ///
    /// Toolbox entries carry a real manifest version and render as
    /// `group/name@version`; versionless resources render as `group/name`.
    #[must_use]
    pub fn label(&self) -> String {
        // the target group always comes from the request the import will send
        check_label::<K>(&self.request, &self.name, self.version.as_deref())
    }
}

/// An incoming image categorized by whether it already exists in Thorium
pub type CategorizedImage = Categorized<ImageKind>;
/// An incoming pipeline categorized by whether it already exists in Thorium
pub type CategorizedPipeline = Categorized<PipelineKind>;

/// Whether a lookup error means "access denied" rather than a real failure
///
/// # Arguments
///
/// * `err` - The error returned by a resource lookup
fn is_access_denied(err: &Error) -> bool {
    err.status()
        .is_some_and(|status| status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN)
}

/// Render an incoming resource's identity for a lookup error
///
/// # Arguments
///
/// * `request` - The incoming request, which carries the target group
/// * `name` - The display name of the resource
/// * `version` - The manifest version, or `None` when versionless
fn check_label<K: ImportKind>(request: &K::Request, name: &str, version: Option<&str>) -> String {
    // toolbox entries carry a real version; on-disk resources are versionless
    match version {
        Some(version) => utils::entry_id(Some(K::group(request)), name, version),
        None => utils::resource_id(K::group(request), name),
    }
}

/// Categorize incoming resources by checking which ones already exist in Thorium
///
/// A resource whose group is missing (or not visible to the current user) is
/// categorized as new: non-admins get an access error rather than a not-found
/// when looking up anything in a group they aren't a member of, which is always
/// the case for a group the import is about to create. An access error for a
/// group the user *can* see is still a hard error.
///
/// Results keep the input order so the confirmation listing and prompts are
/// stable between runs.
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to look up existing resources
/// * `items` - The incoming resources as (name, version, request) tuples (version is `None` when versionless)
/// * `progress` - The progress bar to increment as each resource is checked
pub async fn categorize<K: ImportKind>(
    thorium: &Thorium,
    items: Vec<(String, Option<String>, K::Request)>,
    progress: &Bar,
) -> Result<Vec<Categorized<K>>, Error> {
    // reset the bar to track this pass
    progress.refresh(
        format!("Checking existing {}s", K::NOUN),
        BarKind::Bound(items.len() as u64),
    );
    // the visible-group listing is only needed if a lookup is denied, so fetch it lazily
    // and at most once for the whole pass
    let visible_groups: OnceCell<HashSet<String>> = OnceCell::new();
    let visible_groups = &visible_groups;
    stream::iter(items)
        .map(|(name, version, request)| async move {
            // look up the resource, mapping not-found (and denied lookups in groups the
            // user can't see) to "new"
            let existing = match K::get(thorium, K::group(&request), K::name(&request)).await {
                Ok(resource) => Some(resource),
                // a not-found resource is simply new, not an error
                Err(err)
                    if err
                        .status()
                        .is_some_and(|status| status == StatusCode::NOT_FOUND) =>
                {
                    None
                }
                // a denied lookup in a group the user can't see means the group is missing
                // (or inaccessible) for this user; the import creates it or fails there
                Err(err) if is_access_denied(&err) => {
                    let visible = visible_groups
                        .get_or_try_init(|| super::list_visible_groups(thorium))
                        .await?;
                    if visible.contains(K::group(&request)) {
                        return Err(Error::new(format!(
                            "Failed to check {} '{}': {err}",
                            K::NOUN,
                            check_label::<K>(&request, &name, version.as_deref())
                        )));
                    }
                    None
                }
                Err(err) => {
                    return Err(Error::new(format!(
                        "Failed to check {} '{}': {err}",
                        K::NOUN,
                        check_label::<K>(&request, &name, version.as_deref())
                    )));
                }
            };
            progress.inc(1);
            Ok(Categorized {
                name,
                version,
                request,
                existing,
            })
        })
        // bounded concurrency that keeps the input order; try_collect short-circuits on
        // the first error. Bounded (not wired to --workers) on purpose: this read-only
        // existence check is shared by import, diff, and remove, and --workers is meant
        // for the apply (write) phase.
        .buffered(10)
        .try_collect()
        .await
}

/// Categorize incoming images by checking which ones already exist in Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to look up existing images
/// * `items` - The incoming images as (name, version, request) tuples (version is `None` when versionless)
/// * `progress` - The progress bar to increment as each image is checked
pub async fn categorize_images(
    thorium: &Thorium,
    items: Vec<(String, Option<String>, ImageRequest)>,
    progress: &Bar,
) -> Result<Vec<CategorizedImage>, Error> {
    categorize::<ImageKind>(thorium, items, progress).await
}

/// Categorize incoming pipelines by checking which ones already exist in Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to look up existing pipelines
/// * `items` - The incoming pipelines as (name, version, request) tuples (version is `None` when versionless)
/// * `progress` - The progress bar to increment as each pipeline is checked
pub async fn categorize_pipelines(
    thorium: &Thorium,
    items: Vec<(String, Option<String>, PipelineRequest)>,
    progress: &Bar,
) -> Result<Vec<CategorizedPipeline>, Error> {
    categorize::<PipelineKind>(thorium, items, progress).await
}
