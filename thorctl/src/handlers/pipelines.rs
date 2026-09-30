use itertools::Itertools;
use thorium::CtlConf;
use thorium::{Error, Thorium, models::Pipeline};

use crate::args::pipelines::{DescribePipelines, GetPipelines, Pipelines};
use crate::args::{Args, DescribeCommand};
use crate::utils;

mod bans;
mod edit;
mod notifications;

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "macos"))] {
        use std::collections::{BTreeMap, BTreeSet};

        use futures::stream::{self, StreamExt};
        use thorium::models::PipelineRequest;

        use crate::args::pipelines::{ExportPipelines, ImportPipelines};
        use crate::handlers::imports::{
            self, ConflictMode, editor::resolve_editor, merge::PIPELINE_FIELD_ORDER,
        };
        use crate::handlers::images::{ImageExportOpts, export_images_with};
        use crate::handlers::images::import::{ImageImportOpts, categorize_from_disk};
        use crate::handlers::exports::{
            self, ConfigExport, DiskConflictResolver, ExportReport, ExportedConfig,
        };
        use crate::handlers::progress::{Bar, BarKind};

        pub(crate) mod import;
    }
}

/// Prints the rows of the `pipelines get` table
struct GetPipelinesLine;

impl GetPipelinesLine {
    /// Print the table header
    pub fn header() {
        println!(
            "{:<30} | {:<20} | {:<50}",
            "PIPELINE NAME", "GROUP", "DESCRIPTION",
        );
        println!("{:-<31}+{:-<22}+{:-<50}", "", "", "");
    }

    /// Print a pipeline's info
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The pipeline to print
    pub fn print_pipeline(pipeline: &Pipeline) {
        // limit our description preview to at most 40 characters so the column stays aligned
        let description = utils::render::truncate_description(pipeline.description.as_deref(), 40);
        // print our pipeline info
        println!(
            "{:<30} | {:<20} | {}",
            pipeline.name, pipeline.group, description
        );
    }
}

/// Get pipeline info from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The pipeline get command to execute
async fn get(thorium: Thorium, cmd: &GetPipelines) -> Result<(), Error> {
    GetPipelinesLine::header();
    // get the current user's groups if no groups were specified
    let groups = if cmd.groups.is_empty() {
        utils::groups::get_all_groups(&thorium).await?
    } else {
        cmd.groups.clone()
    };
    // get pipeline cursors for all groups specified
    let pipeline_cursors = groups.iter().map(|group| {
        let cursor = thorium
            .pipelines
            .list(group)
            .page_size(cmd.page_size as u64)
            .details();
        // only apply a limit if the user didn't request no limit
        if cmd.no_limit {
            cursor
        } else {
            cursor.limit(cmd.limit as u64)
        }
    });
    // retrieve the pipelines in each cursor until we've reached our limit
    // or all cursors are exhausted
    let mut pipelines: Vec<Pipeline> = Vec::new();
    for mut cursor in pipeline_cursors {
        while !cursor.exhausted {
            cursor.next().await?;
            if cmd.alpha {
                // save for later if we need to alphabetize
                pipelines.append(&mut cursor.details);
            } else {
                // print immediately if no need to alphabetize
                cursor
                    .details
                    .iter()
                    .for_each(GetPipelinesLine::print_pipeline);
            }
        }
    }
    // sort and print in alphabetical order if alpha flag was set
    if cmd.alpha {
        pipelines
            .iter()
            .sorted_unstable_by(|a, b| Ord::cmp(&a.name, &b.name))
            .for_each(GetPipelinesLine::print_pipeline);
    }
    Ok(())
}

/// Describe pipelines by displaying/saving all of their JSON-formatted details
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe pipeline command to execute
async fn describe(thorium: Thorium, cmd: &DescribePipelines) -> Result<(), Error> {
    cmd.describe(&thorium).await
}

/// Delete pipelines from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete pipelines command to execute
async fn delete(
    thorium: Thorium,
    cmd: &crate::args::pipelines::DeletePipelines,
) -> Result<(), Error> {
    use colored::Colorize;
    // repeated names would only fail as not-found on their second delete
    let names: Vec<&String> = cmd.pipelines.iter().unique().collect();
    // deleting is irreversible, so confirm exactly what will be removed
    if !cmd.skip_confirm {
        // fail clearly (not with a raw dialoguer error) when we can't prompt
        utils::require_confirm_terminal("--skip-confirm")?;
        println!("{}", "Pipelines to delete:".bright_red());
        for name in &names {
            println!("  {}", utils::resource_id(&cmd.group, name));
        }
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Delete the pipelines listed above?")
            .default(false)
            .interact()?;
        if !confirmed {
            return Ok(());
        }
    }
    // delete each pipeline, continuing past failures so one bad pipeline doesn't strand the rest
    let mut failed: Vec<String> = Vec::new();
    for name in names {
        match thorium.pipelines.delete(&cmd.group, name).await {
            Ok(_) => println!(
                "Deleted pipeline '{}'",
                utils::resource_id(&cmd.group, name)
            ),
            // a missing pipeline isn't fatal to the rest of the batch
            Err(err) if err.status() == Some(http::StatusCode::NOT_FOUND) => {
                eprintln!(
                    "{}: pipeline '{}' not found; skipping",
                    "Warning".bright_yellow(),
                    utils::resource_id(&cmd.group, name)
                );
            }
            Err(err) => {
                eprintln!(
                    "{}: Failed to delete pipeline '{}': {err}",
                    "Error".bright_red(),
                    utils::resource_id(&cmd.group, name)
                );
                failed.push(format!("'{}'", utils::resource_id(&cmd.group, name)));
            }
        }
    }
    // exit non-zero if anything failed to delete
    if !failed.is_empty() {
        return Err(Error::new(format!(
            "Failed to delete {} pipeline(s): {}",
            failed.len(),
            failed.join(", ")
        )));
    }
    Ok(())
}

/// Keep only the referenced images that have a config in the export directory
///
/// An image with no on-disk config is fine as long as it already exists in the
/// target group (the pipeline will use the existing image); otherwise the import
/// fails, naming the pipelines that reference it.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The import pipelines command being executed
/// * `image_refs` - Each referenced image mapped to the pipelines that reference it
/// * `progress` - The progress bar to log through
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn images_to_import(
    thorium: &Thorium,
    cmd: &ImportPipelines,
    image_refs: BTreeMap<String, BTreeSet<String>>,
    progress: &Bar,
) -> Result<Vec<String>, Error> {
    let images_dir = cmd.import.join("images");
    let mut on_disk = Vec::with_capacity(image_refs.len());
    for (image, pipelines) in image_refs {
        // images with a config in the export are imported normally
        let path = images_dir.join(format!("{image}.json"));
        let exists = tokio::fs::try_exists(&path)
            .await
            .map_err(|err| Error::new(format!("Failed to stat '{}': {err}", path.display())))?;
        if exists {
            on_disk.push(image);
            continue;
        }
        // otherwise the image has to already exist in the target group
        match thorium.images.get(&cmd.group, &image).await {
            Ok(_) => progress.info(format!(
                "Image '{image}' has no config in '{}'; using the existing image in group '{}'",
                images_dir.display(),
                cmd.group
            )),
            Err(err) if err.status() == Some(http::StatusCode::NOT_FOUND) => {
                return Err(Error::new(format!(
                    "Pipeline(s) {} reference image '{image}', which is neither in '{}' nor in group '{}'",
                    pipelines.iter().map(|name| format!("'{name}'")).join(", "),
                    images_dir.display(),
                    cmd.group
                )));
            }
            Err(err) => {
                return Err(Error::new(format!(
                    "Failed to get image '{}': {err}",
                    utils::resource_id(&cmd.group, &image)
                )));
            }
        }
    }
    Ok(on_disk)
}

/// Import pipelines and the images they reference to Thorium
///
/// The whole import — images first, then pipelines — shares one rollback journal,
/// so stopping partway (editor Quit or an error) can offer to undo everything
/// applied so far. The user is asked to confirm once, up front, when existing
/// resources conflict or groups will be created. Referenced images without a config
/// in the export are allowed when they already exist in the target group.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The import pipelines command to execute
/// * `conf` - The Thorctl config
/// * `workers` - The maximum number of concurrent workers to use
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn import(
    thorium: &Thorium,
    cmd: &ImportPipelines,
    conf: &CtlConf,
    workers: usize,
) -> Result<(), Error> {
    let progress = Bar::new("pipelines import", "Importing pipelines", BarKind::Timer);
    let mode = ConflictMode::from_flags(cmd.overwrite, cmd.skip_conflicts);
    // no explicit list imports every pipeline config in the export directory
    let pipeline_names = if cmd.pipelines.is_empty() {
        imports::list_export_configs(&cmd.import, "pipelines").await?
    } else {
        imports::dedup_names(cmd.pipelines.clone(), &progress)
    };
    // load each pipeline request once, collecting the images it references as we go;
    // ordered maps keep processing and prompts in a stable order between runs
    let mut pipeline_items = Vec::with_capacity(pipeline_names.len());
    let mut image_refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in &pipeline_names {
        let request = import::load_request(&cmd.import, &cmd.group, name).await?;
        // collect every image across all stages; a malformed order contributes no images
        // here and is rejected by the shared driver before anything is applied
        let order = request.deserialize_image_order().unwrap_or_default();
        for image in order.into_iter().flatten() {
            image_refs
                .entry(image.to_owned())
                .or_default()
                .insert(name.clone());
        }
        pipeline_items.push((name.clone(), None, request));
    }
    // keep the images that have configs on disk; the rest must already exist
    let image_names = images_to_import(thorium, cmd, image_refs, &progress).await?;
    // build the image import options from the pipeline import's flags
    let opts = ImageImportOpts {
        import_dir: &cmd.import,
        group: &cmd.group,
        registry: cmd.registry.as_deref(),
        registry_override: cmd.registry_override.as_deref(),
        skip_push: cmd.skip_push,
        migrate_registry: cmd.migrate_registry,
        mode,
        editor: cmd.editor.as_deref(),
        // the driver derives interactive-mode + TTY from this
        is_terminal: imports::is_interactive_terminal(),
        workers,
    };
    // categorize both halves before changing anything
    let images = categorize_from_disk(thorium, &opts, &image_names, &progress).await?;
    let pipelines =
        imports::categorize::categorize_pipelines(thorium, pipeline_items, &progress).await?;
    // images are applied before the pipelines that reference them; the shared driver
    // owns the confirmation, journal, and settle
    Box::pin(imports::disk::run_disk_import(
        thorium,
        conf,
        &progress,
        &opts,
        images,
        pipelines,
        cmd.rollback_on_failure,
    ))
    .await
}

/// Collect the images referenced by exported pipeline configs, in sorted order
///
/// A pipeline whose image order can't be read is logged and recorded as failed.
///
/// # Arguments
///
/// * `exported` - The pipeline configs now on disk
/// * `progress` - The progress bar to log through
/// * `report` - The export report to record failures in
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn referenced_images(
    exported: &[ExportedConfig<PipelineRequest>],
    progress: &Bar,
    report: &mut ExportReport,
) -> BTreeSet<String> {
    let mut images = BTreeSet::new();
    for config in exported {
        // add every image across all of this pipeline's stages
        match config.request.deserialize_image_order() {
            Ok(order) => images.extend(order.into_iter().flatten().map(ToOwned::to_owned)),
            Err(err) => {
                progress.error(format!(
                    "Failed to read the image order of pipeline '{}': {err}",
                    config.name
                ));
                report.fail("pipeline", config.name.clone());
            }
        }
    }
    images
}

/// Export pipelines and the images they reference from Thorium
///
/// Pipeline configs are written first, then the configs (and K8s tarballs) of every
/// image referenced by a pipeline config on disk, all through one conflict resolver
/// so "Overwrite all"/"Skip all" carry across both passes. A Quit at a conflict
/// prompt stops writing pipeline configs but still exports the images of those
/// already on disk (without further prompts, leaving differing files untouched) so
/// the export stays importable, then exits non-zero ("Export stopped early").
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The export pipelines command to execute
/// * `args` - The shared Thorctl args (for the worker count)
/// * `conf` - The Thorctl config
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn export(
    thorium: &Thorium,
    cmd: &ExportPipelines,
    args: &Args,
    conf: &CtlConf,
) -> Result<(), Error> {
    // fail before doing any work if --review can't open an editor
    exports::require_review_terminal(cmd.review)?;
    // no explicit list exports every pipeline in the group
    let listed = if cmd.pipelines.is_empty() {
        let names: Vec<String> = utils::pipelines::list_all_pipelines(thorium, &cmd.group)
            .await?
            .into_iter()
            .map(|pipeline| pipeline.name)
            .collect();
        // an empty group has nothing to export; say so rather than silently succeeding
        if names.is_empty() {
            return Err(Error::new(format!(
                "No pipelines found in group '{}'",
                cmd.group
            )));
        }
        Some(names)
    } else {
        None
    };
    // --quiet gets an inert bar that still prints warnings and errors
    let progress = Bar::new_or_quiet(
        "pipelines export",
        "Exporting pipeline configs",
        BarKind::Timer,
        args.quiet,
    );
    // explicit names are de-duplicated so one pipeline is never exported twice
    let names = listed.unwrap_or_else(|| imports::dedup_names(cmd.pipelines.clone(), &progress));
    // one resolver for the pipeline and image passes so "all" choices carry across both
    let can_prompt = !cmd.skip_conflicts && imports::is_interactive_terminal();
    let editor = resolve_editor(cmd.editor.as_deref(), conf);
    let mut resolver = DiskConflictResolver::new(cmd.overwrite, can_prompt, editor.to_string())
        .explicit_skip(cmd.skip_conflicts);
    let mut report = ExportReport::default();
    // fetch the pipelines concurrently (bounded by --workers, never zero); `buffered`
    // keeps the input order so prompts and errors come out in a stable order
    let fetch_workers = std::cmp::min(args.workers, names.len()).max(1);
    let fetched: Vec<(String, Result<Pipeline, Error>)> = stream::iter(names)
        .map(|name| async move {
            let result = thorium.pipelines.get(&cmd.group, &name).await;
            (name, result)
        })
        .buffered(fetch_workers)
        .collect()
        .await;
    // write each pipeline config sequentially so conflicts can be resolved interactively
    let cfg = ConfigExport {
        kind: "pipeline",
        dir: cmd.output.join("pipelines"),
        order: PIPELINE_FIELD_ORDER,
        review: cmd.review,
        editor,
    };
    let exported = exports::export_configs::<Pipeline, PipelineRequest>(
        &cfg,
        fetched,
        |request: &PipelineRequest| request.name.as_str(),
        &mut resolver,
        &progress,
        &mut report,
    )
    .await;
    // collect the images referenced by the pipeline configs now on disk, since those
    // are what a later import needs
    let images = referenced_images(&exported, &progress, &mut report);
    // after a Quit, finish the images the exported pipelines need without prompting again
    if report.stopped {
        resolver.stop_prompting();
        if !images.is_empty() {
            progress.warning(format!(
                "Export stopped early; still exporting the {} image(s) referenced by the pipelines \
                 already on disk (differing image configs are left untouched)",
                images.len()
            ));
        }
    }
    // export the referenced images; an empty set is skipped since there is nothing to do
    if !images.is_empty() {
        progress.set_message("Exporting image configs");
        let opts = ImageExportOpts {
            group: &cmd.group,
            output: &cmd.output,
            config_only: cmd.config_only,
            review: cmd.review,
            editor,
        };
        let image_report = export_images_with(
            thorium,
            &opts,
            images.into_iter().collect(),
            &mut resolver,
            &progress,
            args,
            conf,
        )
        .await;
        report.failures.extend(image_report.failures);
        report.stopped |= image_report.stopped;
    }
    // print the final banner and exit non-zero on any failure or a Quit
    progress.refresh(report.banner(), BarKind::Timer);
    progress.finish();
    report.into_result()
}

/// Handle all pipelines commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The pipelines command to execute
pub async fn handle(args: &Args, cmd: &Pipelines) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        super::update::ask_update(&thorium).await?;
    }
    // call the right pipelines handler
    match cmd {
        Pipelines::Get(cmd) => get(thorium, cmd).await,
        Pipelines::Describe(cmd) => describe(thorium, cmd).await,
        Pipelines::Edit(cmd) => edit::edit(thorium, &conf, cmd).await,
        Pipelines::Notifications(cmd) => notifications::handle(thorium, cmd).await,
        Pipelines::Bans(cmd) => bans::handle(thorium, cmd).await,
        Pipelines::Delete(cmd) => delete(thorium, cmd).await,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Pipelines::Import(cmd) => {
            // resolve the container runtime (docker/podman) before any image work
            super::container::init_runtime(args.container_runtime, conf.container_runtime);
            import(&thorium, cmd, &conf, args.workers).await
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Pipelines::Export(cmd) => {
            // resolve the container runtime (docker/podman) before any image work
            super::container::init_runtime(args.container_runtime, conf.container_runtime);
            export(&thorium, cmd, args, &conf).await
        }
    }
}
