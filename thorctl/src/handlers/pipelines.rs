use itertools::Itertools;
use thorium::CtlConf;
use thorium::models::{EntityMetadata, Pipeline, PipelineUpdate};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::pipelines::{DescribePipelines, GetPipelines, Pipelines, StageTool};
use crate::args::{Args, DescribeCommand};
use crate::handlers::run_batch::BatchOpts;
use crate::utils;
use crate::utils::pipelines::stage_tools::StageToolRef;

mod bans;
mod create;
mod edit;
mod notifications;
pub(crate) mod view;

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "macos"))] {
        use futures::stream::{self, StreamExt};

        use crate::args::pipelines::{ExportPipelines, ImportPipelines};
        use crate::handlers::imports::{
            self, ConflictMode, editor::resolve_editor, merge::PIPELINE_FIELD_ORDER,
        };
        use crate::handlers::images::import::{ImageImportOpts, categorize_from_disk};
        use crate::handlers::exports::{
            self, ConfigExport, DiskConflictResolver, ExportReport,
        };
        use crate::handlers::progress::{Bar, BarKind};
        use edit::{PipelineAt, PipelineData, PipelineEdit};
        use import::PipelineConfig;
        use crate::handlers::edit::Editable;

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

/// Import pipelines to Thorium
///
/// Every config must name a tool for each stage, which is checked before anything is requested
/// from Thorium; the tools are then looked up by name in the target group. The whole import
/// shares one rollback journal, so stopping partway (editor Quit or an error) can offer to undo
/// everything applied so far. The user is asked to confirm once, up front, when existing
/// resources conflict or groups will be created. Thorium doesn't create or update images, so an
/// export's image configs are not imported.
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
    // read every config first so an export without stage tools fails before any request
    let mut configs: Vec<(String, PipelineConfig)> = Vec::with_capacity(pipeline_names.len());
    for name in &pipeline_names {
        configs.push((name.clone(), import::load_config(&cmd.import, name).await?));
    }
    // resolve each pipeline's tools in the target group
    let mut pipeline_items = Vec::with_capacity(configs.len());
    for (name, config) in &configs {
        let request = import::to_request(thorium, &cmd.group, config).await?;
        pipeline_items.push((name.clone(), None, request));
    }
    // build the import options from the pipeline import's flags
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
    // categorize before changing anything; pipelines bring no images
    let images = categorize_from_disk(thorium, &opts, &[], &progress).await?;
    let pipelines =
        imports::categorize::categorize_pipelines(thorium, pipeline_items, &progress).await?;
    // the shared driver owns the confirmation, journal, and settle
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

/// Export pipelines from Thorium
///
/// Pipeline configs are written through one conflict resolver so "Overwrite all"/"Skip all"
/// carry across them. Each stage's tool is written by name (and version, when pinned), so the
/// config imports into any deployment whose target group has tools of those names. Thorium
/// doesn't create images, so none are exported.
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
    let can_prompt = !cmd.skip_conflicts && imports::is_interactive_terminal();
    let editor = resolve_editor(cmd.editor.as_deref(), conf);
    let mut resolver = DiskConflictResolver::new(cmd.overwrite, can_prompt, editor.to_string())
        .explicit_skip(cmd.skip_conflicts);
    let mut report = ExportReport::default();
    // fetch the pipelines and their tools concurrently (bounded by --workers, never zero);
    // `buffered` keeps the input order so prompts and errors come out in a stable order
    let fetch_workers = std::cmp::min(args.workers, names.len()).max(1);
    let fetched: Vec<(String, Result<PipelineData, Error>)> = stream::iter(names)
        .map(|name| async move {
            let target = PipelineAt {
                group: cmd.group.clone(),
                name: name.clone(),
            };
            let result = PipelineEdit::fetch(thorium, &target).await;
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
    exports::export_configs::<PipelineData, PipelineConfig>(
        &cfg,
        fetched,
        |config: &PipelineConfig| config.name.as_str(),
        &mut resolver,
        &progress,
        &mut report,
    )
    .await;
    // report the configs left untouched, which refuses the export unless --skip-conflicts asked
    resolver.settle(&mut report, &progress);
    // print the final banner and exit non-zero on any failure or a Quit
    progress.refresh(report.banner(), BarKind::Timer);
    progress.finish();
    report.into_result()
}

/// Remove a stage, and with it the stage's tool, from a pipeline
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The pipeline's group
/// * `cmd` - The stage tool command that was run
async fn remove_stage(thorium: &Thorium, group: &str, cmd: &StageTool) -> Result<(), Error> {
    // read the pipeline's order
    let pipeline = thorium.pipelines.get(group, &cmd.pipeline).await?;
    // drop the stage, and any group it leaves empty
    let order: Vec<Vec<String>> = pipeline
        .order
        .iter()
        .map(|stages| {
            stages
                .iter()
                .filter(|stage| **stage != cmd.stage)
                .cloned()
                .collect::<Vec<String>>()
        })
        .filter(|stages| !stages.is_empty())
        .collect();
    // the stage must be in the pipeline, and a pipeline keeps at least one stage
    if !pipeline
        .order
        .iter()
        .flatten()
        .any(|stage| *stage == cmd.stage)
    {
        return Err(Error::new(format!(
            "Pipeline '{}' has no stage '{}'",
            utils::resource_id(group, &cmd.pipeline),
            cmd.stage
        )));
    }
    if order.is_empty() {
        return Err(Error::new(format!(
            "Stage '{}' is the only stage of pipeline '{}'; delete the pipeline instead",
            cmd.stage,
            utils::resource_id(group, &cmd.pipeline)
        )));
    }
    // remove the stage from the order together with its tool
    let update = PipelineUpdate::default()
        .order(order)
        .remove_stage_tool(&cmd.stage);
    thorium
        .pipelines
        .update(group, &cmd.pipeline, &update)
        .await?;
    println!(
        "Removed stage '{}' from pipeline '{}'",
        cmd.stage, cmd.pipeline
    );
    Ok(())
}

/// Find the id a stage should run: a tool, or one of the tool's versions
///
/// Returns the id, the tool's name, and whether the stage is pinned to a version.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The stage tool command that was run
/// * `group` - The pipeline's group, which a tool named without a group is looked up in
/// * `named` - The tool as the user named it
async fn stage_tool_id(
    thorium: &Thorium,
    cmd: &StageTool,
    group: &str,
    named: &str,
) -> Result<(Uuid, String, bool), Error> {
    // a version can be named with the tool as TOOL@VERSION
    let wanted = StageToolRef::parse(named)?;
    if wanted.version.is_some() && cmd.pin.is_some() {
        return Err(Error::new(
            "Name the version as TOOL@VERSION or with --pin, not both",
        ));
    }
    // find the tool the stage should run, in the pipeline's group unless the name says another
    let qualified = if Uuid::parse_str(&wanted.tool).is_ok() || wanted.tool.contains('/') {
        wanted.tool.clone()
    } else {
        format!("{group}/{}", wanted.tool)
    };
    let tool = super::tools::find_tool(thorium, &qualified).await?;
    // a pinned version must be one of the tool's instances
    if let Some(version) = &wanted.version {
        let EntityMetadata::Tool(meta) = &tool.metadata else {
            return Err(Error::new(format!("'{}' is not a tool", tool.name)));
        };
        let id = meta
            .instances
            .iter()
            .find(|summary| summary.version.to_string() == *version)
            .map(|summary| summary.id)
            .ok_or_else(|| {
                Error::new(format!(
                    "Tool '{}' has no version '{version}' (it has: {})",
                    tool.name,
                    meta.instances
                        .iter()
                        .map(|summary| summary.version.to_string())
                        .collect::<Vec<String>>()
                        .join(", ")
                ))
            })?;
        return Ok((id, tool.name, true));
    }
    // an instance named with --pin must be a version of this tool
    let id = match cmd.pin {
        Some(pin) => {
            let instance = thorium.entities.get(pin).await?;
            match &instance.metadata {
                EntityMetadata::ToolInstance(meta) if meta.tool == tool.id => pin,
                _ => {
                    return Err(Error::new(format!(
                        "{pin} is not a version of the tool `{}`",
                        tool.name
                    )));
                }
            }
        }
        None => tool.id,
    };
    Ok((id, tool.name, cmd.pin.is_some()))
}

/// Set the tool that runs a pipeline stage, or remove the stage
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The stage tool command that was run
async fn stage_tool(thorium: &Thorium, cmd: &StageTool) -> Result<(), Error> {
    // the tool to set is needed unless the stage is being removed
    let named = match (&cmd.tool, cmd.remove) {
        (Some(tool), false) => Some(tool.as_str()),
        (_, true) => None,
        (None, false) => return Err(Error::new("A tool is needed to set a stage's tool")),
    };
    // find the pipeline's group if it wasn't given
    let group = match &cmd.group {
        Some(group) => group.clone(),
        None => utils::pipelines::find_pipeline_group(thorium, &cmd.pipeline).await?,
    };
    // find the id to set before any write
    let found = match named {
        Some(named) => Some(stage_tool_id(thorium, cmd, &group, named).await?),
        None => None,
    };
    // removing a stage takes its own path
    let Some((id, tool_name, pinned)) = found else {
        return remove_stage(thorium, &group, cmd).await;
    };
    // set the stage's tool
    let update = PipelineUpdate::default().add_stage_tool(&cmd.stage, id);
    thorium
        .pipelines
        .update(&group, &cmd.pipeline, &update)
        .await?;
    // say what the stage runs now
    if pinned {
        println!(
            "Stage '{}' of pipeline '{}' runs tool '{tool_name}' pinned to version {id}",
            cmd.stage, cmd.pipeline
        );
    } else {
        println!(
            "Stage '{}' of pipeline '{}' runs the latest version of tool '{tool_name}'",
            cmd.stage, cmd.pipeline
        );
    }
    Ok(())
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
    let result = match cmd {
        Pipelines::Get(cmd) => get(thorium, cmd).await,
        Pipelines::Describe(cmd) => describe(thorium, cmd).await,
        Pipelines::Create(cmd) => create::create(&thorium, cmd).await,
        Pipelines::Edit(cmd) => edit::edit(thorium, &conf, cmd).await,
        Pipelines::Update(cmd) => edit::update(thorium, cmd).await,
        Pipelines::Run(cmd) => {
            crate::handlers::runs::run_pipeline(&thorium, cmd, &BatchOpts::from_args(args)).await
        }
        Pipelines::StageTool(cmd) => stage_tool(&thorium, cmd).await,
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
    };
    // show the API's status code and message with any refusal
    result.map_err(show_status)
}

/// Turn an API refusal into a message that names its status code, as the tool commands print it
///
/// Other errors are returned unchanged.
///
/// # Arguments
///
/// * `err` - The error a pipelines command returned
fn show_status(err: Error) -> Error {
    // an error with a status came from the API: print its code and message, not its debug form
    match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    }
}

#[cfg(test)]
mod tests {
    use super::show_status;
    use thorium::Error;

    /// An API refusal prints as its status code and the API's message
    #[test]
    fn refusals_print_their_code_and_message() {
        // a 400 the API answered with a JSON error
        let err = Error::Thorium {
            code: http::StatusCode::BAD_REQUEST,
            msg: Some(r#"{"error":"the name 'pipeline' is reserved"}"#.to_owned()),
        };
        // the message is plain text with the code, which main prints as it is
        match show_status(err) {
            Error::Generic(msg) => {
                assert!(msg.contains("400"), "{msg}");
                assert!(msg.contains("the name 'pipeline' is reserved"), "{msg}");
            }
            other => panic!("expected a plain message, got {other:?}"),
        }
    }

    /// An error without a status is returned as it was
    #[test]
    fn other_errors_are_unchanged() {
        // a local error, such as a bad stage list
        let err = Error::new("A pipeline needs at least one stage");
        match show_status(err) {
            Error::Generic(msg) => assert_eq!(msg, "A pipeline needs at least one stage"),
            other => panic!("expected the same message, got {other:?}"),
        }
    }
}
