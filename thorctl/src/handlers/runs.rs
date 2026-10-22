//! Handles run commands: listing, describing, and retrying tool and workflow runs, starting
//! workflow runs, and showing a run's results

use serde::Serialize;
use std::collections::BTreeMap;
use thorium::client::ResultsClient;
use thorium::models::{
    AgentProvenance, EntityKinds, ModelConfigRef, OutputMap, Reaction, ReactionCursorOpts,
    ReactionModelBinding, ReactionSource, RepoDependencyRequest, ResultGetParams, RunInputs,
    SampleRequest, ToolProvenance, WorkflowResultKey, WorkflowRunRequest,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::repos::RepoTarget;
use crate::args::results::ShowResults;
use crate::args::runs::{DescribeRun, GetRuns, RetryRun, RunInputArgs, Runs};
use crate::args::workflows::RunWorkflow;
use crate::handlers::agents::find;
use crate::handlers::instances::find_instance;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
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
struct RunView(
    /// The run's reaction as the API returned it
    Reaction,
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

/// Build a run's inputs, uploading any local files first
///
/// A missing local file is refused before anything is uploaded.
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
    // make sure every local file exists before uploading any of them
    for path in &args.files {
        if !path.is_file() {
            return Err(Error::new(format!(
                "{} doesn't exist or isn't a file",
                path.display()
            )));
        }
    }
    // parse each repo the way reactions create does
    let mut inputs = RunInputs::default();
    for raw in &args.repos {
        let target = RepoTarget::try_from(raw)?;
        inputs = inputs.repo(RepoDependencyRequest::from(target));
    }
    // run on each given file
    for sample in &args.samples {
        inputs = inputs.sample(sample);
    }
    // upload each local file and run on it
    for path in &args.files {
        let req = SampleRequest::new(path, groups.to_vec());
        let created = thorium.files.create(req).await?;
        inputs = inputs.sample(created.sha256);
    }
    Ok(inputs)
}

/// Start a workflow run and print its reaction id, or wait for a condition and print the run
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The run command to execute
pub async fn run_workflow(thorium: &Thorium, cmd: &RunWorkflow) -> Result<(), Error> {
    // find the workflow instance
    let instance = find_instance(thorium, &cmd.instance).await?;
    if instance.kind != EntityKinds::WorkflowInstance {
        return Err(Error::new(format!(
            "{} is a {}, not a workflow instance",
            cmd.instance, instance.kind
        )));
    }
    // upload local files to the run group, or the instance's groups when none was given
    let groups = match &cmd.group {
        Some(group) => vec![group.clone()],
        None => instance.groups.clone(),
    };
    let inputs = run_inputs(thorium, &cmd.inputs, &groups).await?;
    // build the run request
    let mut req = WorkflowRunRequest::new(inputs);
    req.group.clone_from(&cmd.group);
    req.tags.clone_from(&cmd.tag);
    req.sla = cmd.sla;
    // start the run
    let created = thorium.reactions.run_workflow(instance.id, &req).await?;
    // without a condition or watch just print the run's reaction id
    if cmd.wait.wait_for.is_none() && !cmd.watch.watch {
        println!("{}", created.id);
        return Ok(());
    }
    // otherwise follow the run in the group it was created in
    let group = match &cmd.group {
        Some(group) => group.clone(),
        None => find_run(thorium, created.id, None).await?.group,
    };
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        Ok(RunView(thorium.reactions.get(&group, created.id).await?))
    })
    .await
}

/// List workflow runs
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetRuns) -> Result<(), Error> {
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
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        let mut runs = Vec::new();
        for group in &groups {
            // list this group's run ids and read each run
            let opts = ReactionCursorOpts::default().group(group).limit(cmd.limit);
            let cursor = thorium.reactions.list_tag_new(&tag, &opts).await?;
            for id in cursor.data {
                let id = Uuid::parse_str(&id).map_err(|err| Error::new(err.to_string()))?;
                runs.push(thorium.reactions.get(group, id).await?);
            }
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
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}
