//! Handles workflow commands

use serde::Serialize;
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, InstancedAgentToolset, ModelAuthSource,
    ModelMatcher, WorkflowCatalogEntity, WorkflowCatalogEntityRequest, WorkflowLimits,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::workflows::{
    AddAgent, CreateWorkflow, DescribeWorkflow, GetWorkflows, RemoveAgent, SetEntry, ShowToolset,
    Workflows,
};
use crate::handlers::agents::find;
use crate::handlers::instances::find_instance;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::watch;

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

/// Create a workflow
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command to execute
async fn create(thorium: &Thorium, cmd: &CreateWorkflow) -> Result<(), Error> {
    // find each agent and the entry agent
    let mut req = WorkflowCatalogEntityRequest::default();
    for agent in &cmd.agent {
        req = req.agent(
            find(thorium, EntityKinds::Agent, "an AI agent", agent)
                .await?
                .id,
        );
    }
    if let Some(entry) = &cmd.entry {
        req = req.entry_agent(
            find(thorium, EntityKinds::Agent, "an AI agent", entry)
                .await?
                .id,
        );
    }
    // add the default models in priority order
    for model_id in &cmd.default_model {
        req = req.default_model(ModelMatcher {
            model_id: model_id.clone(),
            ..ModelMatcher::default()
        });
    }
    // set the limits and settings that were given
    if let Some(max) = cmd.max_alive_sub_agents {
        req = req.limits(WorkflowLimits {
            max_alive_sub_agents: Some(max),
        });
    }
    if !cmd.model_auth.is_empty() {
        req = req.model_auth(ModelAuthSource {
            levels: cmd.model_auth.iter().map(|level| (*level).into()).collect(),
        });
    }
    if cmd.no_auto_instance {
        req = req.auto_instance(false);
    }
    if cmd.extend_target_groups {
        req = req.extend_target_groups();
    }
    // create the workflow
    let mut entity = EntityRequest::new(
        &cmd.name,
        EntityMetadataRequest::WorkflowCatalog(req),
        cmd.group.clone(),
    );
    entity.description.clone_from(&cmd.description);
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

/// Handle all workflow commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The workflows command to execute
pub async fn handle(args: &Args, cmd: &Workflows) -> Result<(), Error> {
    // load our config and instance our client
    let (_, thorium) = utils::get_client(args).await?;
    // call the right workflows handler
    let result = match cmd {
        Workflows::Get(cmd) => get(&thorium, cmd).await,
        Workflows::Describe(cmd) => describe(&thorium, cmd).await,
        Workflows::Create(cmd) => create(&thorium, cmd).await,
        Workflows::AddAgent(cmd) => add_agent(&thorium, cmd).await,
        Workflows::RemoveAgent(cmd) => remove_agent(&thorium, cmd).await,
        Workflows::Entry(cmd) => set_entry(&thorium, cmd).await,
        Workflows::Toolset(cmd) => toolset(&thorium, cmd).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}
