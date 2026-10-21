//! Pipeline import support for thorctl
//!
//! Loads pipeline configs from an on-disk export directory and applies them
//! through the shared conflict engine. The orchestration (image pass first,
//! then pipelines, with one rollback journal spanning both) lives in the shared
//! on-disk import driver (`imports::disk`).

use std::path::Path;
use thorium::models::PipelineRequest;
use thorium::{CtlConf, Error, Thorium};

use crate::handlers::imports::kind::PipelineKind;
use crate::handlers::imports::rollback::Journal;
use crate::handlers::imports::{self, ApplyCtx, ApplyOutcome, categorize, create};
use crate::handlers::progress::Bar;

/// Load a pipeline request from the export directory and point it at our group
///
/// # Arguments
///
/// * `import_dir` - The export directory holding `pipelines/<name>.json`
/// * `group` - The group to import the pipeline into
/// * `name` - The name of the pipeline whose config we are loading
pub async fn load_request(
    import_dir: &Path,
    group: &str,
    name: &str,
) -> Result<PipelineRequest, Error> {
    categorize::load_request::<PipelineKind>(import_dir, group, name).await
}

/// Apply the categorized pipelines to Thorium according to the conflict mode
///
/// New pipelines are created and existing ones go through the shared conflict
/// dispatch (`imports::apply_existing`), exactly like the image pass.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor)
/// * `new` - The pipelines that don't exist in Thorium yet
/// * `existing` - The pipelines that already exist in Thorium
/// * `ctx` - The shared apply settings (mode, editor, prompting, workers)
/// * `progress` - The progress bar
/// * `journal` - The journal to record applied changes in
pub async fn apply_pipelines(
    thorium: &Thorium,
    conf: &CtlConf,
    new: Vec<&categorize::CategorizedPipeline>,
    existing: Vec<&categorize::CategorizedPipeline>,
    ctx: &ApplyCtx<'_>,
    progress: &Bar,
    journal: &Journal,
) -> Result<ApplyOutcome, Error> {
    // create the pipelines that don't exist yet, collecting per-pipeline failures
    let mut failures =
        create::import_new_pipelines(thorium, new, ctx.workers, progress, journal).await;
    // handle existing pipelines according to the conflict mode (shared dispatch)
    let existing = imports::apply_existing::<PipelineKind>(
        thorium,
        conf,
        existing,
        ctx.mode,
        ctx.editor,
        ctx.can_prompt,
        ctx.workers,
        progress,
        journal,
    )
    .await?;
    failures.extend(existing.failures);
    Ok(ApplyOutcome {
        outcome: existing.outcome,
        failures,
    })
}
