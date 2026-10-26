//! Handles workflow commands

use serde::Serialize;
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, InstancedAgentToolset, ModelMatcher,
    WorkflowCatalogEntity,
};
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::workflows::{
    AddAgent, CreateWorkflow, DeleteWorkflow, DescribeWorkflow, EditWorkflow, GetWorkflows,
    RemoveAgent, RunWorkflow, SetEntry, ShowToolset, SpecWorkflow, UpdateWorkflow, Workflows,
};
use crate::handlers::agents::refs::resolve_agent;
use crate::handlers::agents::{confirm_delete, find};
use crate::handlers::edit::{Editable, edit_with, update_with};
use crate::handlers::instances::find_instance;
use crate::handlers::run_batch::BatchOpts;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::spec_io;
use crate::utils::watch;

mod edit;

use edit::{WorkflowEditable, WorkflowTarget};

/// Get a workflow's metadata, if the entity is a workflow catalog
///
/// # Arguments
///
/// * `workflow` - The entity to read
fn workflow_meta(workflow: &Entity) -> Option<&WorkflowCatalogEntity> {
    // only workflow catalogs have workflow metadata
    match &workflow.metadata {
        EntityMetadata::WorkflowCatalog(meta) => Some(meta),
        _ => None,
    }
}

/// Show the model ids of a models list in priority order, or `-` when it's empty
///
/// # Arguments
///
/// * `models` - The models to show
fn models(models: &[ModelMatcher]) -> String {
    // an empty list accepts any model the run can reach
    if models.is_empty() {
        return "-".to_owned();
    }
    models
        .iter()
        .map(|matcher| matcher.model_id.clone())
        .collect::<Vec<String>>()
        .join(", ")
}

/// Name a workflow's entry agent, or `-` when it has none
///
/// # Arguments
///
/// * `meta` - The workflow's metadata
fn entry(meta: &WorkflowCatalogEntity) -> String {
    // name the entry agent when it's one of the workflow's agents
    match meta.entry_agent {
        Some(id) => meta
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .map_or_else(|| id.to_string(), |agent| format!("{} ({id})", agent.name)),
        None => "-".to_owned(),
    }
}

/// The workflows one command lists
#[derive(Serialize)]
#[serde(transparent)]
struct WorkflowsView(
    /// The workflows as the API returned them
    Vec<Entity>,
);

impl View for WorkflowsView {
    /// Render the workflows as a table
    fn table(&self) -> String {
        // build one row per workflow
        let rows = self
            .0
            .iter()
            .map(|workflow| {
                let meta = workflow_meta(workflow);
                vec![
                    workflow.name.clone(),
                    workflow.id.to_string(),
                    meta.map_or(0, |meta| meta.agents.len()).to_string(),
                    meta.map_or_else(|| "-".to_owned(), entry),
                    meta.and_then(|meta| meta.latest_instance)
                        .map_or_else(|| "-".to_owned(), |id| id.to_string()),
                    workflow.groups.join(", "),
                ]
            })
            .collect();
        output::table_from(
            &[
                "NAME",
                "ID",
                "AGENTS",
                "ENTRY AGENT",
                "LATEST INSTANCE",
                "GROUPS",
            ],
            rows,
        )
    }

    /// Render the workflows as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each workflow, separated by rules
        self.0
            .iter()
            .map(|workflow| describe_text(workflow, ansi))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// Render a workflow's details as text
///
/// # Arguments
///
/// * `workflow` - The workflow to describe
/// * `ansi` - Whether to style the text
fn describe_text(workflow: &Entity, ansi: bool) -> String {
    // start with the workflow's name and groups
    let mut lines = vec![render::header(
        &workflow.name,
        &workflow.groups.join(", "),
        ansi,
    )];
    lines.push(render::field("ID", &workflow.id.to_string(), ansi));
    lines.push(render::field("Groups", &workflow.groups.join(", "), ansi));
    // add the agents, entry agent, settings, and latest instance
    if let Some(meta) = workflow_meta(workflow) {
        let agents = if meta.agents.is_empty() {
            "-".to_owned()
        } else {
            meta.agents
                .iter()
                .map(|agent| format!("{} ({})", agent.name, agent.id))
                .collect::<Vec<String>>()
                .join(", ")
        };
        lines.push(render::field("Agents", &agents, ansi));
        lines.push(render::field("Entry agent", &entry(meta), ansi));
        lines.push(render::field(
            "Default models",
            &models(&meta.default_models),
            ansi,
        ));
        let max_alive = meta
            .limits
            .max_alive_sub_agents
            .map_or_else(|| "-".to_owned(), |max| max.to_string());
        lines.push(render::field("Max alive sub-agents", &max_alive, ansi));
        let levels = if meta.model_auth.levels.is_empty() {
            "every level".to_owned()
        } else {
            meta.model_auth
                .levels
                .iter()
                .map(|level| format!("{level:?}"))
                .collect::<Vec<String>>()
                .join(", ")
        };
        lines.push(render::field("Model auth", &levels, ansi));
        lines.push(render::field(
            "Auto instance",
            &meta.auto_instance.to_string(),
            ansi,
        ));
        lines.push(render::field("Modified", &meta.modified.to_rfc3339(), ansi));
        lines.push(render::field(
            "Latest instance",
            &meta
                .latest_instance
                .map_or_else(|| "-".to_owned(), |id| id.to_string()),
            ansi,
        ));
    }
    // add the description if there is one
    lines.push(render::field(
        "Description",
        workflow.description.as_deref().unwrap_or("-"),
        ansi,
    ));
    lines.join("\n")
}

/// One workflow described in full
#[derive(Serialize)]
#[serde(transparent)]
struct WorkflowView(
    /// The workflow as the API returned it
    Entity,
);

impl View for WorkflowView {
    /// Render the workflow as a one row table
    fn table(&self) -> String {
        // reuse the list table for a single workflow
        WorkflowsView(vec![self.0.clone()]).table()
    }

    /// Render the workflow as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        describe_text(&self.0, ansi)
    }
}

/// One agent of a workflow instance and the full spec of each of its tools
#[derive(Serialize)]
#[serde(transparent)]
struct ToolsetView(
    /// The agent and its tools as the API returned them
    InstancedAgentToolset,
);

impl View for ToolsetView {
    /// Render the agent's tools as a table
    fn table(&self) -> String {
        // one row per tool
        let rows = self
            .0
            .tools
            .iter()
            .map(|tool| {
                vec![
                    tool.tool.name.clone(),
                    tool.tool
                        .function_name
                        .clone()
                        .unwrap_or_else(|| "-".to_owned()),
                    tool.tool.version.clone(),
                    tool.tool.tool_instance.to_string(),
                    tool.tool.spec_hash.clone(),
                ]
            })
            .collect();
        output::table_from(
            &["NAME", "FUNCTION", "VERSION", "TOOL INSTANCE", "SPEC HASH"],
            rows,
        )
    }

    /// Render the agent and each tool's full spec as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // start with the agent
        let agent = &self.0.agent;
        let mut lines = vec![render::header(&agent.name, &agent.groups.join(", "), ansi)];
        lines.push(render::field("Agent", &agent.agent.to_string(), ansi));
        lines.push(render::field("Config hash", &agent.config_hash, ansi));
        lines.push(render::field("Toolset digest", &agent.toolset_digest, ansi));
        // then each tool with its spec
        for tool in &self.0.tools {
            lines.push(render::separator(ansi));
            lines.push(render::field(
                "Tool",
                &format!(
                    "{} {} ({})",
                    tool.tool.name, tool.tool.version, tool.tool.tool_instance
                ),
                ansi,
            ));
            lines.push(render::field("Spec hash", &tool.tool.spec_hash, ansi));
            let spec = serde_json::to_string_pretty(&tool.spec).unwrap_or_default();
            lines.push(render::label("Spec", ansi));
            lines.push(spec);
        }
        lines.join("\n")
    }
}

/// Build a workflow update that changes only the given fields
///
/// # Arguments
///
/// * `change` - Sets the fields of an empty workflow update
fn workflow_update(change: impl FnOnce(&mut EntityMetadataUpdate)) -> EntityUpdate {
    // start from an update that changes nothing
    let mut update = EntityMetadataUpdate::WorkflowCatalog {
        entry_agent: None,
        clear_entry_agent: None,
        auto_instance: None,
        limits: None,
        default_models: None,
        model_auth: None,
        add_agents: Vec::new(),
        remove_agents: Vec::new(),
        extend_target_groups: false,
    };
    change(&mut update);
    EntityUpdate::default().metadata(update)
}

/// List workflows
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetWorkflows) -> Result<(), Error> {
    // list workflows in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // list the details of the workflows in the requested groups
        let opts = EntityListOpts::default()
            .groups(cmd.group.clone())
            .kind(EntityKinds::WorkflowCatalog)
            .limit(cmd.limit);
        let mut cursor = thorium.entities.list_details(&opts).await?;
        // read pages until the limit or the last workflow
        let mut workflows = std::mem::take(&mut cursor.data);
        while !cursor.exhausted() {
            cursor.refill().await?;
            workflows.append(&mut cursor.data);
        }
        Ok(WorkflowsView(workflows))
    })
    .await
}

/// Describe a workflow
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeWorkflow) -> Result<(), Error> {
    // describe the workflow in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // find the workflow with its agents and latest instance filled in
        let workflow = find(
            thorium,
            EntityKinds::WorkflowCatalog,
            "a workflow",
            &cmd.workflow,
        )
        .await?;
        Ok(WorkflowView(workflow))
    })
    .await
}

/// Create a workflow from its flags and spec
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command to execute
/// * `spec` - The spec document, if one was given
async fn create(
    thorium: &Thorium,
    cmd: &CreateWorkflow,
    spec: Option<&serde_json::Value>,
) -> Result<(), Error> {
    // describe the workflow from its spec and flags
    let view = edit::create_view(spec, cmd)?;
    // find each agent and the entry agent in the workflow's groups
    let mut agents = Vec::new();
    for agent in &view.agents {
        agents.push(resolve_agent(thorium, &view.groups, agent).await?);
    }
    let entry = match &view.entry_agent {
        Some(entry) => Some(resolve_agent(thorium, &view.groups, entry).await?),
        None => None,
    };
    let req = edit::create_request(&view, &agents, entry, cmd.extend_target_groups)?;
    // create the workflow
    let mut entity = EntityRequest::new(
        &view.name,
        EntityMetadataRequest::WorkflowCatalog(req),
        view.groups.clone(),
    );
    entity.description.clone_from(&view.description);
    let resp = thorium.entities.create(entity).await?;
    println!("Created workflow {} ({})", resp.name, resp.id);
    Ok(())
}

/// Add an AI agent to a workflow
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The add-agent command to execute
async fn add_agent(thorium: &Thorium, cmd: &AddAgent) -> Result<(), Error> {
    // find the workflow and the agent
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    // add the orchestrates edge
    let extend = cmd.extend_target_groups;
    let update = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            add_agents,
            extend_target_groups,
            ..
        } = update
        {
            add_agents.push(agent.id);
            *extend_target_groups = extend;
        }
    });
    thorium.entities.update(workflow.id, update).await?;
    println!("Workflow {} now includes {}", workflow.name, agent.name);
    Ok(())
}

/// Remove an AI agent from a workflow
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The remove-agent command to execute
async fn remove_agent(thorium: &Thorium, cmd: &RemoveAgent) -> Result<(), Error> {
    // find the workflow and the agent
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    // remove the orchestrates edge
    let update = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(agent.id);
        }
    });
    thorium.entities.update(workflow.id, update).await?;
    println!(
        "Workflow {} no longer includes {}",
        workflow.name, agent.name
    );
    Ok(())
}

/// Set or clear a workflow's entry agent
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The entry command to execute
async fn set_entry(thorium: &Thorium, cmd: &SetEntry) -> Result<(), Error> {
    // find the workflow and the new entry agent
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    let agent = match &cmd.agent {
        Some(agent) => Some(find(thorium, EntityKinds::Agent, "an AI agent", agent).await?),
        None => None,
    };
    // replace or clear the entry agent
    let (new, clear) = (agent.as_ref().map(|agent| agent.id), cmd.clear);
    let update = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            entry_agent,
            clear_entry_agent,
            ..
        } = update
        {
            *entry_agent = new;
            *clear_entry_agent = clear.then_some(true);
        }
    });
    thorium.entities.update(workflow.id, update).await?;
    match agent {
        Some(agent) => println!("Workflow {} now starts with {}", workflow.name, agent.name),
        None => println!("Workflow {} no longer has an entry agent", workflow.name),
    }
    Ok(())
}

/// Find one of a workflow instance's own agents by its ID or name
///
/// Returns `None` when no agent of the instance has that ID or name.
///
/// # Arguments
///
/// * `instance` - The workflow instance
/// * `value` - The agent's ID or name
fn instance_agent(instance: &Entity, value: &str) -> Result<Option<Uuid>, Error> {
    // only workflow instances have agents
    let EntityMetadata::WorkflowInstance(meta) = &instance.metadata else {
        return Ok(None);
    };
    // match the ID or the name each agent was snapshotted with
    let id = Uuid::parse_str(value).ok();
    let matches = meta
        .agents
        .iter()
        .filter(|agent| Some(agent.agent) == id || agent.name == value)
        .map(|agent| agent.agent)
        .collect::<Vec<Uuid>>();
    match matches.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(*id)),
        ids => Err(Error::new(format!(
            "{} agents of {} are named {value}; use one of their IDs: {}",
            ids.len(),
            instance.name,
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>()
                .join(", ")
        ))),
    }
}

/// Show one agent of a workflow instance and the full spec of each of its tools
///
/// The agent is found among the instance's own agents first, so an agent deleted since the
/// instance was made is still shown as it was snapshotted.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The toolset command to execute
async fn toolset(thorium: &Thorium, cmd: &ShowToolset) -> Result<(), Error> {
    // find the instance and the agent, among the instance's agents first
    let instance = find_instance(thorium, &cmd.instance).await?;
    let agent = match instance_agent(&instance, &cmd.agent)? {
        Some(id) => id,
        None => {
            find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent)
                .await?
                .id
        }
    };
    // read the agent's toolset from the instance
    let read = thorium.entities.toolset(instance.id, agent).await?;
    output::print(
        &ToolsetView(read),
        cmd.format.format.unwrap_or(OutputFormat::Human),
    )
}

/// Edit a workflow in an editor, or from `--file`, `--set`, `--unset` and `--patch`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The edit command to execute
async fn edit_workflow(thorium: &Thorium, conf: &CtlConf, cmd: &EditWorkflow) -> Result<(), Error> {
    // find the workflow
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    let target = WorkflowTarget {
        id: workflow.id,
        extend_target_groups: cmd.extend_target_groups,
    };
    // run the shared edit flow, which prints the result
    edit_with::<WorkflowEditable>(thorium, conf, &target, &cmd.edit).await?;
    Ok(())
}

/// Update a workflow from flags, `--file`, `--set`, `--unset` and `--patch`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command to execute
async fn update_workflow(thorium: &Thorium, cmd: &UpdateWorkflow) -> Result<(), Error> {
    // find the workflow
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    let target = WorkflowTarget {
        id: workflow.id,
        extend_target_groups: cmd.extend_target_groups,
    };
    // turn the typed flags into edits that apply before the `--set` and `--unset` edits
    let view = WorkflowEditable::to_view(&workflow);
    let mut args = cmd.edit.clone();
    let mut ops = edit::typed_ops(cmd, &view)?;
    ops.append(&mut args.ops);
    args.ops = ops;
    // run the shared update flow, which prints the result
    update_with::<WorkflowEditable>(thorium, &target, &args).await?;
    Ok(())
}

/// Print a workflow as a spec
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The spec command to execute
async fn print_spec(thorium: &Thorium, cmd: &SpecWorkflow) -> Result<(), Error> {
    // find the workflow and build its view
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    let view = WorkflowEditable::to_view(&workflow);
    // write the view as JSON or, by default, YAML
    let text = match cmd.format.format {
        Some(OutputFormat::Json) => utils::curated_json(&view, WorkflowEditable::FIELD_ORDER)?,
        _ => utils::curated_yaml(&view, WorkflowEditable::FIELD_ORDER)?,
    };
    // print it, or save it to the file asked for
    match &cmd.output {
        Some(path) => std::fs::write(path, text)
            .map_err(|err| Error::new(format!("Failed to write {}: {err}", path.display()))),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

/// Delete a workflow
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete command to execute
async fn delete_workflow(thorium: &Thorium, cmd: &DeleteWorkflow) -> Result<(), Error> {
    // find the workflow
    let workflow = find(
        thorium,
        EntityKinds::WorkflowCatalog,
        "a workflow",
        &cmd.workflow,
    )
    .await?;
    // a dry run only says what would be deleted
    if cmd.dry_run {
        println!(
            "Dry run: would delete workflow {} ({})",
            workflow.name, workflow.id
        );
        return Ok(());
    }
    // confirm the deletion
    if !confirm_delete("workflow", &workflow.name, cmd.skip_confirm)? {
        println!("Cancelled.");
        return Ok(());
    }
    // delete the workflow, which the API refuses while something still needs it
    thorium
        .entities
        .delete(workflow.id)
        .await
        .map_err(utils::delete_refusal)?;
    println!("Deleted workflow {} ({})", workflow.name, workflow.id);
    Ok(())
}

/// Whether a name is of the form `{workflow}#r{revision}`
///
/// # Arguments
///
/// * `value` - The name to check
fn names_revision(value: &str) -> bool {
    // a revision is a number after the last `#r`, behind a non-empty workflow name
    value
        .rsplit_once("#r")
        .is_some_and(|(root, revision)| !root.is_empty() && revision.parse::<u64>().is_ok())
}

/// Get the latest instance of a workflow
///
/// # Arguments
///
/// * `workflow` - The workflow
fn latest_instance_of(workflow: &Entity) -> Result<Uuid, Error> {
    // the workflow keeps its newest ready instance
    workflow_meta(workflow)
        .and_then(|meta| meta.latest_instance)
        .ok_or_else(|| {
            Error::new(format!(
                "Workflow {} has no ready instance yet; request one with `thorctl instances \
                 request workflow {}` and wait for it with `--wait-for instance-revision=1`",
                workflow.name, workflow.name
            ))
        })
}

/// Find the instance a run command names
///
/// A workflow's name or ID means its latest instance, `{workflow}#r{revision}` one revision, and
/// an instance's ID that instance.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `value` - What the command was given
pub(crate) async fn run_instance(thorium: &Thorium, value: &str) -> Result<String, Error> {
    // a revision name is an instance name
    if names_revision(value) {
        return Ok(value.to_owned());
    }
    // an ID names a workflow or an instance, and anything else is left to the instance lookup
    if let Ok(id) = Uuid::parse_str(value) {
        let found = thorium.entities.get(id).await?;
        if found.kind == EntityKinds::WorkflowCatalog {
            return latest_instance_of(&found).map(|latest| latest.to_string());
        }
        return Ok(value.to_owned());
    }
    // a name is a workflow's, or failing that an instance's
    match find(thorium, EntityKinds::WorkflowCatalog, "a workflow", value).await {
        Ok(workflow) => latest_instance_of(&workflow).map(|latest| latest.to_string()),
        Err(not_a_workflow) => match find_instance(thorium, value).await {
            Ok(instance) => Ok(instance.id.to_string()),
            Err(_) => Err(not_a_workflow),
        },
    }
}

/// Run a workflow and follow it as the command's flags ask
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The run command to execute
/// * `opts` - How a batch of runs is started and printed
async fn run(thorium: &Thorium, cmd: &RunWorkflow, opts: &BatchOpts) -> Result<(), Error> {
    // find the instance to run, noting a lookup by workflow name on stderr
    let instance = run_instance(thorium, &cmd.instance).await?;
    if instance != cmd.instance {
        eprintln!("Running {} at instance {instance}", cmd.instance);
    }
    // run it with the same inputs and flags
    let resolved = RunWorkflow {
        instance,
        common: cmd.common.clone(),
    };
    crate::handlers::runs::run_workflow(thorium, &resolved, opts).await
}

/// Handle all workflow commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The workflows command to execute
pub async fn handle(args: &Args, cmd: &Workflows) -> Result<(), Error> {
    // read a create's spec before making any request
    let spec = match cmd {
        Workflows::Create(CreateWorkflow {
            spec: Some(path), ..
        }) => Some(spec_io::read_document(&path.to_string_lossy(), "--spec")?),
        _ => None,
    };
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // call the right workflows handler
    let result = match cmd {
        Workflows::Get(cmd) => get(&thorium, cmd).await,
        Workflows::Describe(cmd) => describe(&thorium, cmd).await,
        Workflows::Create(cmd) => create(&thorium, cmd, spec.as_ref()).await,
        Workflows::AddAgent(cmd) => add_agent(&thorium, cmd).await,
        Workflows::RemoveAgent(cmd) => remove_agent(&thorium, cmd).await,
        Workflows::Entry(cmd) => set_entry(&thorium, cmd).await,
        Workflows::Toolset(cmd) => toolset(&thorium, cmd).await,
        Workflows::Update(cmd) => update_workflow(&thorium, cmd).await,
        Workflows::Edit(cmd) => edit_workflow(&thorium, &conf, cmd).await,
        Workflows::Spec(cmd) => print_spec(&thorium, cmd).await,
        Workflows::Delete(cmd) => delete_workflow(&thorium, cmd).await,
        Workflows::Run(cmd) => run(&thorium, cmd, &BatchOpts::from_args(args)).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use super::{latest_instance_of, names_revision};
    use crate::handlers::agents::fixtures::entity;
    use thorium::models::{EntityKinds, EntityMetadata, WorkflowCatalogEntity};
    use uuid::Uuid;

    /// Build a workflow named `triage` with a latest instance or none
    ///
    /// # Arguments
    ///
    /// * `latest` - The workflow's latest instance
    fn workflow(latest: Option<Uuid>) -> thorium::models::Entity {
        // only the latest instance matters to a run by name
        entity(
            "triage",
            EntityKinds::WorkflowCatalog,
            EntityMetadata::WorkflowCatalog(WorkflowCatalogEntity {
                latest_instance: latest,
                ..WorkflowCatalogEntity::default()
            }),
            &["lab"],
        )
    }

    /// A name with `#r` and a number names one revision, and nothing else does
    #[test]
    fn revisions_are_named_with_a_number() {
        // a revision of a workflow
        assert!(names_revision("triage#r1"));
        assert!(names_revision("my flow#r12"));
        // a bare name, an ID, and a malformed revision are not revision names
        assert!(!names_revision("triage"));
        assert!(!names_revision("triage#r"));
        assert!(!names_revision("triage#rx"));
        assert!(!names_revision("#r1 "));
        assert!(!names_revision(&Uuid::new_v4().to_string()));
    }

    /// A run by name goes to the workflow's latest instance, or says how to get one
    #[test]
    fn latest_instance_is_the_default() {
        // the latest instance is the one the workflow names
        let latest = Uuid::new_v4();
        assert_eq!(latest_instance_of(&workflow(Some(latest))).unwrap(), latest);
        // a workflow with no ready instance names the command that requests one
        let err = latest_instance_of(&workflow(None)).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("triage") && text.contains("no ready instance"),
            "{text}"
        );
        assert!(text.contains("instances request workflow triage"), "{text}");
        // an entity that isn't a workflow has none either
        let other = entity("yara", EntityKinds::Tool, EntityMetadata::Other, &["lab"]);
        assert!(latest_instance_of(&other).is_err());
    }
}
