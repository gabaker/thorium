//! Resource creation and force-update operations
//!
//! Handles importing new images/pipelines and force-updating existing ones
//! without interactive merge resolution. Both passes are generic over the
//! resource [`ImportKind`]; the per-kind wrappers exist only to keep call sites
//! reading naturally.

use futures::{StreamExt, stream};
use thorium::{Error, Thorium};

use super::categorize::{Categorized, CategorizedImage, CategorizedPipeline};
use super::kind::{ImageKind, ImportKind, PipelineKind};
use super::rollback::Journal;
use crate::handlers::progress::{Bar, BarKind};

/// Import new resources (ones that don't already exist in Thorium)
///
/// Per-resource failures are collected and skipped rather than aborting the rest of the
/// import, so one bad resource doesn't block the others; each failure is warned and its
/// label returned for the caller's end-of-import summary. Successful creations are kept.
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to create resources
/// * `new` - Resources from the manifest that do not yet exist in Thorium
/// * `workers` - Max concurrent creates (the global `--workers`)
/// * `progress` - The progress bar to increment as resources are created
/// * `journal` - The journal to record created resources in for rollback
pub async fn import_new<K: ImportKind>(
    thorium: &Thorium,
    new: Vec<&Categorized<K>>,
    workers: usize,
    progress: &Bar,
    journal: &Journal,
) -> Vec<String> {
    // nothing to create
    if new.is_empty() {
        return Vec::new();
    }
    // reset the bar to track the creates
    progress.refresh(
        format!("Importing new {}s", K::NOUN),
        BarKind::Bound(new.len() as u64),
    );
    // create each new resource concurrently, bounded by the worker count, pairing each
    // result with a label so failures can be collected without aborting the rest
    let results: Vec<(String, Result<(), Error>)> = stream::iter(new)
        .map(|item| {
            let label = failure_label(item);
            async move {
                let result = create_resource(thorium, item, progress, journal).await;
                (label, result)
            }
        })
        .buffer_unordered(workers.max(1))
        .collect()
        .await;
    collect_failures(results, progress)
}

/// The label identifying a resource in failure lists and summaries
///
/// # Arguments
///
/// * `item` - The categorized resource to label
pub fn failure_label<K: ImportKind>(item: &Categorized<K>) -> String {
    format!("{} '{}'", K::NOUN, item.label())
}

/// Warn for and collect the labels of every failed create/update from a batch
///
/// The success path increments the progress bar inside `create_resource`/
/// `update_resource`; a failure didn't, so the bar is advanced here to stay accurate.
///
/// # Arguments
///
/// * `results` - The `(label, result)` pairs from a batch of creates or updates
/// * `progress` - The progress bar to warn through and advance for failures
fn collect_failures(results: Vec<(String, Result<(), Error>)>, progress: &Bar) -> Vec<String> {
    let mut failures = Vec::new();
    for (label, result) in results {
        // successes were already counted; warn, count, and collect each failure
        if let Err(err) = result {
            progress.warning(format!("{err}"));
            progress.inc(1);
            failures.push(label);
        }
    }
    failures
}

/// Force-update all existing resources without the editor
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to apply updates
/// * `existing` - Resources from the manifest that already exist in Thorium
/// * `workers` - Max concurrent updates (the global `--workers`)
/// * `progress` - The progress bar to increment as updates are applied
/// * `journal` - The journal to snapshot pre-update state in for rollback
pub async fn force_update<K: ImportKind>(
    thorium: &Thorium,
    existing: Vec<&Categorized<K>>,
    workers: usize,
    progress: &Bar,
    journal: &Journal,
) -> Vec<String> {
    // compute the update for each existing resource, dropping the no-op ones
    let updates: Vec<_> = existing.into_iter().filter_map(prepare_update).collect();
    // nothing to update
    if updates.is_empty() {
        return Vec::new();
    }
    // reset the bar to track the updates
    progress.refresh(
        format!("Force-updating {}s", K::NOUN),
        BarKind::Bound(updates.len() as u64),
    );
    // apply each update concurrently, bounded by the worker count, collecting failures
    // so one bad update doesn't abort the rest
    let results: Vec<(String, Result<(), Error>)> = stream::iter(updates)
        .map(|(item, update_req)| {
            let label = failure_label(item);
            async move {
                let result = update_resource(thorium, item, update_req, progress, journal).await;
                (label, result)
            }
        })
        .buffer_unordered(workers.max(1))
        .collect()
        .await;
    collect_failures(results, progress)
}

/// Import new images (see [`import_new`])
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to create images
/// * `new_images` - Images that do not yet exist in Thorium
/// * `workers` - Max concurrent creates (the global `--workers`)
/// * `progress` - The progress bar to increment as images are created
/// * `journal` - The journal to record created images in for rollback
pub async fn import_new_images(
    thorium: &Thorium,
    new_images: Vec<&CategorizedImage>,
    workers: usize,
    progress: &Bar,
    journal: &Journal,
) -> Vec<String> {
    import_new::<ImageKind>(thorium, new_images, workers, progress, journal).await
}

/// Import new pipelines (see [`import_new`])
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to create pipelines
/// * `new_pipelines` - Pipelines that do not yet exist in Thorium
/// * `workers` - Max concurrent creates (the global `--workers`)
/// * `progress` - The progress bar to increment as pipelines are created
/// * `journal` - The journal to record created pipelines in for rollback
pub async fn import_new_pipelines(
    thorium: &Thorium,
    new_pipelines: Vec<&CategorizedPipeline>,
    workers: usize,
    progress: &Bar,
    journal: &Journal,
) -> Vec<String> {
    import_new::<PipelineKind>(thorium, new_pipelines, workers, progress, journal).await
}

/// Build a contextual error for a failed import or force-update of a resource
///
/// # Arguments
///
/// * `action` - The verb describing the failed operation (e.g. "import")
/// * `item` - The resource the operation was acting on
/// * `err` - The underlying error returned by Thorium
fn operation_error<K: ImportKind>(action: &str, item: &Categorized<K>, err: &Error) -> Error {
    Error::new(format!(
        "Failed to {action} {} '{}': {}",
        K::NOUN,
        item.label(),
        err
    ))
}

/// Compute the update needed for a single existing resource, or `None` to skip it
///
/// New resources and resources whose computed update is empty are skipped; the
/// update calculation is the same change predicate every import path uses.
///
/// # Arguments
///
/// * `item` - The categorized resource paired with its existing Thorium state
fn prepare_update<K: ImportKind>(item: &Categorized<K>) -> Option<(&Categorized<K>, K::Update)> {
    // skip resources that don't exist yet (those are handled by the import-new pass)
    let current = item.existing.as_ref()?;
    // the computed update is the single change predicate: no update means unchanged
    K::calculate_update(current.clone(), item.request.clone()).map(|update_req| (item, update_req))
}

/// Create a single new resource in Thorium and record it for rollback
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to create the resource
/// * `item` - The categorized resource to create
/// * `progress` - The progress bar to increment once the resource is created
/// * `journal` - The journal to record the creation in for rollback
async fn create_resource<K: ImportKind>(
    thorium: &Thorium,
    item: &Categorized<K>,
    progress: &Bar,
    journal: &Journal,
) -> Result<(), Error> {
    // create the resource, annotating any failure with its name and version
    K::create(thorium, &item.request)
        .await
        .map_err(|err| operation_error("import", item, &err))?;
    // record the creation so it can be rolled back on a later failure
    K::record_created(journal, K::group(&item.request), K::name(&item.request));
    progress.inc(1);
    Ok(())
}

/// Apply a single force-update to an existing resource and snapshot its prior state
///
/// # Arguments
///
/// * `thorium` - The Thorium client used to apply the update
/// * `item` - The categorized resource being updated
/// * `update_req` - The update payload to apply
/// * `progress` - The progress bar to increment once the update is applied
/// * `journal` - The journal to snapshot the pre-update state in for rollback
async fn update_resource<K: ImportKind>(
    thorium: &Thorium,
    item: &Categorized<K>,
    update_req: K::Update,
    progress: &Bar,
    journal: &Journal,
) -> Result<(), Error> {
    // apply the update, annotating any failure with its name and version
    K::update(
        thorium,
        K::group(&item.request),
        K::name(&item.request),
        &update_req,
    )
    .await
    .map_err(|err| operation_error("force-update", item, &err))?;
    // snapshot the pre-update state so the update can be reverted
    if let Some(original) = item.existing.as_ref() {
        K::record_updated(journal, original.clone());
    }
    progress.inc(1);
    Ok(())
}
