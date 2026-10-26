//! Arguments for pipeline-related Thorctl commands

#![allow(clippy::module_name_repetitions)]

use clap::Parser;
use clap::builder::NonEmptyStringValueParser;
use std::path::PathBuf;
use uuid::Uuid;

use crate::utils;
use crate::utils::pipelines::stage_tools::{StageToolRef, parse_stage_tool};

use super::helpers::{EditArgs, ShortFormatArgs};
use super::runs::RunPipeline;
use super::traits::describe::{DescribeCommand, DescribeFormat, DescribeSealed};
use super::traits::search::{SearchParameterized, SearchParams, SearchSealed};
use super::{CreateNotification, GetNotificationOpts};

/// The commands to send to the pipelines task handler
#[derive(Parser, Debug)]
pub enum Pipelines {
    /// Get available pipelines and their details
    #[clap(version, author)]
    Get(GetPipelines),
    /// Describe specific pipelines in a human-readable format (use `--format json`
    /// for the full raw config)
    #[clap(version, author)]
    Describe(DescribePipelines),
    /// Create a pipeline from stages and the tool that runs each one
    ///
    /// The API needs a tool for every stage, so give each stage in `--stage` a matching
    /// `--stage-tool STAGE=TOOL[@VERSION]` (or describe the pipeline in a `--spec` file).
    #[clap(version, author, verbatim_doc_comment)]
    Create(CreatePipeline),
    /// Edit a pipeline in an editor, or from `--file`, `--patch`, `--set` and `--unset`
    ///
    /// The document lists the order, SLA, description, triggers, and the tool behind each stage.
    /// Group, name, and bans are not editable here.
    #[clap(version, author, verbatim_doc_comment)]
    Edit(EditPipeline),
    /// Update a pipeline from flags or a document without opening an editor
    ///
    /// Typed flags (`--order`, `--sla`, ...) and the document flags (`--file`, `--patch`,
    /// `--set`, `--unset`) can be combined; the document flags apply first.
    #[clap(version, author, verbatim_doc_comment)]
    Update(UpdatePipeline),
    /// Set the tool that runs a pipeline stage, or remove a stage
    ///
    /// The stage follows the tool's latest version, or the version given as TOOL@VERSION or
    /// with `--pin`
    #[clap(version, author, verbatim_doc_comment)]
    StageTool(StageTool),
    /// Run a pipeline on files or repos
    #[clap(version, author)]
    Run(RunPipeline),
    /// Manage/list pipeline notifications
    #[clap(subcommand)]
    Notifications(PipelineNotifications),
    /// Manage/list pipeline bans
    #[clap(subcommand)]
    Bans(PipelineBans),
    /// Delete pipelines
    #[clap(version, author)]
    Delete(DeletePipelines),
    /// Import pipelines
    #[clap(version, author)]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Import(ImportPipelines),
    /// Export pipelines
    #[clap(version, author)]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Export(ExportPipelines),
}

/// A command to get info on some pipelines
#[derive(Parser, Debug)]
pub struct GetPipelines {
    /// Any groups to filter by when searching for pipelines
    ///     Note: If no groups are given, the search will include all groups the user is a part of
    #[clap(short, long, value_delimiter = ',', verbatim_doc_comment)]
    pub groups: Vec<String>,
    /// The max number of pipelines to list per group
    #[clap(short, long, default_value = "50")]
    pub limit: usize,
    /// Refrain from setting a limit when retrieving pipelines
    ///     Note: This can lead to retrieving info for many thousands of pipelines
    ///           inadvertently. Be careful!
    #[clap(long, verbatim_doc_comment)]
    pub no_limit: bool,
    /// The page size to use in retrieving the pipelines
    #[clap(short, long, default_value = "50")]
    pub page_size: usize,
    /// Print the pipelines in alphabetical order rather than by group, then creation date
    ///     Note: Sorting can require many system resources for large amounts of pipelines
    #[clap(short, long, verbatim_doc_comment)]
    pub alpha: bool,
}

/// A command to describe specific pipelines in full
#[derive(Parser, Debug)]
pub struct DescribePipelines {
    /// Any specific pipelines to describe, optionally qualified with their group in case
    /// other groups have a pipeline with the same name (e.g. `<GROUP>/<PIPELINE>`;
    /// `<PIPELINE>:<GROUP>` is also accepted)
    #[clap(value_name = "[GROUP/]PIPELINE")]
    pub pipelines: Vec<String>,
    /// The path to a file containing a list of pipelines to describe separated by newlines;
    /// each pipeline can be qualified with its group in case other groups have a pipeline
    /// with the same name (e.g. `<GROUP>/<PIPELINE>`; `<PIPELINE>:<GROUP>` is also accepted)
    #[clap(short = 'L', long = "list")]
    pub list: Option<PathBuf>,
    /// The path to the file to write output to; if not provided, details will be output to stdout
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// The output format for the description (human-readable by default)
    #[clap(long, value_enum, default_value_t, ignore_case = true)]
    pub format: DescribeFormat,
    /// Output details as condensed JSON (no formatting/whitespace); implies `--format json`
    #[clap(long)]
    pub condensed: bool,
    /// Any specific groups to filter by when describing pipelines
    #[clap(short, long, value_delimiter = ',')]
    pub groups: Vec<String>,
    /// Describe all pipelines to which you have access (still within the limit given in `--limit`)
    #[clap(long)]
    pub all: bool,
    /// The maximum number of pipelines to retrieve per group
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// Describe pipelines with no limit
    #[clap(long)]
    pub no_limit: bool,
    /// The number of pipelines to retrieve per request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
}

impl SearchSealed for DescribePipelines {
    fn get_search_params(&self) -> SearchParams<'_> {
        SearchParams {
            groups: &self.groups,
            tags: &[],
            tags_case_insensitive: false,
            delimiter: '=',
            start: &None,
            end: &None,
            date_fmt: "",
            cursor: None,
            limit: self.limit,
            no_limit: self.no_limit,
            page_size: self.page_size,
        }
    }
}

impl SearchParameterized for DescribePipelines {
    fn has_targets(&self) -> bool {
        !self.pipelines.is_empty() || self.list.is_some()
    }

    fn apply_to_all(&self) -> bool {
        self.all
    }
}

/// A specific pipeline target containing an optional group in case
/// more than one group has a pipeline with the same name
pub struct PipelineTarget {
    /// The name of the pipeline
    pub pipeline: String,
    /// The optional group that the pipeline belongs to
    pub group: Option<String>,
}

impl PipelineTarget {
    /// Parse a `<GROUP>/<PIPELINE>`, `<PIPELINE>:<GROUP>`, or bare `<PIPELINE>` target
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw target to parse
    pub fn parse(raw: &str) -> Result<Self, thorium::Error> {
        // split the target with the parser shared with image targets
        let (pipeline, group) = super::images::parse_name_group(raw, "pipeline")?;
        Ok(PipelineTarget { pipeline, group })
    }
}

impl DescribeSealed for DescribePipelines {
    type Data = thorium::models::Pipeline;

    type Target<'a> = PipelineTarget;

    type Cursor = thorium::client::Cursor<Self::Data>;

    fn raw_targets(&self) -> &[String] {
        &self.pipelines
    }

    fn condensed(&self) -> bool {
        self.condensed
    }

    fn format(&self) -> Result<DescribeFormat, thorium::Error> {
        // `--condensed` is a JSON modifier, so it forces JSON
        if self.condensed {
            return Ok(DescribeFormat::Json);
        }
        // pipelines can only be rendered as JSON or human-readable text; reject any
        // other format the shared DescribeFormat enum may offer
        if matches!(self.format, DescribeFormat::Json | DescribeFormat::Human) {
            Ok(self.format)
        } else {
            Err(thorium::Error::new(
                "describe pipelines only supports the 'json' or 'human' output format",
            ))
        }
    }

    fn render_human(
        &self,
        datum: &Self::Data,
        out: &mut dyn std::io::Write,
        ansi: bool,
    ) -> Result<(), thorium::Error> {
        utils::pipelines::print_pipeline_details(datum, out, ansi)
    }

    fn out_path(&self) -> Option<&std::path::PathBuf> {
        self.output.as_ref()
    }

    fn target_list(&self) -> Option<&std::path::PathBuf> {
        self.list.as_ref()
    }

    /// Parse a raw image or pipeline target into its name and optional group
    ///
    /// Accepts `<GROUP>/<PIPELINE>`, `<PIPELINE>:<GROUP>`, or a bare `<PIPELINE>`.
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw target to parse
    fn parse_target<'a>(&self, raw: &'a str) -> Result<Self::Target<'a>, thorium::Error> {
        PipelineTarget::parse(raw)
    }

    async fn retrieve_data(
        &self,
        target: Self::Target<'_>,
        thorium: &thorium::Thorium,
    ) -> Result<Self::Data, thorium::Error> {
        let group = if let Some(group) = &target.group {
            group.clone()
        } else {
            utils::pipelines::find_pipeline_group(thorium, &target.pipeline).await?
        };
        thorium.pipelines.get(&group, &target.pipeline).await
    }

    async fn retrieve_data_search(
        &self,
        thorium: &thorium::Thorium,
    ) -> Result<Vec<Self::Cursor>, thorium::Error> {
        let params = self.get_search_params();
        let groups = if self.apply_to_all() {
            // retrieve all of the users groups if all images should be described
            utils::groups::get_all_groups(thorium).await?
        } else {
            // otherwise use only the specified groups
            params.groups.to_vec()
        };
        let cursors = groups.iter().map(|group| {
            thorium
                .pipelines
                .list(group)
                .details()
                .page_size(params.page_size as u64)
        });
        if params.no_limit {
            Ok(cursors.collect())
        } else {
            Ok(cursors
                .map(|cursor| cursor.limit(params.limit as u64))
                .collect())
        }
    }
}

impl DescribeCommand for DescribePipelines {}

/// Args for editing a pipeline
#[derive(Parser, Debug)]
pub struct EditPipeline {
    /// The name of the pipeline to edit, optionally as `<GROUP>/<PIPELINE>`
    #[clap(value_name = "[GROUP/]PIPELINE")]
    pub pipeline: String,
    /// The group the pipeline is in; required if other pipelines have
    /// the same name
    #[clap(conflicts_with = "group_flag")]
    pub group: Option<String>,
    /// The group the pipeline is in, as a flag
    #[clap(short = 'g', long = "group", value_name = "GROUP")]
    pub group_flag: Option<String>,
    /// The editor to use when editing the pipeline (the short name of `--editor`)
    #[clap(
        id = "editor_short",
        short = 'e',
        hide = true,
        value_name = "EDITOR",
        conflicts_with = "editor"
    )]
    pub editor_short: Option<String>,
    /// How to change the pipeline, and what to do with the change
    #[clap(flatten)]
    pub edit: EditArgs,
}

impl EditPipeline {
    /// The group the user gave, as an argument or as `--group`
    #[must_use]
    pub fn group_name(&self) -> Option<&str> {
        self.group_flag.as_deref().or(self.group.as_deref())
    }
}

/// Stages in run order, as groups of stages that run together
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageOrder(pub Vec<Vec<String>>);

/// Parse an `--order` value such as `A,B;C` into stage groups
///
/// # Arguments
///
/// * `raw` - The text to parse
pub fn parse_order(raw: &str) -> Result<StageOrder, String> {
    // groups run one after another and a group's stages run together
    let groups: Vec<Vec<String>> = raw
        .split(';')
        .map(|group| {
            group
                .split(',')
                .map(|stage| stage.trim().to_owned())
                .collect()
        })
        .collect();
    // every stage needs a name
    if groups.iter().flatten().any(String::is_empty) {
        return Err(format!(
            "'{raw}' has an empty stage; write stages like 'A,B;C'"
        ));
    }
    Ok(StageOrder(groups))
}

/// Args for creating a pipeline
#[derive(Parser, Debug)]
pub struct CreatePipeline {
    /// The name of the new pipeline
    #[clap(value_parser = NonEmptyStringValueParser::new())]
    pub name: String,
    /// The group to create the pipeline in
    #[clap(short, long)]
    pub group: String,
    /// A stage, or comma separated stages that run together; repeat for each step in run order
    #[clap(long, value_name = "STAGE[,STAGE]...", value_parser = parse_order)]
    pub stage: Vec<StageOrder>,
    /// The tool that runs a stage (`TOOL` follows its latest version, `TOOL@VERSION` pins one);
    /// every stage needs one
    #[clap(long, value_name = "STAGE=TOOL[@VERSION]", value_parser = parse_stage_tool)]
    pub stage_tool: Vec<(String, StageToolRef)>,
    /// The seconds a reaction has to meet the pipeline's SLA
    #[clap(long)]
    pub sla: Option<u64>,
    /// A description of the pipeline
    #[clap(short, long)]
    pub description: Option<String>,
    /// A trigger that starts the pipeline: `NAME=JSON` or `NAME=@FILE`; repeatable
    #[clap(long, value_name = "NAME=JSON|@FILE")]
    pub trigger: Vec<String>,
    /// A YAML or JSON document describing the pipeline (`-` reads stdin); the flags above
    /// add to or replace what it holds
    #[clap(long, value_name = "FILE|-")]
    pub spec: Option<String>,
    /// Print the pipeline that would be created without creating it
    #[clap(long)]
    pub dry_run: bool,
    /// How to print the new pipeline
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// Args for updating a pipeline without an editor
#[derive(Parser, Debug)]
pub struct UpdatePipeline {
    /// The pipeline to update, optionally as `<GROUP>/<PIPELINE>`
    #[clap(value_name = "[GROUP/]PIPELINE")]
    pub pipeline: String,
    /// The group the pipeline is in; required if other pipelines have the same name
    #[clap(short, long)]
    pub group: Option<String>,
    /// Replace the order: `;` separates steps and `,` separates stages that run together
    /// (`--order 'A,B;C'`); every new stage needs a `--stage-tool`
    #[clap(long, value_name = "STAGES", value_parser = parse_order)]
    pub order: Option<StageOrder>,
    /// The seconds a reaction has to meet the pipeline's SLA
    #[clap(long)]
    pub sla: Option<u64>,
    /// Set the pipeline's description
    #[clap(short, long, conflicts_with = "clear_description")]
    pub description: Option<String>,
    /// Remove the pipeline's description
    #[clap(long)]
    pub clear_description: bool,
    /// Add or replace a trigger: `NAME=JSON` or `NAME=@FILE`; repeatable
    #[clap(long, value_name = "NAME=JSON|@FILE")]
    pub add_trigger: Vec<String>,
    /// Remove a trigger by name; repeatable
    #[clap(long, value_name = "NAME")]
    pub remove_trigger: Vec<String>,
    /// Set the tool that runs a stage (`TOOL` follows its latest version, `TOOL@VERSION` pins
    /// one); repeatable
    #[clap(long, value_name = "STAGE=TOOL[@VERSION]", value_parser = parse_stage_tool)]
    pub stage_tool: Vec<(String, StageToolRef)>,
    /// Remove a stage's tool; only valid for a stage the update also removes from the order
    #[clap(long, value_name = "STAGE")]
    pub remove_stage_tool: Vec<String>,
    /// How to change the pipeline from a document, and what to do with the change
    #[clap(flatten)]
    pub edit: EditArgs,
}

impl UpdatePipeline {
    /// Whether any typed flag asks for a change
    #[must_use]
    pub fn has_flag_changes(&self) -> bool {
        self.order.is_some()
            || self.sla.is_some()
            || self.description.is_some()
            || self.clear_description
            || !self.add_trigger.is_empty()
            || !self.remove_trigger.is_empty()
            || !self.stage_tool.is_empty()
            || !self.remove_stage_tool.is_empty()
    }
}

/// A command to set the tool that runs a pipeline stage, or to remove a stage
#[derive(Parser, Debug, Clone)]
pub struct StageTool {
    /// The name of the pipeline
    pub pipeline: String,
    /// The name of the stage
    pub stage: String,
    /// The tool to run the stage with, by ID or name; `TOOL@VERSION` pins a version
    #[clap(required_unless_present = "remove")]
    pub tool: Option<String>,
    /// Pin the stage to this version of the tool (a tool instance ID) instead of its latest
    #[clap(long, conflicts_with = "remove")]
    pub pin: Option<Uuid>,
    /// Remove the stage (and its tool) from the pipeline instead of setting its tool
    #[clap(long, conflicts_with = "tool")]
    pub remove: bool,
    /// The group the pipeline is in; required if other pipelines have the same name
    #[clap(short, long)]
    pub group: Option<String>,
}

/// The pipeline ban specific subcommands
#[derive(Parser, Debug, Clone)]
pub enum PipelineBans {
    /// Add a ban to a pipeline, preventing it from being run
    #[clap(version, author)]
    Create(CreatePipelineBan),
    /// Remove a ban from a pipeline
    #[clap(version, author)]
    Delete(DeletePipelineBan),
}

/// The args related to adding pipeline bans
#[derive(Parser, Debug, Clone)]
pub struct CreatePipelineBan {
    /// The pipeline's group
    pub group: String,
    /// The name of the pipeline
    pub pipeline: String,
    /// The message explaining why the pipeline was banned
    pub msg: String,
}

/// The args related to removing pipeline bans
#[derive(Parser, Debug, Clone)]
pub struct DeletePipelineBan {
    /// The pipeline's group
    pub group: String,
    /// The name of the pipeline
    pub pipeline: String,
    /// The pipeline ban's unique ID
    pub id: Uuid,
}

/// The pipeline notification specific subcommands
#[derive(Parser, Debug, Clone)]
pub enum PipelineNotifications {
    /// Get notifications for a pipeline
    #[clap(version, author)]
    Get(GetPipelineNotifications),
    /// Create a pipeline notification
    #[clap(version, author)]
    Create(CreatePipelineNotification),
    /// Delete a pipeline notification
    #[clap(version, author)]
    Delete(DeletePipelineNotification),
}

/// A command to get a pipeline's notifications
#[derive(Parser, Debug, Clone)]
pub struct GetPipelineNotifications {
    /// The group the pipeline belongs to
    pub group: String,
    /// The pipeline to get notifications for
    pub pipeline: String,
    /// The options for getting notifications
    #[clap(flatten)]
    pub opts: GetNotificationOpts,
}

/// The args related to creating pipeline notifications
#[derive(Parser, Debug, Clone)]
pub struct CreatePipelineNotification {
    /// The pipeline's group
    pub group: String,
    /// The name of the pipeline
    pub pipeline: String,
    /// The params needed when creating a notification
    #[clap(flatten)]
    pub notification: CreateNotification,
}

/// The args related to deleting pipeline notifications
#[derive(Parser, Debug, Clone)]
pub struct DeletePipelineNotification {
    /// The pipeline's group
    pub group: String,
    /// The name of the pipeline
    pub pipeline: String,
    /// The notification's unique ID
    pub id: Uuid,
}

/// A command to delete pipelines
#[derive(Parser, Debug, Clone)]
pub struct DeletePipelines {
    /// The pipelines to delete
    #[clap(required = true, value_parser = NonEmptyStringValueParser::new())]
    pub pipelines: Vec<String>,
    /// The group to delete pipelines from
    #[clap(short, long, required = true)]
    pub group: String,
    /// Skip the confirmation dialog
    #[clap(long, visible_alias = "yes")]
    pub skip_confirm: bool,
}

/// A command to export pipelines
#[derive(Parser, Debug, Clone)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportPipelines {
    /// The pipelines to export, by name (default: every pipeline in the group)
    #[clap(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
    pub pipelines: Vec<String>,
    /// The group to export pipelines from
    #[clap(short, long, value_name = "GROUP", required = true)]
    pub group: String,
    /// The directory to export pipelines to
    #[clap(short, long, value_name = "DIR", default_value = "exports")]
    pub output: PathBuf,
    /// Only export pipeline/image configs, without container image tarballs (tarballs
    /// are only saved for K8s images, the only ones import loads them for)
    #[clap(long)]
    pub config_only: bool,
    /// Overwrite existing on-disk configs that differ without prompting
    #[clap(long, conflicts_with = "skip_conflicts")]
    pub overwrite: bool,
    /// Skip on-disk conflicts: write new configs and leave differing existing ones
    /// untouched with a warning (use --overwrite to overwrite instead)
    ///
    /// Without a terminal, differing configs end the export in an error unless this or
    /// --overwrite is given; with this flag a summary line says how many were skipped.
    #[clap(long)]
    pub skip_conflicts: bool,
    /// Open each config in an editor to review/tweak it before writing (requires a terminal)
    #[clap(long)]
    pub review: bool,
    /// Override the default editor for reviews and on-disk merge conflicts
    #[clap(long, value_name = "EDITOR")]
    pub editor: Option<String>,
}

/// A command to import pipelines
#[derive(Parser, Debug, Clone)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::struct_excessive_bools)]
pub struct ImportPipelines {
    /// The pipelines to import, by name (default: every pipeline config in the import directory)
    #[clap(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
    pub pipelines: Vec<String>,
    /// The group to import pipelines to
    #[clap(short, long, value_name = "GROUP", required = true)]
    pub group: String,
    /// The directory to import pipelines from
    #[clap(short, long, value_name = "DIR", required = true)]
    pub import: PathBuf,
    /// The registry to retag and push container images to; only applies to images whose
    /// container is actually pushed (K8s images with a tarball in the export, without
    /// --skip-push)
    #[clap(short, long, value_name = "REGISTRY")]
    pub registry: Option<String>,
    /// The registry url to override the domain stored in Thorium with; applies to every
    /// image, whether or not its container is pushed
    #[clap(long, value_name = "URL")]
    pub registry_override: Option<String>,
    /// Skip loading, retagging, and pushing container image tarballs (docker/podman);
    /// the configs are still imported and --registry has no effect
    #[clap(long)]
    pub skip_push: bool,
    /// Only migrate image urls: existing images get a url-only update (rewritten by
    /// --registry/--registry-override, skipped if the url already matches), missing
    /// images and pipelines are created, existing pipelines are left untouched,
    /// and failures are collected and reported at the end
    #[clap(long, conflicts_with_all = ["overwrite", "skip_conflicts"])]
    pub migrate_registry: bool,
    /// Overwrite existing pipelines/images without opening the editor
    #[clap(long, conflicts_with = "skip_conflicts")]
    pub overwrite: bool,
    /// Skip existing pipelines/images that differ instead of updating them
    ///
    /// Each skipped resource logs a warning listing the fields that differ,
    /// making this safe for non-interactive imports that must never
    /// overwrite local changes. A summary line says how many were skipped.
    ///
    /// Without a terminal, an import that finds differing resources refuses (exit 1)
    /// unless this or --overwrite is given.
    #[clap(long)]
    pub skip_conflicts: bool,
    /// Automatically roll back applied changes if the import stops early
    ///
    /// Only applies when the session can't prompt; interactive sessions are
    /// asked instead. Registry pushes are not undone, only Thorium state.
    #[clap(long)]
    pub rollback_on_failure: bool,
    /// Override the default editor for reviewing merge conflicts
    #[clap(long, value_name = "EDITOR")]
    pub editor: Option<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use uuid::Uuid;

    use super::Pipelines;
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

    /// Parse thorctl arguments into a pipelines command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl pipelines`
    fn parse(args: &[&str]) -> Result<Pipelines, clap::Error> {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "pipelines"];
        full.extend_from_slice(args);
        // parse them into a pipelines command
        let SubCommands::Pipelines(pipelines) = Args::try_parse_from(full)?.cmd else {
            panic!("expected a pipelines command");
        };
        Ok(pipelines)
    }

    /// `pipelines stage-tool` takes a pipeline, a stage, and a tool, plus a pinned version and
    /// group
    #[test]
    fn stage_tool_args() {
        // the tool follows its latest version by default
        let Pipelines::StageTool(cmd) =
            parse(&["stage-tool", "orbit", "plant", "scanner"]).unwrap()
        else {
            panic!("expected pipelines stage-tool");
        };
        assert_eq!(cmd.pipeline, "orbit");
        assert_eq!(cmd.stage, "plant");
        assert_eq!(cmd.tool.as_deref(), Some("scanner"));
        assert_eq!(cmd.pin, None);
        assert!(!cmd.remove);
        assert_eq!(cmd.group, None);
        // a pinned version and group are taken
        let pinned = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Pipelines::StageTool(cmd) = parse(&[
            "stage-tool",
            "orbit",
            "plant",
            "scanner",
            "--pin",
            pinned,
            "-g",
            "jupiter",
        ])
        .unwrap() else {
            panic!("expected pipelines stage-tool");
        };
        assert_eq!(cmd.pin, Some(Uuid::parse_str(pinned).unwrap()));
        assert_eq!(cmd.group.as_deref(), Some("jupiter"));
        // the tool is required and the pin must be an id
        assert!(parse(&["stage-tool", "orbit", "plant"]).is_err());
        assert!(parse(&["stage-tool", "orbit", "plant", "scanner", "--pin", "latest"]).is_err());
    }

    /// `pipelines stage-tool --remove` takes no tool, and cannot be combined with one
    #[test]
    fn stage_tool_remove_args() {
        // removing needs only the pipeline and the stage
        let Pipelines::StageTool(cmd) =
            parse(&["stage-tool", "orbit", "plant", "--remove", "-g", "jupiter"]).unwrap()
        else {
            panic!("expected pipelines stage-tool");
        };
        assert!(cmd.remove);
        assert_eq!(cmd.tool, None);
        // a tool or a pin makes no sense with --remove
        assert!(parse(&["stage-tool", "orbit", "plant", "scanner", "--remove"]).is_err());
        let pinned = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        assert!(parse(&["stage-tool", "orbit", "plant", "--remove", "--pin", pinned]).is_err());
        // without --remove the tool is still required
        assert!(parse(&["stage-tool", "orbit", "plant"]).is_err());
    }

    /// `--order` and `--stage` write steps with `;` and stages that run together with `,`
    #[test]
    fn order_text_parses_into_groups() {
        let order = super::parse_order("A,B;C").unwrap();
        assert_eq!(
            order.0,
            vec![vec!["A".to_owned(), "B".to_owned()], vec!["C".to_owned()]]
        );
        // spaces around names are ignored and a lone stage is one group
        assert_eq!(
            super::parse_order(" A ").unwrap().0,
            vec![vec!["A".to_owned()]]
        );
        // empty stages are refused
        assert!(super::parse_order("A,,B").is_err());
        assert!(super::parse_order("A;").is_err());
        assert!(super::parse_order("").is_err());
    }

    /// `pipelines create` takes a name, a group, stages with tools, and the optional settings
    #[test]
    fn create_args() {
        let Pipelines::Create(cmd) = parse(&[
            "create",
            "orbit",
            "-g",
            "jupiter",
            "--stage",
            "plant",
            "--stage",
            "grow,fertilize",
            "--stage-tool",
            "plant=seeder",
            "--stage-tool",
            "grow=grower@1.2.0",
            "--sla",
            "3600",
            "-d",
            "A pipeline",
            "--trigger",
            "t=NewSample",
            "--dry-run",
            "-o",
            "json",
        ])
        .unwrap() else {
            panic!("expected pipelines create");
        };
        assert_eq!(
            (cmd.name.as_str(), cmd.group.as_str()),
            ("orbit", "jupiter")
        );
        assert_eq!(cmd.stage.len(), 2);
        assert_eq!(
            cmd.stage[1].0,
            vec![vec!["grow".to_owned(), "fertilize".to_owned()]]
        );
        assert_eq!(cmd.stage_tool[1].0, "grow");
        assert_eq!(cmd.stage_tool[1].1.version.as_deref(), Some("1.2.0"));
        assert_eq!(cmd.sla, Some(3600));
        assert_eq!(cmd.description.as_deref(), Some("A pipeline"));
        assert_eq!(cmd.trigger, ["t=NewSample"]);
        assert!(cmd.dry_run);
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        // the group is required, and a stage tool needs STAGE=TOOL
        assert!(parse(&["create", "orbit"]).is_err());
        assert!(parse(&["create", "orbit", "-g", "j", "--stage-tool", "plant"]).is_err());
        // a spec can stand in for the stages
        let Pipelines::Create(cmd) = parse(&["create", "orbit", "-g", "j", "--spec", "-"]).unwrap()
        else {
            panic!("expected pipelines create");
        };
        assert_eq!(cmd.spec.as_deref(), Some("-"));
    }

    /// `pipelines update` takes typed flags and the shared document flags together
    #[test]
    fn update_args() {
        let Pipelines::Update(cmd) = parse(&[
            "update",
            "jupiter/orbit",
            "--order",
            "A,B;C",
            "--sla",
            "10",
            "-d",
            "text",
            "--add-trigger",
            "t=NewSample",
            "--remove-trigger",
            "old",
            "--stage-tool",
            "C=tool@1.0.0",
            "--remove-stage-tool",
            "D",
            "--set",
            "sla=5",
            "--patch",
            "{}",
            "--dry-run",
            "--yes",
        ])
        .unwrap() else {
            panic!("expected pipelines update");
        };
        assert_eq!(cmd.pipeline, "jupiter/orbit");
        assert_eq!(cmd.order.unwrap().0.len(), 2);
        assert_eq!(cmd.sla, Some(10));
        assert_eq!(cmd.add_trigger, ["t=NewSample"]);
        assert_eq!(cmd.remove_trigger, ["old"]);
        assert_eq!(cmd.stage_tool[0].0, "C");
        assert_eq!(cmd.remove_stage_tool, ["D"]);
        assert!(cmd.edit.dry_run && cmd.edit.skip_confirm);
        assert_eq!(cmd.edit.ops.len(), 1);
        // --description and --clear-description are exclusive
        assert!(parse(&["update", "orbit", "-d", "x", "--clear-description"]).is_err());
        // a bad order is a usage error
        assert!(parse(&["update", "orbit", "--order", "A,,B"]).is_err());
    }

    /// `pipelines update` knows whether a typed flag asks for a change
    #[test]
    fn update_flag_changes_are_detected() {
        let Pipelines::Update(none) = parse(&["update", "orbit", "--set", "sla=1"]).unwrap() else {
            panic!("expected pipelines update");
        };
        assert!(!none.has_flag_changes());
        let Pipelines::Update(some) = parse(&["update", "orbit", "--clear-description"]).unwrap()
        else {
            panic!("expected pipelines update");
        };
        assert!(some.has_flag_changes());
    }

    /// `pipelines edit` keeps its positional group and `-e`, and gains `-g` and the edit flags
    #[test]
    fn edit_args() {
        // the existing positional group still works
        let Pipelines::Edit(cmd) = parse(&["edit", "orbit", "jupiter", "-e", "nano"]).unwrap()
        else {
            panic!("expected pipelines edit");
        };
        assert_eq!(cmd.group_name(), Some("jupiter"));
        assert_eq!(cmd.editor_short.as_deref(), Some("nano"));
        // so does the long editor flag, now from the shared flags
        let Pipelines::Edit(cmd) = parse(&["edit", "orbit", "--editor", "vim"]).unwrap() else {
            panic!("expected pipelines edit");
        };
        assert_eq!(cmd.edit.editor.as_deref(), Some("vim"));
        assert_eq!(cmd.group_name(), None);
        // the group can also be a flag, but not both ways
        let Pipelines::Edit(cmd) = parse(&["edit", "orbit", "-g", "jupiter"]).unwrap() else {
            panic!("expected pipelines edit");
        };
        assert_eq!(cmd.group_name(), Some("jupiter"));
        assert!(parse(&["edit", "orbit", "jupiter", "-g", "jupiter"]).is_err());
        // the non-interactive sources and the result format are taken
        let Pipelines::Edit(cmd) = parse(&[
            "edit",
            "orbit",
            "--set",
            "sla=5",
            "--unset",
            "description",
            "--file",
            "f.yml",
            "--dry-run",
            "--skip-confirm",
            "--force",
            "-o",
            "yaml",
        ])
        .unwrap() else {
            panic!("expected pipelines edit");
        };
        assert!(cmd.edit.has_changes());
        assert!(cmd.edit.dry_run && cmd.edit.skip_confirm && cmd.edit.force);
        assert_eq!(cmd.edit.format, Some(OutputFormat::Yaml));
        // -e and --editor together are refused
        assert!(parse(&["edit", "orbit", "-e", "a", "--editor", "b"]).is_err());
    }

    /// `pipelines run` takes the common run flags and a parent reaction
    #[test]
    fn run_args() {
        let Pipelines::Run(cmd) = parse(&[
            "run",
            "jupiter/orbit",
            "--sample",
            "abc",
            "--tag",
            "t1",
            "--parent",
            "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d",
            "--wait",
            "--output",
            "out",
        ])
        .unwrap() else {
            panic!("expected pipelines run");
        };
        assert_eq!(cmd.pipeline, "jupiter/orbit");
        assert_eq!(cmd.common.inputs.samples, ["abc"]);
        assert_eq!(cmd.common.tag, ["t1"]);
        assert!(cmd.parent.is_some());
        assert!(cmd.common.wait);
        // the pipeline is required, and --wait conflicts with --wait-for
        assert!(parse(&["run"]).is_err());
        assert!(parse(&["run", "orbit", "--wait", "--wait-for", "status=Completed"]).is_err());
    }

    /// `pipelines delete` skips its confirmation with `--skip-confirm` or its `--yes` alias, and
    /// asks by default
    #[test]
    fn delete_skip_confirm_and_yes() {
        // the confirmation is asked unless a flag skips it
        let Pipelines::Delete(cmd) = parse(&["delete", "orbit", "-g", "corn"]).unwrap() else {
            panic!("expected pipelines delete");
        };
        assert!(!cmd.skip_confirm);
        // both spellings skip it
        for flag in ["--skip-confirm", "--yes"] {
            let Pipelines::Delete(cmd) = parse(&["delete", "orbit", "-g", "corn", flag]).unwrap()
            else {
                panic!("expected pipelines delete");
            };
            assert!(cmd.skip_confirm, "{flag} did not skip the confirmation");
        }
    }
}
