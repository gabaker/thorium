//! The shared driver for on-disk imports (`images import`, `pipelines import`)
//!
//! Both commands share the same skeleton — categorize, confirm, create the
//! target group, apply images then pipelines under one rollback journal, and
//! settle. The only difference is whether there are pipelines to apply, so
//! `images import` simply passes an empty pipeline list. The toolbox import has
//! extra pre-apply steps (policies, bundled images, collisions) and drives its
//! own flow.

use std::collections::HashSet;
use thorium::{CtlConf, Error, Thorium};

use super::categorize::{CategorizedImage, CategorizedPipeline};
use super::rollback::Journal;
use super::{ApplyCtx, ApplyOutcome, ConflictMode, ImportOutcome, ImportPlan, PlanLabels};
use crate::handlers::images::import::{ImageImportOpts, apply_images};
use crate::handlers::pipelines::import::apply_pipelines;
use crate::handlers::progress::{Bar, BarKind};

/// Reject pipelines whose order is malformed, empty, or has an empty stage
///
/// The server would reject these too, but only after containers were pushed and
/// other resources applied, so they are caught before anything changes.
///
/// # Arguments
///
/// * `pipelines` - The categorized pipelines to validate
fn validate_pipeline_orders(pipelines: &[CategorizedPipeline]) -> Result<(), Error> {
    for pipe in pipelines {
        // parse the order into stages of image names
        let order = pipe.request.deserialize_image_order().map_err(|err| {
            Error::new(format!(
                "Malformed image order in pipeline '{}': {err}",
                pipe.name
            ))
        })?;
        // a pipeline needs at least one stage
        if order.is_empty() {
            return Err(Error::new(format!(
                "Pipeline '{}' has an empty order; it must run at least one image",
                pipe.name
            )));
        }
        // and every stage needs at least one image
        if let Some(stage) = order.iter().position(Vec::is_empty) {
            return Err(Error::new(format!(
                "Pipeline '{}' has an empty stage (stage {}); every stage must run at least one image",
                pipe.name,
                stage + 1
            )));
        }
    }
    Ok(())
}

/// Drop pipelines that reference images which failed to import
///
/// Each dropped pipeline is warned about and returned as a failure label, so it
/// is reported once with a clear reason instead of failing server-side with a
/// less obvious "image does not exist" error.
///
/// # Arguments
///
/// * `pipelines` - The pipelines about to be applied
/// * `missing` - The names of images that failed to import
/// * `progress` - The progress bar to warn through
fn drop_dependent_pipelines<'a>(
    pipelines: Vec<&'a CategorizedPipeline>,
    missing: &HashSet<String>,
    progress: &Bar,
) -> (Vec<&'a CategorizedPipeline>, Vec<String>) {
    // nothing failed, so every pipeline can be applied
    if missing.is_empty() {
        return (pipelines, Vec::new());
    }
    let mut kept = Vec::with_capacity(pipelines.len());
    let mut failures = Vec::new();
    for pipe in pipelines {
        // collect the referenced images that failed (orders were validated up front)
        let mut failed: Vec<&str> = pipe
            .request
            .deserialize_image_order()
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter(|image| missing.contains(*image))
            .collect();
        failed.sort_unstable();
        failed.dedup();
        if failed.is_empty() {
            kept.push(pipe);
        } else {
            progress.warning(format!(
                "Skipping pipeline '{}': it references image(s) that failed to import: {}",
                pipe.name,
                failed.join(", ")
            ));
            failures.push(super::create::failure_label(pipe));
        }
    }
    (kept, failures)
}

/// Confirm the import plan with the user, or announce group creation when nobody
/// can confirm; returns `false` if the user declined
///
/// Like `toolbox import`, an interactive session is asked to confirm before any
/// existing resource would change or any group would be created (groups are
/// access boundaries); sessions that can't prompt announce the group creation.
///
/// # Arguments
///
/// * `thorium` - The Thorium client (for the username shown in the prompt)
/// * `conf` - The Thorctl config (for the API url shown in the prompt)
/// * `progress` - The progress bar
/// * `opts` - The image import options
/// * `plan` - The categorized resources this import will act on
/// * `can_prompt` - Whether the session can prompt
async fn confirm_or_announce(
    thorium: &Thorium,
    conf: &CtlConf,
    progress: &Bar,
    opts: &ImageImportOpts<'_>,
    plan: &ImportPlan<'_>,
    can_prompt: bool,
) -> Result<bool, Error> {
    // --migrate-registry only changes existing images' urls, so label and gate on that
    let (labels, has_conflicts) = if opts.migrate_registry {
        let migrating = plan
            .existing_images
            .iter()
            .any(|img| super::image_url_would_change(img));
        (PlanLabels::MigrateRegistry, migrating)
    } else {
        (PlanLabels::Conflicts(opts.mode), plan.has_conflicts())
    };
    // confirm before changing existing resources or creating groups
    if can_prompt && (has_conflicts || !plan.missing_groups.is_empty()) {
        let username = super::current_username(thorium, progress).await;
        return progress.suspend(|| super::confirm_plan(conf, plan, &username, labels));
    }
    // nobody can confirm, so at least make the group creation visible
    if !plan.missing_groups.is_empty() {
        progress.info(format!(
            "Group '{}' does not exist and will be created",
            plan.missing_groups.join("', '")
        ));
    }
    Ok(true)
}

/// Apply the groups, images, and pipelines of a confirmed plan
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor)
/// * `opts` - The image import options
/// * `ctx` - The shared apply settings
/// * `images` - The categorized images to apply
/// * `plan` - The plan built from the categorized images and pipelines
/// * `progress` - The progress bar
/// * `journal` - The journal to record applied changes in
#[allow(clippy::too_many_arguments)]
async fn apply_plan(
    thorium: &Thorium,
    conf: &CtlConf,
    opts: &ImageImportOpts<'_>,
    ctx: &ApplyCtx<'_>,
    images: &[CategorizedImage],
    plan: &ImportPlan<'_>,
    progress: &Bar,
    journal: &Journal,
) -> Result<ApplyOutcome, Error> {
    // create the target group if needed
    if !plan.missing_groups.is_empty() {
        progress.refresh(
            "Creating groups",
            BarKind::Bound(plan.missing_groups.len() as u64),
        );
        super::create_groups(
            thorium,
            plan.missing_groups.clone(),
            opts.workers,
            progress,
            journal,
        )
        .await?;
    }
    // import the images first since the pipelines reference them; a Quit
    // in the image pass stops the pipeline pass too
    let images_applied = Box::pin(apply_images(
        thorium, conf, opts, ctx, images, progress, journal,
    ))
    .await?;
    let mut failures = images_applied.applied.failures;
    if images_applied.applied.outcome == ImportOutcome::Quit {
        return Ok(ApplyOutcome {
            outcome: ImportOutcome::Quit,
            failures,
        });
    }
    // --migrate-registry only creates missing pipelines and leaves existing ones alone
    let existing_pipelines = if opts.migrate_registry {
        if !plan.existing_pipelines.is_empty() {
            progress.info(format!(
                "Leaving {} existing pipeline(s) unchanged (--migrate-registry only migrates image urls)",
                plan.existing_pipelines.len()
            ));
        }
        Vec::new()
    } else {
        plan.existing_pipelines.clone()
    };
    // skip pipelines whose images failed to import, with a clear reason
    let (new_pipelines, new_skipped) = drop_dependent_pipelines(
        plan.new_pipelines.clone(),
        &images_applied.missing,
        progress,
    );
    let (existing_pipelines, existing_skipped) =
        drop_dependent_pipelines(existing_pipelines, &images_applied.missing, progress);
    failures.extend(new_skipped);
    failures.extend(existing_skipped);
    // a no-op when there are no pipelines (the `images import` case)
    let pipelines_applied = apply_pipelines(
        thorium,
        conf,
        new_pipelines,
        existing_pipelines,
        ctx,
        progress,
        journal,
    )
    .await?;
    // carry forward both passes' collected failures
    failures.extend(pipelines_applied.failures);
    Ok(ApplyOutcome {
        outcome: pipelines_applied.outcome,
        failures,
    })
}

/// Run an on-disk import of already-categorized images and (optionally) pipelines
///
/// Images are applied before the pipelines that reference them, under one journal,
/// so a Quit or error partway through can offer to undo everything. Owns the
/// whole flow from the confirmation gate through settling the journal.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor and confirmation)
/// * `progress` - The progress bar
/// * `opts` - The image import options (carries mode/editor/group/registry/etc.)
/// * `images` - The categorized images to apply (container work happens in `apply_images`)
/// * `pipelines` - The categorized pipelines to apply (empty for `images import`)
/// * `rollback_on_failure` - Auto-roll-back a partial import when we can't prompt
pub async fn run_disk_import(
    thorium: &Thorium,
    conf: &CtlConf,
    progress: &Bar,
    opts: &ImageImportOpts<'_>,
    images: Vec<CategorizedImage>,
    pipelines: Vec<CategorizedPipeline>,
    rollback_on_failure: bool,
) -> Result<(), Error> {
    // reject broken pipeline orders before anything is changed
    validate_pipeline_orders(&pipelines)?;
    // the target group is auto-created if it doesn't exist yet
    let missing_groups =
        super::get_missing_groups(thorium, HashSet::from([opts.group.to_string()])).await?;
    let plan = ImportPlan::new(&images, &pipelines, missing_groups);
    // we can prompt only in the interactive default with a terminal; this gates the
    // plan confirmation, the merge editor, and the rollback offer
    let can_prompt = opts.mode == ConflictMode::Interactive
        && opts.is_terminal
        && super::is_interactive_terminal();
    // confirm (or announce) before changing anything
    if !confirm_or_announce(thorium, conf, progress, opts, &plan, can_prompt).await? {
        progress.finish_with_message("Import cancelled; nothing was changed");
        return Ok(());
    }
    // the settings shared by the image and pipeline passes
    let ctx = ApplyCtx {
        mode: opts.mode,
        editor: opts.editor,
        can_prompt,
        workers: opts.workers,
    };
    // journal every applied change so a partial import can be rolled back
    let journal = Journal::new();
    let result = Box::pin(apply_plan(
        thorium, conf, opts, &ctx, &images, &plan, progress, &journal,
    ))
    .await;
    // per-resource failures are kept (not rolled back) and reported below; settle the
    // journal only on the outcome/error so rollback still covers a Quit or fatal error
    let (settle_input, failures) = match result {
        Ok(applied) => (Ok(applied.outcome), applied.failures),
        Err(err) => (Err(err), Vec::new()),
    };
    // rollback can be offered only when we could prompt (interactive mode + terminal)
    let outcome = super::settle_journal(
        thorium,
        progress,
        journal,
        settle_input,
        can_prompt,
        rollback_on_failure,
    )
    .await?;
    // pick the final banner and exit non-zero if any resource failed
    super::finish_import(progress, outcome, &failures)
}

/// Unit tests for the pure pipeline checks in the on-disk driver
#[cfg(test)]
mod tests {
    use super::*;
    use thorium::models::PipelineRequest;

    /// Build a categorized (new) pipeline with the given order
    ///
    /// # Arguments
    ///
    /// * `name` - The pipeline name
    /// * `order` - The pipeline's image order
    fn pipeline(name: &str, order: serde_json::Value) -> CategorizedPipeline {
        CategorizedPipeline {
            name: name.to_string(),
            version: None,
            request: PipelineRequest::new("g", name, order),
            existing: None,
        }
    }

    /// Empty orders and empty stages are rejected; normal orders pass
    #[test]
    fn validates_orders() {
        assert!(
            validate_pipeline_orders(&[pipeline("ok", serde_json::json!([["a"], "b"]))]).is_ok()
        );
        assert!(validate_pipeline_orders(&[pipeline("empty", serde_json::json!([]))]).is_err());
        assert!(
            validate_pipeline_orders(&[pipeline("stage", serde_json::json!([["a"], []]))]).is_err()
        );
    }

    /// Pipelines referencing a failed image are dropped with a failure label
    #[test]
    fn drops_dependent_pipelines() {
        let ok = pipeline("ok", serde_json::json!([["a"]]));
        let bad = pipeline("bad", serde_json::json!([["a"], ["x"]]));
        let missing = HashSet::from(["x".to_string()]);
        let progress = Bar::new_unbounded("test", "");
        let (kept, failures) = drop_dependent_pipelines(vec![&ok, &bad], &missing, &progress);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "ok");
        assert_eq!(failures, vec!["pipeline 'g/bad'".to_string()]);
    }
}
