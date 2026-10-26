//! Handles run commands: listing, describing, retrying, cancelling, and reading the logs of tool,
//! workflow, and pipeline runs, starting workflow and pipeline runs, and showing a run's results

use futures::StreamExt;
use futures::stream;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thorium::client::ResultsClient;
use thorium::models::{
    AgentProvenance, EntityKinds, ModelConfigRef, OutputMap, Reaction, ReactionCursorOpts,
    ReactionModelBinding, ReactionRequest, ReactionSource, ReactionStatus, RepoDependencyRequest,
    ResultGetParams, RunInputs, SampleRequest, ToolProvenance, WorkflowResultKey,
    WorkflowRunRequest,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::repos::RepoTarget;
use crate::args::results::ShowResults;
use crate::args::runs::{
    CancelRun, DescribeRun, GetRuns, LogsRun, RetryRun, RunCommonArgs, RunInputArgs, RunKind,
    RunPipeline, Runs,
};
use crate::args::workflows::RunWorkflow;
use crate::handlers::agents::find;
use crate::handlers::instances::find_instance;
use crate::handlers::reactions::run_logs_view;
use crate::handlers::run_batch::{self, BatchOpts, Plan, print_plan, split_inputs};
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::run_wait::{self, is_finished};
use crate::utils::watch;

/// The tag every workflow run's reaction is listed under
const WORKFLOW_RUN_TAG: &str = "WorkflowRun";

/// Name the kind of run a reaction is
///
/// # Arguments
///
/// * `reaction` - The run's reaction
fn kind(reaction: &Reaction) -> &'static str {
    // name each source
    match &reaction.source {
        ReactionSource::Pipeline { .. } => "pipeline",
        ReactionSource::Tool { .. } => "tool run",
        ReactionSource::WorkflowRun(_) => "workflow run",
    }
}

/// The runs one command shows
#[derive(Serialize)]
#[serde(transparent)]
struct RunsView(
    /// The runs' reactions as the API returned them
    Vec<Reaction>,
);

impl View for RunsView {
    /// Render the runs as a table
    fn table(&self) -> String {
        // build one row per run
        let rows = self
            .0
            .iter()
            .map(|reaction| {
                // a workflow run's key shows as the instance's name, without the id it carries
                let key = match &reaction.source {
                    ReactionSource::WorkflowRun(_) => {
                        reaction.result_key().map(WorkflowResultKey::display_name)
                    }
                    _ => reaction.result_key(),
                };
                vec![
                    reaction.id.to_string(),
                    reaction.group.clone(),
                    kind(reaction).to_owned(),
                    key.unwrap_or("-").to_owned(),
                    reaction.status.to_string(),
                    reaction.creator.clone(),
                ]
            })
            .collect();
        output::table_from(
            &["ID", "GROUP", "KIND", "RESULT KEY", "STATUS", "CREATOR"],
            rows,
        )
    }

    /// Render the runs as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each run, separated by rules
        self.0
            .iter()
            .map(|reaction| describe_text(reaction, ansi))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// One run
#[derive(Serialize)]
#[serde(transparent)]
pub(crate) struct RunView(
    /// The run's reaction as the API returned it
    pub Reaction,
);

impl View for RunView {
    /// Render the run as a one row table
    fn table(&self) -> String {
        // reuse the list table
        RunsView(vec![self.0.clone()]).table()
    }

    /// Render the run as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        describe_text(&self.0, ansi)
    }
}

/// Describe a reference to group or user model configuration
///
/// # Arguments
///
/// * `kind` - What is referenced (endpoint or credential)
/// * `config` - The reference
fn config_text(kind: &str, config: ModelConfigRef) -> String {
    // say whose configuration it is
    match config {
        ModelConfigRef::Group(id) => format!("group {kind} {id}"),
        ModelConfigRef::User(id) => format!("the runner's {kind} {id}"),
    }
}

/// Describe the model one agent of a workflow run resolved to
///
/// # Arguments
///
/// * `binding` - The agent's binding
fn binding_text(binding: &ReactionModelBinding) -> String {
    // the agent, its model and protocol, and where its auth came from
    let mut parts = vec![format!(
        "  {} -> {} ({:?}) at {:?} on {}",
        binding.agent,
        binding.model_id,
        binding.api,
        binding.level,
        config_text("endpoint", binding.endpoint)
    )];
    if let Some(profile) = binding.profile {
        parts.push(format!("profile {profile}"));
    }
    parts.push(match binding.credential {
        Some(credential) => config_text("credential", credential),
        None => "keyless".to_owned(),
    });
    parts.join(", ")
}

/// Describe one agent a run's provenance records
///
/// # Arguments
///
/// * `agent` - The agent's provenance
fn agent_text(agent: &AgentProvenance) -> String {
    // the agent and the hashes of its configuration and tools
    format!(
        "  agent {} ({}): config {}, toolset {}",
        agent.agent, agent.name, agent.config_hash, agent.toolset_digest
    )
}

/// Describe one tool instance a run's provenance records
///
/// # Arguments
///
/// * `tool` - The tool instance's provenance
/// * `harness` - The harness tool instance of a workflow run, if any
fn tool_text(tool: &ToolProvenance, harness: Option<Uuid>) -> String {
    // the instance, its kind and version, its hash, and its image as written
    let marker = if harness == Some(tool.tool_instance) {
        " [harness]"
    } else {
        ""
    };
    format!(
        "  {} {} ({}{marker}): version {}, spec {}, image {}, tool {}",
        tool.kind,
        tool.tool_instance,
        tool.name,
        tool.version,
        tool.spec_hash,
        tool.image.as_deref().unwrap_or("-"),
        tool.tool
    )
}

/// Render the exact versions a run was created with
///
/// # Arguments
///
/// * `reaction` - The run's reaction
/// * `ansi` - Whether to style the text
fn provenance_lines(reaction: &Reaction, ansi: bool) -> Vec<String> {
    // a workflow run marks its harness among its tools
    let harness = match &reaction.source {
        ReactionSource::WorkflowRun(run) => Some(run.harness),
        ReactionSource::Pipeline { .. } | ReactionSource::Tool { .. } => None,
    };
    let provenance = reaction.source.provenance();
    let mut lines = vec![render::label("Provenance", ansi)];
    // a reaction created before provenance was recorded has none
    if provenance.is_empty() {
        lines.push("  -".to_owned());
        return lines;
    }
    lines.extend(provenance.agents.iter().map(agent_text));
    lines.extend(provenance.tools.iter().map(|tool| tool_text(tool, harness)));
    lines
}

/// Render a run's details as text: its source, plan, provenance, result key, and status
///
/// # Arguments
///
/// * `reaction` - The run's reaction
/// * `ansi` - Whether to style the text
fn describe_text(reaction: &Reaction, ansi: bool) -> String {
    // start with the run's id and group
    let mut lines = vec![render::header(
        &reaction.id.to_string(),
        &reaction.group,
        ansi,
    )];
    lines.push(render::field("Kind", kind(reaction), ansi));
    lines.push(render::field("Status", &reaction.status.to_string(), ansi));
    lines.push(render::field("Creator", &reaction.creator, ansi));
    // describe what the run runs
    match &reaction.source {
        ReactionSource::WorkflowRun(run) => {
            lines.push(render::field(
                "Workflow instance",
                &run.workflow_instance.to_string(),
                ansi,
            ));
            lines.push(render::field("Digest", &run.digest, ansi));
            lines.push(render::field("Entry agent", &run.agent.to_string(), ansi));
            lines.push(render::field("Harness", &run.harness.to_string(), ansi));
            lines.push(render::field("Result key", &run.result_key, ansi));
            // list the model each agent resolved to when the run was created
            lines.push(render::label("Models", ansi));
            if run.models.is_empty() {
                lines.push("  -".to_owned());
            }
            lines.extend(run.models.iter().map(binding_text));
        }
        ReactionSource::Tool { tool_instance, .. } => {
            lines.push(render::field(
                "Tool instance",
                &tool_instance.to_string(),
                ansi,
            ));
            if let Some(key) = reaction.result_key() {
                lines.push(render::field("Result key", key, ansi));
            }
        }
        ReactionSource::Pipeline { name, .. } => {
            lines.push(render::field("Pipeline", name, ansi));
        }
    }
    // list each planned stage and the tool instance it runs
    lines.push(render::label("Plan", ansi));
    for (index, step) in reaction.plan.stages.iter().enumerate() {
        for stage in step {
            lines.push(format!(
                "  {}. {} -> {}",
                index + 1,
                stage.name,
                stage.tool_instance
            ));
        }
    }
    // list the exact versions the run was created with
    lines.extend(provenance_lines(reaction, ansi));
    // show the inputs and the run's place among other reactions
    let samples = if reaction.samples.is_empty() {
        "-".to_owned()
    } else {
        reaction.samples.join(", ")
    };
    lines.push(render::field("Samples", &samples, ansi));
    let repos = if reaction.repos.is_empty() {
        "-".to_owned()
    } else {
        reaction
            .repos
            .iter()
            .map(|repo| match &repo.commitish {
                Some(commitish) => format!("{}:{commitish}", repo.url),
                None => repo.url.clone(),
            })
            .collect::<Vec<String>>()
            .join(", ")
    };
    lines.push(render::field("Repos", &repos, ansi));
    let parent = reaction
        .parent
        .map_or_else(|| "-".to_owned(), |parent| parent.to_string());
    lines.push(render::field("Parent", &parent, ansi));
    lines.join("\n")
}

/// A run's results on each of its inputs, by input
#[derive(Serialize)]
#[serde(transparent)]
struct RunResultsView(
    /// The results stored under the run's key, by input file SHA256 or repo url
    BTreeMap<String, OutputMap>,
);

impl View for RunResultsView {
    /// Render the results as a table of inputs and how many results each has
    fn table(&self) -> String {
        // one row per input
        let rows = self
            .0
            .iter()
            .map(|(input, results)| {
                let count = results.results.values().map(Vec::len).sum::<usize>();
                vec![input.clone(), count.to_string()]
            })
            .collect();
        output::table_from(&["INPUT", "RESULTS"], rows)
    }

    /// Render the results as text, one input at a time
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // show each input's results as pretty JSON
        let mut lines = Vec::new();
        for (input, results) in &self.0 {
            lines.push(render::label(input, ansi));
            let rendered = serde_json::to_string_pretty(&results.results)
                .unwrap_or_else(|err| format!("unable to render results: {err}"));
            lines.push(rendered);
        }
        lines.join("\n")
    }
}

/// Find a run's reaction in a group, or in all of the user's groups
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `id` - The id of the run's reaction
/// * `group` - The group the run is in, if known
pub async fn find_run(thorium: &Thorium, id: Uuid, group: Option<&str>) -> Result<Reaction, Error> {
    // read the run from its group, or search for it
    match group {
        Some(group) => thorium.reactions.get(group, id).await,
        None => utils::reactions::find_reaction_no_group(thorium, &id).await,
    }
}

/// A run's inputs once its lists are read and its repos parsed, before any file is uploaded
#[derive(Debug, Clone, Default)]
pub struct PlannedInputs {
    /// The files and repos the run is given
    pub inputs: RunInputs,
    /// The local files that still need uploading, to be run on once they are
    pub files: Vec<PathBuf>,
}

/// Read a run's inputs without uploading or creating anything
///
/// A missing local file or list file is refused here, so a dry run finds the same mistakes a run
/// does.
///
/// # Arguments
///
/// * `args` - The files and repos given on the command line
pub async fn plan_inputs(args: &RunInputArgs) -> Result<PlannedInputs, Error> {
    // make sure every local file exists before anything is uploaded
    for path in &args.files {
        if !path.is_file() {
            return Err(Error::new(format!(
                "{} doesn't exist or isn't a file",
                path.display()
            )));
        }
    }
    // stdin can only be read for one list
    if args.sample_list.as_deref() == Some(Path::new("-"))
        && args.repo_list.as_deref() == Some(Path::new("-"))
    {
        return Err(Error::new(
            "--sample-list and --repo-list can't both read stdin",
        ));
    }
    // read the samples and repos named in list files
    let mut samples = args.samples.clone();
    if let Some(list) = &args.sample_list {
        samples.extend(run_wait::list_entries(&read_list(list).await?, true));
    }
    let mut repos = args.repos.clone();
    if let Some(list) = &args.repo_list {
        repos.extend(run_wait::list_entries(&read_list(list).await?, false));
    }
    // parse each repo the way reactions create does
    let mut inputs = RunInputs::default();
    for raw in &repos {
        let target = RepoTarget::try_from(raw)?;
        inputs = inputs.repo(RepoDependencyRequest::from(target));
    }
    // run on each given file
    for sample in &samples {
        inputs = inputs.sample(sample);
    }
    Ok(PlannedInputs {
        inputs,
        files: args.files.clone(),
    })
}

/// Build a run's inputs, uploading any local files first
///
/// A missing local file or list file is refused before anything is uploaded.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `args` - The files and repos given on the command line
/// * `groups` - The groups to upload local files to
pub async fn run_inputs(
    thorium: &Thorium,
    args: &RunInputArgs,
    groups: &[String],
) -> Result<RunInputs, Error> {
    // read the lists and check the local files
    let planned = plan_inputs(args).await?;
    let mut inputs = planned.inputs;
    // upload each local file and run on it
    for path in &planned.files {
        let req = SampleRequest::new(path, groups.to_vec());
        let created = thorium.files.create(req).await?;
        inputs = inputs.sample(created.sha256);
    }
    Ok(inputs)
}

/// Read a list file of run inputs, or stdin when the path is `-`
///
/// # Arguments
///
/// * `path` - The list file to read
async fn read_list(path: &Path) -> Result<String, Error> {
    // `-` reads the list from stdin
    if path == Path::new("-") {
        return read_list_from(tokio::io::stdin()).await;
    }
    // name the file in the error when it can't be read
    tokio::fs::read_to_string(path)
        .await
        .map_err(|err| Error::new(format!("Unable to read {}: {err}", path.display())))
}

/// Read a list of run inputs from a reader
///
/// # Arguments
///
/// * `reader` - Where to read the list from
async fn read_list_from<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Result<String, Error> {
    // read everything the reader has
    let mut text = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut text)
        .await
        .map_err(|err| Error::new(format!("Unable to read the list from stdin: {err}")))?;
    Ok(text)
}

/// Get the tags to give a run: its own and any extra ones
///
/// # Arguments
///
/// * `common` - The run command's flags
/// * `extra` - Tags to add, such as the batch name
fn run_tags(common: &RunCommonArgs, extra: &[String]) -> Vec<String> {
    // the tags the user gave come first
    let mut tags = common.tag.clone();
    tags.extend(extra.iter().cloned());
    tags
}

/// Start one workflow run and get its reaction id
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `instance` - The workflow instance to run
/// * `common` - The run command's flags
/// * `inputs` - The inputs of this run
/// * `extra` - Tags to add to the run's own
async fn start_workflow(
    thorium: &Thorium,
    instance: Uuid,
    common: &RunCommonArgs,
    inputs: RunInputs,
    extra: Vec<String>,
) -> Result<Uuid, Error> {
    // build the run request
    let mut req = WorkflowRunRequest::new(inputs);
    req.group.clone_from(&common.group);
    req.tags = run_tags(common, &extra);
    req.sla = common.sla;
    // start the run
    Ok(thorium.reactions.run_workflow(instance, &req).await?.id)
}

/// Start a workflow run, or one run per input, and follow it as the command's flags ask
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The run command to execute
/// * `opts` - How a batch of runs is started and printed
pub async fn run_workflow(
    thorium: &Thorium,
    cmd: &RunWorkflow,
    opts: &BatchOpts,
) -> Result<(), Error> {
    // find the workflow instance
    let instance = find_instance(thorium, &cmd.instance).await?;
    if instance.kind != EntityKinds::WorkflowInstance {
        return Err(Error::new(format!(
            "{} is a {}, not a workflow instance",
            cmd.instance, instance.kind
        )));
    }
    let common = &cmd.common;
    // a dry run shows what would start and stops
    if common.dry_run {
        let planned = plan_inputs(&common.inputs).await?;
        let plan = Plan::new(
            "workflow",
            &cmd.instance,
            common,
            &instance.groups,
            &planned,
        );
        return print_plan(&plan, common.format.format);
    }
    // upload local files to the run group, or the instance's groups when none was given
    let groups = match &common.group {
        Some(group) => vec![group.clone()],
        None => instance.groups.clone(),
    };
    let inputs = run_inputs(thorium, &common.inputs, &groups).await?;
    // start one run per input and follow the batch
    if common.one_per_input {
        let start = |inputs, extra| start_workflow(thorium, instance.id, common, inputs, extra);
        return run_batch::run_batch(thorium, common, opts, split_inputs(&inputs), start).await;
    }
    // start the run, tagged with the batch name if one was given
    let extra: Vec<String> = common.batch.iter().cloned().collect();
    let id = start_workflow(thorium, instance.id, common, inputs, extra).await?;
    // follow the run in the group it was created in
    let group = match &common.group {
        Some(group) => group.clone(),
        None if common.follows() => find_run(thorium, id, None).await?.group,
        None => String::new(),
    };
    run_wait::follow(thorium, &group, id, common).await
}

/// Start one pipeline run and get its reaction id
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The run command to execute
/// * `group` - The pipeline's group
/// * `pipeline` - The pipeline's name
/// * `inputs` - The inputs of this run
/// * `extra` - Tags to add to the run's own
async fn start_pipeline(
    thorium: &Thorium,
    cmd: &RunPipeline,
    group: &str,
    pipeline: &str,
    inputs: RunInputs,
    extra: Vec<String>,
) -> Result<Uuid, Error> {
    let common = &cmd.common;
    // build the reaction request
    let mut req = ReactionRequest::new(group, pipeline)
        .samples(inputs.samples)
        .tags(run_tags(common, &extra));
    for repo in inputs.repos {
        req = req.repo(repo);
    }
    if let Some(sla) = common.sla {
        req = req.sla(sla);
    }
    if let Some(parent) = cmd.parent {
        req = req.parent(parent);
    }
    // create the reaction
    Ok(thorium.reactions.create(&req).await?.id)
}

/// Start a pipeline run, or one run per input, and follow it as the command's flags ask
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The run command to execute
/// * `opts` - How a batch of runs is started and printed
pub async fn run_pipeline(
    thorium: &Thorium,
    cmd: &RunPipeline,
    opts: &BatchOpts,
) -> Result<(), Error> {
    let common = &cmd.common;
    // a pipeline may be named `<group>/<name>`
    let (named_group, pipeline) = match cmd.pipeline.split_once('/') {
        Some((group, name)) => (Some(group.to_owned()), name.to_owned()),
        None => (None, cmd.pipeline.clone()),
    };
    // find the pipeline's group when none was given
    let group = match common.group.clone().or(named_group) {
        Some(group) => group,
        None => utils::pipelines::find_pipeline_group(thorium, &pipeline).await?,
    };
    // a dry run shows what would start and stops
    if common.dry_run {
        let planned = plan_inputs(&common.inputs).await?;
        let mut plan = Plan::new("pipeline", &pipeline, common, &[], &planned);
        plan.group = Some(group);
        return print_plan(&plan, common.format.format);
    }
    // upload local files to the pipeline's group and gather the inputs
    let inputs = run_inputs(thorium, &common.inputs, std::slice::from_ref(&group)).await?;
    // start one run per input and follow the batch
    if common.one_per_input {
        let start = |inputs, extra| start_pipeline(thorium, cmd, &group, &pipeline, inputs, extra);
        return run_batch::run_batch(thorium, common, opts, split_inputs(&inputs), start).await;
    }
    // create the reaction, tagged with the batch name if one was given, and follow it
    let extra: Vec<String> = common.batch.iter().cloned().collect();
    let id = start_pipeline(thorium, cmd, &group, &pipeline, inputs, extra).await?;
    run_wait::follow(thorium, &group, id, common).await
}

/// Whether a reaction is a run of one kind
///
/// # Arguments
///
/// * `reaction` - The reaction to check
/// * `kind` - The kind of run to look for
fn is_kind(reaction: &Reaction, kind: RunKind) -> bool {
    // match the reaction's source against the kind
    matches!(
        (&reaction.source, kind),
        (ReactionSource::Tool { .. }, RunKind::Tool)
            | (ReactionSource::WorkflowRun(_), RunKind::Workflow)
            | (ReactionSource::Pipeline { .. }, RunKind::Pipeline)
    )
}

/// List the runs of one group that are not workflow runs listed by tag
///
/// A tool run or pipeline reaction has no tag shared by every run of its kind, so each status of
/// the group is read page by page and the reactions of the wanted kind are kept until there are
/// enough.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group to list
/// * `kind` - The kind of run to keep
/// * `status` - The only status to keep, or every status when none is given
/// * `limit` - The most runs to keep
async fn list_kind(
    thorium: &Thorium,
    group: &str,
    kind: RunKind,
    status: Option<&ReactionStatus>,
    limit: usize,
) -> Result<Vec<Reaction>, Error> {
    // read each wanted status's reactions and keep the ones of this kind
    let mut runs = Vec::new();
    for wanted in [
        ReactionStatus::Created,
        ReactionStatus::Started,
        ReactionStatus::Completed,
        ReactionStatus::Failed,
    ] {
        if status.is_some_and(|only| *only != wanted) {
            continue;
        }
        // read pages of this status until enough runs of the kind are kept or it runs out
        let mut cursor = thorium
            .reactions
            .list_group(group, &wanted)
            .page_size(100)
            .details();
        while runs.len() < limit && !cursor.exhausted {
            cursor.next().await?;
            runs.extend(
                cursor
                    .details
                    .drain(..)
                    .filter(|reaction| is_kind(reaction, kind)),
            );
        }
    }
    // never return more than the limit
    runs.truncate(limit);
    Ok(runs)
}

/// Read every run in a group that has a tag
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group to list
/// * `tag` - The tag the runs are listed under
async fn list_tagged(thorium: &Thorium, group: &str, tag: &str) -> Result<Vec<Reaction>, Error> {
    // read each page of the tag's reactions
    let mut cursor = thorium
        .reactions
        .list_tag(group, tag)
        .page_size(100)
        .details();
    let mut runs = Vec::new();
    while !cursor.exhausted {
        cursor.next().await?;
        runs.append(&mut cursor.details);
    }
    Ok(runs)
}

/// List the workflow runs of one group under a tag, keeping the ones with a status
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group to list
/// * `tag` - The tag the workflow runs are listed under
/// * `status` - The only status to keep, or every status when none is given
/// * `limit` - The most runs to keep
async fn list_workflow_runs(
    thorium: &Thorium,
    group: &str,
    tag: &str,
    status: Option<&ReactionStatus>,
    limit: usize,
) -> Result<Vec<Reaction>, Error> {
    // a status is checked on each run, so only an unfiltered listing is limited by the server
    let mut opts = ReactionCursorOpts::default().group(group);
    if status.is_none() {
        opts = opts.limit(limit);
    }
    let mut cursor = thorium.reactions.list_tag_new(tag, &opts).await?;
    let mut runs = Vec::new();
    // read pages of run ids until enough runs are kept or the listing runs out
    loop {
        // read each run on this page, keeping the ones with the status
        for id in std::mem::take(&mut cursor.data) {
            if runs.len() >= limit {
                return Ok(runs);
            }
            let id = Uuid::parse_str(&id).map_err(|err| Error::new(err.to_string()))?;
            let run = thorium.reactions.get(group, id).await?;
            if status.is_none_or(|status| run.status == *status) {
                runs.push(run);
            }
        }
        // stop once the listing has no more run ids
        if runs.len() >= limit || cursor.exhausted() {
            return Ok(runs);
        }
        cursor.refill().await?;
    }
}

/// List runs
///
/// Runs are listed per group and can be narrowed by kind, status, workflow, or batch name; a
/// workflow filter only applies to workflow runs.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetRuns) -> Result<(), Error> {
    // a workflow filter only makes sense for workflow runs
    if cmd.workflow.is_some() && cmd.kind.is_some_and(|kind| kind != RunKind::Workflow) {
        return Err(Error::new("--workflow only lists workflow runs"));
    }
    // list one workflow's runs or every workflow run
    let tag = match &cmd.workflow {
        Some(workflow) => {
            let found = find(
                thorium,
                EntityKinds::WorkflowCatalog,
                "a workflow",
                workflow,
            )
            .await?;
            format!("Workflow={}", found.id)
        }
        None => WORKFLOW_RUN_TAG.to_owned(),
    };
    // list the requested groups or all of the user's groups
    let groups = if cmd.group.is_empty() {
        utils::groups::get_all_groups(thorium).await?
    } else {
        cmd.group.clone()
    };
    // tool and pipeline runs are found by their source, workflow runs by their tag
    let other = cmd.kind.filter(|kind| *kind != RunKind::Workflow);
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        let mut runs = Vec::new();
        for group in &groups {
            // a batch lists the runs under its name, of any kind
            if let Some(batch) = &cmd.batch {
                let mut found = list_tagged(thorium, group, batch).await?;
                found.retain(|run| cmd.kind.is_none_or(|kind| is_kind(run, kind)));
                found.retain(|run| {
                    cmd.status
                        .as_ref()
                        .is_none_or(|status| run.status == *status)
                });
                found.truncate(cmd.limit);
                runs.extend(found);
                continue;
            }
            if let Some(kind) = other {
                runs.extend(list_kind(thorium, group, kind, cmd.status.as_ref(), cmd.limit).await?);
                continue;
            }
            // list this group's workflow runs with the status
            runs.extend(
                list_workflow_runs(thorium, group, &tag, cmd.status.as_ref(), cmd.limit).await?,
            );
        }
        Ok(RunsView(runs))
    })
    .await
}

/// Describe a run
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeRun) -> Result<(), Error> {
    // describe the run in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        Ok(RunView(
            find_run(thorium, cmd.reaction, cmd.group.as_deref()).await?,
        ))
    })
    .await
}

/// Retry a failed workflow run and print the new run's reaction id
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The retry command to execute
async fn retry(thorium: &Thorium, cmd: &RetryRun) -> Result<(), Error> {
    // find the failed run and the workflow instance it ran
    let failed = find_run(thorium, cmd.reaction, cmd.group.as_deref()).await?;
    let ReactionSource::WorkflowRun(run) = &failed.source else {
        return Err(Error::new(format!(
            "{} is a {}, not a workflow run",
            failed.id,
            kind(&failed)
        )));
    };
    // retry it and print the new run's reaction id
    let created = thorium
        .reactions
        .retry_workflow_run(run.workflow_instance, failed.id)
        .await?;
    println!("{}", created.id);
    Ok(())
}

/// Print a run's status updates and stage logs
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The logs command to execute
async fn logs(thorium: &Thorium, cmd: &LogsRun) -> Result<(), Error> {
    // print the logs as text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, None, || async {
        let reaction = find_run(thorium, cmd.reaction, cmd.group.as_deref()).await?;
        run_logs_view(thorium, reaction, cmd.stage.as_deref()).await
    })
    .await
}

/// Find the unfinished runs a bulk cancel names
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The cancel command to execute
async fn select_unfinished(thorium: &Thorium, cmd: &CancelRun) -> Result<Vec<Reaction>, Error> {
    // only runs that have not finished can be cancelled
    if cmd.status.as_ref().is_some_and(is_finished) {
        return Err(Error::new(
            "--status must be Created or Started: only unfinished runs can be cancelled",
        ));
    }
    // a workflow's runs carry the workflow's tag
    let workflow_tag = match &cmd.workflow {
        Some(workflow) => {
            let found = find(
                thorium,
                EntityKinds::WorkflowCatalog,
                "a workflow",
                workflow,
            )
            .await?;
            Some(format!("Workflow={}", found.id))
        }
        None => None,
    };
    // list by the batch name when there is one, and by the workflow's tag otherwise
    let tag = match (&cmd.batch, &workflow_tag) {
        (Some(batch), _) => batch.clone(),
        (None, Some(tag)) => tag.clone(),
        (None, None) => return Err(Error::new("name a run, --batch, or --workflow")),
    };
    // search the given group or all of the user's groups
    let groups = match &cmd.group {
        Some(group) => vec![group.clone()],
        None => utils::groups::get_all_groups(thorium).await?,
    };
    let mut selected = Vec::new();
    for group in &groups {
        // a batch narrowed to a workflow keeps only that workflow's runs
        for run in list_tagged(thorium, group, &tag).await? {
            let in_workflow = workflow_tag
                .as_ref()
                .is_none_or(|tag| run.tags.contains(tag));
            let has_status = cmd
                .status
                .as_ref()
                .is_none_or(|status| run.status == *status);
            if in_workflow && has_status && !is_finished(&run.status) {
                selected.push(run);
            }
        }
    }
    Ok(selected)
}

/// Cancel every unfinished run a batch name or workflow names
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The cancel command to execute
/// * `workers` - The most runs to cancel at once
/// * `asker` - Where the confirmation answer comes from
async fn cancel_selected<A: utils::Asker>(
    thorium: &Thorium,
    cmd: &CancelRun,
    workers: usize,
    asker: &A,
) -> Result<(), Error> {
    // find the runs to cancel
    let runs = select_unfinished(thorium, cmd).await?;
    if runs.is_empty() {
        println!("No unfinished runs match; nothing to cancel");
        return Ok(());
    }
    // show what would be failed and stop
    if cmd.dry_run {
        println!("Would fail {} runs:", runs.len());
        for run in &runs {
            println!("  {} ({}) in {}", run.id, run.status, run.group);
        }
        return Ok(());
    }
    // failing runs can't be undone, so confirm unless told not to
    let question = format!("Fail {} runs?", runs.len());
    if !utils::confirm_or_skip(asker, &question, "--skip-confirm", cmd.skip_confirm)? {
        return Ok(());
    }
    // fail the runs a few at a time
    let total = runs.len();
    let results: Vec<(Reaction, Result<ReactionStatus, Error>)> = stream::iter(runs)
        .map(|run| async move {
            let failed = thorium.reactions.fail(&run.group, &run.id).await;
            (run, failed.map(|handled| handled.status))
        })
        .buffer_unordered(workers.max(1))
        .collect()
        .await;
    // report each run and count the ones that could not be failed
    let mut errors = 0;
    for (run, result) in &results {
        match result {
            Ok(status) => println!("Run {} is now {status}", run.id),
            Err(error) => {
                errors += 1;
                eprintln!("Unable to fail run {}: {error}", run.id);
            }
        }
    }
    println!("Failed {} of {total} runs", total - errors);
    if errors > 0 {
        return Err(Error::new(format!("{errors} runs could not be failed")));
    }
    Ok(())
}

/// Cancel a run, or every unfinished run of a batch or workflow, by failing it
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The cancel command to execute
/// * `workers` - The most runs to cancel at once
pub async fn cancel(thorium: &Thorium, cmd: &CancelRun, workers: usize) -> Result<(), Error> {
    // ask on the real terminal
    cancel_asking(thorium, cmd, workers, &utils::Terminal).await
}

/// Cancel a run, or every unfinished run of a batch or workflow, confirming with `asker`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The cancel command to execute
/// * `workers` - The most runs to cancel at once
/// * `asker` - Where the confirmation answer comes from
pub async fn cancel_asking<A: utils::Asker>(
    thorium: &Thorium,
    cmd: &CancelRun,
    workers: usize,
    asker: &A,
) -> Result<(), Error> {
    // a batch or workflow names many runs
    let Some(id) = cmd.reaction else {
        return cancel_selected(thorium, cmd, workers, asker).await;
    };
    // find the run
    let reaction = find_run(thorium, id, cmd.group.as_deref()).await?;
    // a finished run has nothing left to cancel
    if is_finished(&reaction.status) {
        println!(
            "Run {} is already {}; nothing to cancel",
            reaction.id, reaction.status
        );
        return Ok(());
    }
    // show what would be failed and stop
    if cmd.dry_run {
        println!(
            "Would fail run {} ({}) in {}",
            reaction.id, reaction.status, reaction.group
        );
        return Ok(());
    }
    // failing a run can't be undone, so confirm unless told not to
    let question = format!("Fail run {} ({})?", reaction.id, reaction.status);
    if !utils::confirm_or_skip(asker, &question, "--skip-confirm", cmd.skip_confirm)? {
        return Ok(());
    }
    // fail the run
    let handled = thorium
        .reactions
        .fail(&reaction.group, &reaction.id)
        .await?;
    println!("Run {} is now {}", reaction.id, handled.status);
    Ok(())
}

/// Show a run's results on each of its input files and repos
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The show command to execute
pub async fn show_results(thorium: &Thorium, cmd: &ShowResults) -> Result<(), Error> {
    // find the run and the key its results are stored under
    let reaction = find_run(thorium, cmd.reaction, cmd.group.as_deref()).await?;
    let Some(key) = reaction.result_key() else {
        return Err(Error::new(format!(
            "{} is a pipeline reaction, not a tool or workflow run",
            reaction.id
        )));
    };
    // ask for hidden results too; Hidden only marks a result as not shown by default in the UI
    // and this command explicitly asks for the run's own result
    let params = ResultGetParams::default().tool(key).hidden();
    // read the results on each input file and repo
    let mut results = BTreeMap::new();
    for sha256 in &reaction.samples {
        let found = thorium.files.get_results(sha256, &params).await?;
        results.insert(sha256.clone(), found);
    }
    for repo in &reaction.repos {
        let found = thorium.repos.get_results(&repo.url, &params).await?;
        results.insert(repo.url.clone(), found);
    }
    output::print(
        &RunResultsView(results),
        cmd.format.unwrap_or(OutputFormat::Human),
    )
}

/// Handle all run commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The runs command to execute
pub async fn handle(args: &Args, cmd: &Runs) -> Result<(), Error> {
    // load our config and instance our client
    let (_, thorium) = utils::get_client(args).await?;
    // call the right runs handler
    let result = match cmd {
        Runs::Get(cmd) => get(&thorium, cmd).await,
        Runs::Describe(cmd) => describe(&thorium, cmd).await,
        Runs::Retry(cmd) => retry(&thorium, cmd).await,
        Runs::Logs(cmd) => logs(&thorium, cmd).await,
        Runs::Cancel(cmd) => cancel(&thorium, cmd, args.workers).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::{Value, json};
    use thorium::models::ReactionStatus;
    use uuid::Uuid;

    use super::{
        cancel, cancel_asking, list_kind, list_workflow_runs, plan_inputs, read_list_from,
        run_pipeline, show_results,
    };
    use crate::args::results::ShowResults;
    use crate::args::runs::RunKind;
    use crate::args::runs::{CancelRun, RunInputArgs, RunPipeline};
    use crate::handlers::reactions::retag::retag;
    use crate::handlers::run_batch::BatchOpts;
    use crate::utils::run_wait;
    use crate::utils::run_wait::testing::{client, mock, reaction};
    use crate::utils::testing::Scripted;
    use std::path::PathBuf;
    use thorium::Error;

    /// A pipeline run creates one reaction carrying the run flags and exits without waiting
    #[tokio::test]
    async fn pipeline_run_creates_a_reaction_with_its_flags() {
        let parent = Uuid::new_v4();
        let created = Uuid::new_v4();
        let (host, requests) = mock(move |line, _| {
            if line.starts_with("POST /api/reactions/") {
                (200, json!({"id": created}).to_string())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let cmd = RunPipeline::try_parse_from([
            "run",
            "static/harvest",
            "--sample",
            "aa",
            "--sample-list",
            "/nonexistent-list",
            "--tag",
            "t1",
            "--sla",
            "30",
            "--parent",
            &parent.to_string(),
        ])
        .unwrap();
        // a list file that can't be read is refused before any request
        let err = run_pipeline(&thorium, &cmd, &BatchOpts::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("/nonexistent-list"), "{err}");
        assert!(requests.lock().unwrap().is_empty());
        // without the list the reaction is created from the flags
        let cmd = RunPipeline::try_parse_from([
            "run",
            "static/harvest",
            "--sample",
            "aa",
            "--tag",
            "t1",
            "--sla",
            "30",
            "--parent",
            &parent.to_string(),
        ])
        .unwrap();
        run_pipeline(&thorium, &cmd, &BatchOpts::default())
            .await
            .unwrap();
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 1, "{sent:?}");
        let body: Value = serde_json::from_str(&sent[0].1).unwrap();
        assert_eq!(body["group"], "static");
        assert_eq!(body["pipeline"], "harvest");
        assert_eq!(body["samples"], json!(["aa"]));
        assert_eq!(body["tags"], json!(["t1"]));
        assert_eq!(body["sla"], 30);
        assert_eq!(body["parent"], json!(parent));
    }

    /// Cancelling fails only an unfinished run, and a dry run or a finished run sends nothing
    #[tokio::test]
    async fn cancel_fails_only_unfinished_runs() {
        let id = Uuid::new_v4();
        let status = std::sync::Arc::new(std::sync::Mutex::new(ReactionStatus::Started));
        let current = status.clone();
        let (host, requests) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/corn/") {
                let run = reaction(id, current.lock().unwrap().clone());
                (200, serde_json::to_string(&run).unwrap())
            } else if line.contains("/handle/corn/") && line.contains("/fail") {
                (200, json!({"status": "Failed"}).to_string())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let parse = |flags: &[&str]| {
            let mut argv = vec!["cancel", &id.to_string()]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            argv.extend(flags.iter().map(|flag| (*flag).to_owned()));
            CancelRun::try_parse_from(argv).unwrap()
        };
        let count = |needle: &str| {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(line, _)| line.contains(needle))
                .count()
        };
        // a dry run reads the run and fails nothing
        cancel(&thorium, &parse(&["-g", "corn", "--dry-run"]), 2)
            .await
            .unwrap();
        assert_eq!(count("/fail"), 0);
        // an unfinished run is failed
        cancel(&thorium, &parse(&["-g", "corn", "--skip-confirm"]), 2)
            .await
            .unwrap();
        assert_eq!(count("/fail"), 1);
        // a finished run is left alone
        *status.lock().unwrap() = ReactionStatus::Completed;
        cancel(&thorium, &parse(&["-g", "corn", "--yes"]), 2)
            .await
            .unwrap();
        assert_eq!(count("/fail"), 1);
    }

    /// Changing a reaction's tags sends the added and removed tags
    #[tokio::test]
    async fn retag_sends_the_tag_changes() {
        let id = Uuid::new_v4();
        let (host, requests) = mock(move |line, _| {
            let mut run = reaction(id, ReactionStatus::Started);
            run.tags = vec!["kept".to_owned(), "new".to_owned()];
            if line.starts_with("GET /api/reactions/corn/")
                || line.starts_with("PATCH /api/reactions/corn/")
            {
                (200, serde_json::to_string(&run).unwrap())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let cmd = crate::args::reactions::UpdateReaction::try_parse_from([
            "update",
            &id.to_string(),
            "-g",
            "corn",
            "--add-tag",
            "new",
            "--remove-tag",
            "old",
        ])
        .unwrap();
        retag(&thorium, &cmd).await.unwrap();
        let sent = requests.lock().unwrap();
        let patch = sent
            .iter()
            .find(|(line, _)| line.starts_with("PATCH"))
            .unwrap();
        let body: Value = serde_json::from_str(&patch.1).unwrap();
        assert_eq!(body["add_tags"], json!(["new"]));
        assert_eq!(body["remove_tags"], json!(["old"]));
        // a change needs at least one tag
        assert!(
            crate::args::reactions::UpdateReaction::try_parse_from(["update", &id.to_string()])
                .is_err()
        );
    }

    /// Build the arguments to cancel runs by filter in the corn group
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags after `cancel`
    fn cancel_by(flags: &[&str]) -> CancelRun {
        let mut argv = vec!["cancel", "-g", "corn"];
        argv.extend_from_slice(flags);
        CancelRun::try_parse_from(argv).unwrap()
    }

    /// Start a mock that lists the runs of a batch and fails them on request
    ///
    /// # Arguments
    ///
    /// * `runs` - The statuses of the batch's runs
    /// * `broken` - A run whose failing returns a server error
    async fn batch_mock(
        runs: Vec<(Uuid, ReactionStatus)>,
        broken: Option<Uuid>,
    ) -> (String, crate::utils::run_wait::testing::Requests) {
        mock(move |line, _| {
            if line.starts_with("GET /api/reactions/tag/corn/nightly/") {
                let details: Vec<Value> = runs
                    .iter()
                    .map(|(id, status)| {
                        serde_json::to_value(reaction(*id, status.clone())).unwrap()
                    })
                    .collect();
                (200, json!({"cursor": null, "details": details}).to_string())
            } else if line.contains("/handle/corn/") && line.contains("/fail") {
                match broken {
                    Some(id) if line.contains(&id.to_string()) => {
                        (500, r#"{"error":"x"}"#.to_owned())
                    }
                    _ => (200, json!({"status": "Failed"}).to_string()),
                }
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await
    }

    /// Cancelling by batch fails only the unfinished runs, and a dry run fails none
    #[tokio::test]
    async fn cancel_by_batch_fails_only_unfinished_runs() {
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let runs = vec![
            (ids[0], ReactionStatus::Started),
            (ids[1], ReactionStatus::Created),
            (ids[2], ReactionStatus::Completed),
            (ids[3], ReactionStatus::Failed),
        ];
        let (host, requests) = batch_mock(runs, None).await;
        let thorium = client(&host).await;
        let failed = |needle: &str| {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(line, _)| line.contains("/fail") && line.contains(needle))
                .count()
        };
        // a dry run lists the batch and fails nothing
        cancel(
            &thorium,
            &cancel_by(&["--batch", "nightly", "--dry-run"]),
            2,
        )
        .await
        .unwrap();
        assert_eq!(failed(""), 0);
        // the unfinished runs are failed and the finished ones left alone
        cancel(
            &thorium,
            &cancel_by(&["--batch", "nightly", "--skip-confirm"]),
            2,
        )
        .await
        .unwrap();
        assert_eq!(failed(&ids[0].to_string()), 1);
        assert_eq!(failed(&ids[1].to_string()), 1);
        assert_eq!(failed(&ids[2].to_string()) + failed(&ids[3].to_string()), 0);
        // a status filter narrows it to one run
        cancel(
            &thorium,
            &cancel_by(&["--batch", "nightly", "--status", "started", "--yes"]),
            2,
        )
        .await
        .unwrap();
        assert_eq!(failed(&ids[0].to_string()), 2);
        assert_eq!(failed(&ids[1].to_string()), 1);
        // a finished status can't be cancelled
        let err = cancel(
            &thorium,
            &cancel_by(&["--batch", "nightly", "--status", "completed", "--yes"]),
            2,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unfinished"), "{err}");
    }

    /// A batch with nothing unfinished cancels nothing, and a cancel that fails for one run says so
    #[tokio::test]
    async fn cancel_by_batch_reports_runs_it_could_not_fail() {
        // nothing to cancel is not an error and sends no fail
        let done = vec![(Uuid::new_v4(), ReactionStatus::Completed)];
        let (host, requests) = batch_mock(done, None).await;
        let thorium = client(&host).await;
        cancel(&thorium, &cancel_by(&["--batch", "nightly", "--yes"]), 2)
            .await
            .unwrap();
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|(line, _)| !line.contains("/fail"))
        );
        // one run that can't be failed makes the command fail after trying the others
        let (good, bad) = (Uuid::new_v4(), Uuid::new_v4());
        let runs = vec![
            (good, ReactionStatus::Started),
            (bad, ReactionStatus::Started),
        ];
        let (host, requests) = batch_mock(runs, Some(bad)).await;
        let thorium = client(&host).await;
        let err = cancel(&thorium, &cancel_by(&["--batch", "nightly", "--yes"]), 1)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("1 runs could not be failed"),
            "{err}"
        );
        let fails = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _)| line.contains("/fail"))
            .count();
        assert_eq!(fails, 2);
    }

    /// Count the fail requests a mock API received
    ///
    /// # Arguments
    ///
    /// * `requests` - The mock's request log
    fn fails(requests: &crate::utils::run_wait::testing::Requests) -> usize {
        // only the fail calls count
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _)| line.contains("/fail"))
            .count()
    }

    /// Answering no to cancelling one run fails nothing and is not an error; yes fails it
    #[tokio::test]
    async fn cancel_one_run_asks_and_honors_the_answer() {
        let id = Uuid::new_v4();
        let (host, requests) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/corn/") {
                let run = reaction(id, ReactionStatus::Started);
                (200, serde_json::to_string(&run).unwrap())
            } else if line.contains("/handle/corn/") && line.contains("/fail") {
                (200, json!({"status": "Failed"}).to_string())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let cmd = CancelRun::try_parse_from(["cancel", &id.to_string(), "-g", "corn"]).unwrap();
        // no: nothing is failed and the command succeeds
        let no = Scripted::answering(false);
        cancel_asking(&thorium, &cmd, 2, &no).await.unwrap();
        assert_eq!(no.asked.borrow().len(), 1);
        assert_eq!(fails(&requests), 0);
        // yes: the run is failed
        let yes = Scripted::answering(true);
        cancel_asking(&thorium, &cmd, 2, &yes).await.unwrap();
        assert_eq!(fails(&requests), 1);
        // no terminal: refused, naming the flag, and nothing is failed
        let none = Scripted::no_terminal();
        let err = cancel_asking(&thorium, &cmd, 2, &none).await.unwrap_err();
        assert!(err.to_string().contains("--skip-confirm"), "{err}");
        assert_eq!(fails(&requests), 1);
    }

    /// Answering no to cancelling a batch fails nothing and is not an error; no terminal refuses
    #[tokio::test]
    async fn cancel_a_batch_asks_and_honors_the_answer() {
        let runs = vec![
            (Uuid::new_v4(), ReactionStatus::Started),
            (Uuid::new_v4(), ReactionStatus::Created),
        ];
        let (host, requests) = batch_mock(runs, None).await;
        let thorium = client(&host).await;
        let cmd = cancel_by(&["--batch", "nightly"]);
        // no: nothing is failed and the question names the count
        let no = Scripted::answering(false);
        cancel_asking(&thorium, &cmd, 2, &no).await.unwrap();
        assert!(no.asked.borrow()[0].contains('2'), "{:?}", no.asked);
        assert_eq!(fails(&requests), 0);
        // no terminal: refused, naming the flag
        let none = Scripted::no_terminal();
        let err = cancel_asking(&thorium, &cmd, 2, &none).await.unwrap_err();
        assert!(err.to_string().contains("--skip-confirm"), "{err}");
        assert_eq!(fails(&requests), 0);
        // yes: both runs are failed
        let yes = Scripted::answering(true);
        cancel_asking(&thorium, &cmd, 2, &yes).await.unwrap();
        assert_eq!(fails(&requests), 2);
    }

    /// Cancelling needs a run, a batch, or a workflow, and a run excludes the filters
    #[test]
    fn cancel_needs_a_target() {
        assert!(CancelRun::try_parse_from(["cancel"]).is_err());
        let id = Uuid::new_v4().to_string();
        assert!(CancelRun::try_parse_from(["cancel", &id]).is_ok());
        assert!(CancelRun::try_parse_from(["cancel", "--batch", "b"]).is_ok());
        assert!(CancelRun::try_parse_from(["cancel", "--workflow", "w", "--batch", "b"]).is_ok());
        assert!(CancelRun::try_parse_from(["cancel", &id, "--batch", "b"]).is_err());
        assert!(CancelRun::try_parse_from(["cancel", &id, "--status", "started"]).is_err());
    }

    /// Lists read from stdin, and only one list may use it
    #[tokio::test]
    async fn lists_are_read_from_stdin() {
        // the reader's text is returned whole
        let text = read_list_from(&b"aa\nbb,cc\n"[..]).await.unwrap();
        assert_eq!(run_wait::list_entries(&text, true), ["aa", "bb", "cc"]);
        // both lists can't read the one stdin
        let args = RunInputArgs {
            sample_list: Some(PathBuf::from("-")),
            repo_list: Some(PathBuf::from("-")),
            ..RunInputArgs::default()
        };
        let err = plan_inputs(&args).await.unwrap_err();
        assert!(err.to_string().contains("both read stdin"), "{err}");
    }

    /// Planning reads lists and repos, refuses missing files, and uploads nothing
    #[tokio::test]
    async fn planning_inputs_reads_without_uploading() {
        let dir = std::env::temp_dir().join(format!("thorctl-plan-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("samples.txt");
        std::fs::write(&list, "# files\naa\nbb,cc\n").unwrap();
        let local = dir.join("a.bin");
        std::fs::write(&local, "x").unwrap();
        let args = RunInputArgs {
            samples: vec!["dd".to_owned()],
            files: vec![local.clone()],
            repos: vec!["example.com/r:main".to_owned()],
            sample_list: Some(list),
            ..RunInputArgs::default()
        };
        let planned = plan_inputs(&args).await.unwrap();
        // the lists and the repo are read, and the local file waits to be uploaded
        assert_eq!(planned.inputs.samples, ["dd", "aa", "bb", "cc"]);
        assert_eq!(planned.inputs.repos.len(), 1);
        assert_eq!(planned.files, [local]);
        // a file that is missing is refused
        let missing = RunInputArgs {
            files: vec![dir.join("missing.bin")],
            ..RunInputArgs::default()
        };
        assert!(plan_inputs(&missing).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Parse a pipeline run command
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags after the pipeline name
    fn pipeline(flags: &[&str]) -> RunPipeline {
        let mut argv = vec!["run", "static/harvest"];
        argv.extend_from_slice(flags);
        RunPipeline::try_parse_from(argv).unwrap()
    }

    /// Start a mock that creates reactions
    async fn create_mock() -> (String, crate::utils::run_wait::testing::Requests) {
        mock(|line, _| {
            if line.starts_with("POST /api/reactions/") {
                (200, json!({"id": Uuid::new_v4()}).to_string())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await
    }

    /// One run per input creates one reaction per file, each tagged with the batch and the run's own
    /// tags, and without the flag every input joins one reaction
    #[tokio::test]
    async fn one_per_input_creates_a_reaction_per_input() {
        let (host, requests) = create_mock().await;
        let thorium = client(&host).await;
        let opts = BatchOpts {
            quiet: true,
            ..BatchOpts::default()
        };
        let dir = std::env::temp_dir().join(format!("thorctl-fan-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("list.txt");
        std::fs::write(&list, "cc\ndd,ee\n").unwrap();
        let list = list.to_string_lossy().into_owned();
        let flags = [
            "--sample",
            "aa",
            "--sample",
            "bb",
            "--sample-list",
            &list,
            "--tag",
            "t1",
        ];
        // the fan-out makes one reaction per file with the batch name
        let mut fan = flags.to_vec();
        fan.extend(["--one-per-input", "--batch", "nightly"]);
        run_pipeline(&thorium, &pipeline(&fan), &opts)
            .await
            .unwrap();
        let bodies: Vec<Value> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| serde_json::from_str(body).unwrap())
            .collect();
        assert_eq!(bodies.len(), 5, "{bodies:?}");
        let mut samples: Vec<String> = bodies
            .iter()
            .map(|body| body["samples"][0].as_str().unwrap().to_owned())
            .collect();
        samples.sort();
        assert_eq!(samples, ["aa", "bb", "cc", "dd", "ee"]);
        for body in &bodies {
            assert_eq!(body["samples"].as_array().unwrap().len(), 1);
            assert_eq!(body["tags"], json!(["t1", "nightly"]));
            assert_eq!(body["group"], "static");
            assert_eq!(body["pipeline"], "harvest");
        }
        // without the flag all the files join one reaction, tagged with a given batch name
        requests.lock().unwrap().clear();
        let mut single = flags.to_vec();
        single.extend(["--batch", "nightly"]);
        run_pipeline(&thorium, &pipeline(&single), &opts)
            .await
            .unwrap();
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let body: Value = serde_json::from_str(&sent[0].1).unwrap();
        assert_eq!(body["samples"].as_array().unwrap().len(), 5);
        assert_eq!(body["tags"], json!(["t1", "nightly"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fan-out with a partly failing API creates what it can and then fails the command
    #[tokio::test]
    async fn a_fan_out_with_a_refused_input_exits_nonzero() {
        let (host, requests) = mock(|line, body| {
            if line.starts_with("POST /api/reactions/") && body.contains("bad") {
                (400, r#"{"error":"no such file"}"#.to_owned())
            } else if line.starts_with("POST /api/reactions/") {
                (200, json!({"id": Uuid::new_v4()}).to_string())
            } else {
                (404, "{}".to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let opts = BatchOpts {
            quiet: true,
            workers: 1,
            ..BatchOpts::default()
        };
        let cmd = pipeline(&[
            "--one-per-input",
            "--sample",
            "aa",
            "--sample",
            "bad",
            "--sample",
            "cc",
        ]);
        let err = run_pipeline(&thorium, &cmd, &opts).await.unwrap_err();
        assert!(err.to_string().contains("1 of 3 runs"), "{err}");
        // all three were tried
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    /// A dry run reads the inputs and creates nothing, uploads nothing, and an empty fan-out is
    /// refused
    #[tokio::test]
    async fn dry_run_creates_nothing() {
        let (host, requests) = create_mock().await;
        let thorium = client(&host).await;
        let opts = BatchOpts {
            quiet: true,
            ..BatchOpts::default()
        };
        let dir = std::env::temp_dir().join(format!("thorctl-dry-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("a.bin");
        std::fs::write(&local, "x").unwrap();
        let local = local.to_string_lossy().into_owned();
        // a dry run with a local file, a fan-out, and a wait sends no request at all
        let cmd = pipeline(&[
            "--dry-run",
            "--one-per-input",
            "--sample",
            "aa",
            "--file",
            &local,
            "--wait",
        ]);
        run_pipeline(&thorium, &cmd, &opts).await.unwrap();
        assert!(requests.lock().unwrap().is_empty());
        // a dry run still refuses a missing file
        let cmd = pipeline(&["--dry-run", "--file", "/nonexistent-file"]);
        assert!(run_pipeline(&thorium, &cmd, &opts).await.is_err());
        // a fan-out with nothing to run on is an error before any request
        let err = run_pipeline(&thorium, &pipeline(&["--one-per-input"]), &opts)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least one input"), "{err}");
        assert!(requests.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write a keys file for a mock API and parse a thorctl command line that uses it
    ///
    /// # Arguments
    ///
    /// * `host` - The mock's url
    /// * `argv` - The arguments after the global flags
    fn parse_with_keys(host: &str, argv: &[&str]) -> crate::args::Args {
        // a keys file with a token needs no login request
        let dir = std::env::temp_dir().join(format!("thorctl-keys-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = dir.join("keys.yml");
        std::fs::write(&keys, format!("api: {host}\ntoken: token\n")).unwrap();
        let keys = keys.to_string_lossy().into_owned();
        let mut full = vec!["thorctl", "--keys", &keys, "--skip-update", "-q"];
        full.extend_from_slice(argv);
        crate::args::Args::try_parse_from(full).unwrap()
    }

    /// Run a parsed command line through the root handler of its subcommand
    ///
    /// # Arguments
    ///
    /// * `args` - The parsed arguments
    async fn dispatch(args: &crate::args::Args) -> Result<(), Error> {
        // every spelling is routed the way main.rs routes it
        match &args.cmd {
            crate::args::SubCommands::Run(run) => crate::handlers::run::handle(args, run).await,
            crate::args::SubCommands::Tools(tools) => {
                crate::handlers::tools::handle(args, tools).await
            }
            crate::args::SubCommands::Workflows(workflows) => {
                crate::handlers::workflows::handle(args, workflows).await
            }
            crate::args::SubCommands::Pipelines(pipelines) => {
                crate::handlers::pipelines::handle(args, pipelines).await
            }
            other => panic!("not a run command: {other:?}"),
        }
    }

    /// `run <kind>` reaches the handler of its kind and sends what the older spelling sends
    #[tokio::test]
    async fn every_kind_dispatches_like_its_own_command() {
        let id = Uuid::new_v4().to_string();
        let spellings: [(&[&str], &[&str]); 3] = [
            (
                &["run", "tool", "scan@1.0.0"],
                &["tools", "run", "scan@1.0.0"],
            ),
            (&["run", "workflow", &id], &["workflows", "run", &id]),
            (
                &["run", "pipeline", "static/harvest"],
                &["pipelines", "run", "static/harvest"],
            ),
        ];
        let mut first_requests = Vec::new();
        for (new, old) in spellings {
            let mut logs = Vec::new();
            for spelling in [new, old] {
                let (host, requests) = create_mock().await;
                let mut argv = spelling.to_vec();
                argv.extend([
                    "--sample",
                    "aa",
                    "--tag",
                    "t1",
                    "--one-per-input",
                    "--batch",
                    "b1",
                ]);
                let parsed = parse_with_keys(&host, &argv);
                let result = dispatch(&parsed).await;
                let sent: Vec<(String, String)> = requests.lock().unwrap().clone();
                logs.push((result.map_err(|err| err.to_string()), sent));
            }
            // both spellings send the same requests and end the same way
            assert_eq!(logs[0], logs[1], "{new:?} vs {old:?}");
            first_requests.push(logs.remove(0).1);
        }
        // the pipeline kind created a reaction with the flags
        let pipeline_sent = &first_requests[2];
        assert_eq!(pipeline_sent.len(), 1, "{pipeline_sent:?}");
        let body: Value = serde_json::from_str(&pipeline_sent[0].1).unwrap();
        assert_eq!(body["pipeline"], "harvest");
        assert_eq!(body["tags"], json!(["t1", "b1"]));
        // the workflow and tool kinds went looking for their own targets first
        assert!(
            first_requests[1]
                .iter()
                .all(|(line, _)| line.contains("/entities")),
            "{:?}",
            first_requests[1]
        );
        assert!(first_requests[0].len() + first_requests[1].len() > 0);
        assert_ne!(first_requests[0], first_requests[1]);
    }

    /// A reaction of the corn group started by a pipeline
    ///
    /// # Arguments
    ///
    /// * `status` - The reaction's status
    fn pipeline_reaction(status: ReactionStatus) -> thorium::models::Reaction {
        // a tool run's reaction with a pipeline source
        let mut reaction = reaction(Uuid::new_v4(), status);
        reaction.source = thorium::models::ReactionSource::Pipeline {
            name: "harvest".to_owned(),
            provenance: thorium::models::RunProvenance::default(),
        };
        reaction
    }

    /// Listing one kind reads past a first page that holds none of that kind
    #[tokio::test]
    async fn listing_a_kind_reads_every_page() {
        let tool_run = Uuid::new_v4();
        let first = json!({
            "cursor": 1,
            "details": [
                pipeline_reaction(ReactionStatus::Completed),
                pipeline_reaction(ReactionStatus::Completed),
            ],
        })
        .to_string();
        let second = json!({
            "cursor": null,
            "details": [reaction(tool_run, ReactionStatus::Completed)],
        })
        .to_string();
        let (host, _) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/group/corn/Completed/details/") {
                // the newest page holds only pipeline reactions, the next one the tool run
                if line.contains("cursor=1") {
                    (200, second.clone())
                } else {
                    (200, first.clone())
                }
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let runs = list_kind(
            &thorium,
            "corn",
            RunKind::Tool,
            Some(&ReactionStatus::Completed),
            10,
        )
        .await
        .unwrap();
        let ids: Vec<Uuid> = runs.iter().map(|run| run.id).collect();
        assert_eq!(ids, [tool_run]);
    }

    /// Listing workflow runs with a status reads past a first page that holds none with it
    #[tokio::test]
    async fn listing_workflow_runs_reads_every_page() {
        let (done, failed) = (Uuid::new_v4(), Uuid::new_v4());
        let first = json!({"cursor": Uuid::new_v4(), "data": [done.to_string()]}).to_string();
        let second = json!({"cursor": null, "data": [failed.to_string()]}).to_string();
        let done_json = serde_json::to_string(&reaction(done, ReactionStatus::Completed)).unwrap();
        let failed_json = serde_json::to_string(&reaction(failed, ReactionStatus::Failed)).unwrap();
        let (host, _) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/tag/WorkflowRun/") {
                // the newest page holds a completed run, the next one the failed run
                if line.contains("cursor=") {
                    (200, second.clone())
                } else {
                    (200, first.clone())
                }
            } else if line.starts_with(&format!("GET /api/reactions/corn/{done}")) {
                (200, done_json.clone())
            } else if line.starts_with(&format!("GET /api/reactions/corn/{failed}")) {
                (200, failed_json.clone())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let runs = list_workflow_runs(
            &thorium,
            "corn",
            "WorkflowRun",
            Some(&ReactionStatus::Failed),
            10,
        )
        .await
        .unwrap();
        let ids: Vec<Uuid> = runs.iter().map(|run| run.id).collect();
        assert_eq!(ids, [failed]);
    }

    /// Showing a run's results asks for hidden results, so a Hidden display type isn't dropped
    #[tokio::test]
    async fn show_results_reads_hidden_results() {
        let id = Uuid::new_v4();
        let run = reaction(id, ReactionStatus::Completed);
        let outputs = json!({"results": {"harvester": [{
            "id": Uuid::new_v4(), "groups": ["corn"], "cmd": null, "uploaded": chrono::Utc::now(),
            "result": {"functions": 412}, "entities": {}, "display_type": "Hidden", "children": {},
        }]}})
        .to_string();
        // the hidden result is only returned when hidden results are asked for, as the API does
        let (host, requests) = mock(move |line, _| {
            if line.starts_with(&format!("GET /api/reactions/corn/{id}")) {
                (200, serde_json::to_string(&run).unwrap())
            } else if line.starts_with("GET /api/files/results/") && line.contains("hidden=true") {
                (200, outputs.clone())
            } else if line.starts_with("GET /api/files/results/") {
                (200, r#"{"results": {}}"#.to_owned())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let cmd = ShowResults::try_parse_from([
            "show",
            &id.to_string(),
            "-g",
            "corn",
            "--format",
            "json",
        ])
        .unwrap();
        show_results(&thorium, &cmd).await.unwrap();
        // the run's single results read carries hidden=true, as the run collectors' reads do
        let sent = requests.lock().unwrap();
        let reads: Vec<_> = sent
            .iter()
            .filter(|(line, _)| line.contains("/results/"))
            .collect();
        assert_eq!(reads.len(), 1, "{sent:?}");
        assert!(reads[0].0.contains("hidden=true"), "{:?}", reads[0].0);
    }
}
