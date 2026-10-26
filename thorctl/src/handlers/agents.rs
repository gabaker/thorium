//! Handles AI agent commands

use serde::Serialize;
use std::path::Path;
use thorium::models::{
    AgentConfig, AgentEntity, AgentEntityRequest, Entity, EntityKinds, EntityListOpts,
    EntityMetadata, EntityMetadataRequest, EntityMetadataUpdate, EntityRequest, EntityUpdate,
    ModelAuthSource,
};
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::agents::{
    Agents, ClearHarness, CreateAgent, Delegate, DeleteAgent, DescribeAgent, EditAgent, GetAgents,
    LinkTool, LinkToolset, SetHarness, SpecAgent, Undelegate, UnlinkTool, UnlinkToolset,
    UpdateAgent,
};
use crate::handlers::edit::{Editable, edit_with, update_with};
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::spec_io;
use crate::utils::watch;

pub(crate) mod edit;
pub(crate) mod refs;

#[cfg(test)]
pub(crate) mod fixtures;

use edit::{AgentEditable, AgentTarget};
use refs::{ToolRef, ToolSetRef};

/// Get an AI agent's metadata, if the entity is an AI agent
///
/// # Arguments
///
/// * `agent` - The entity to read
fn agent_meta(agent: &Entity) -> Option<&AgentEntity> {
    // only AI agents have agent metadata
    match &agent.metadata {
        EntityMetadata::Agent(meta) => Some(meta),
        _ => None,
    }
}

/// List entities as `name (id)`, or `-` when there are none
///
/// # Arguments
///
/// * `entities` - The entities to list
fn names(entities: &[Entity]) -> String {
    // name each entity with its id
    if entities.is_empty() {
        return "-".to_owned();
    }
    entities
        .iter()
        .map(|entity| format!("{} ({})", entity.name, entity.id))
        .collect::<Vec<String>>()
        .join(", ")
}

/// Show an optional limit, or `-` when it's unset
///
/// # Arguments
///
/// * `limit` - The limit to show
fn limit(limit: Option<u32>) -> String {
    // unset limits use the harness's default
    limit.map_or_else(|| "-".to_owned(), |limit| limit.to_string())
}

/// The AI agents one command lists
#[derive(Serialize)]
#[serde(transparent)]
struct AgentsView(
    /// The AI agents as the API returned them
    Vec<Entity>,
);

impl View for AgentsView {
    /// Render the AI agents as a table
    fn table(&self) -> String {
        // build one row per AI agent
        let rows = self
            .0
            .iter()
            .map(|agent| {
                let meta = agent_meta(agent);
                vec![
                    agent.name.clone(),
                    agent.id.to_string(),
                    meta.map_or(0, |meta| meta.config.models.len()).to_string(),
                    meta.map_or(0, |meta| meta.latest_tools.len() + meta.pinned_tools.len())
                        .to_string(),
                    meta.map_or(0, |meta| meta.sub_agents.len()).to_string(),
                    agent.groups.join(", "),
                ]
            })
            .collect();
        output::table_from(
            &["NAME", "ID", "MODELS", "TOOLS", "SUB-AGENTS", "GROUPS"],
            rows,
        )
    }

    /// Render the AI agents as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each AI agent, separated by rules
        self.0
            .iter()
            .map(|agent| describe_text(agent, ansi))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// Render an AI agent's details as text
///
/// # Arguments
///
/// * `agent` - The AI agent to describe
/// * `ansi` - Whether to style the text
fn describe_text(agent: &Entity, ansi: bool) -> String {
    // start with the AI agent's name and groups
    let mut lines = vec![render::header(&agent.name, &agent.groups.join(", "), ansi)];
    lines.push(render::field("ID", &agent.id.to_string(), ansi));
    lines.push(render::field("Groups", &agent.groups.join(", "), ansi));
    // add the config, its hash, and the edges
    if let Some(meta) = agent_meta(agent) {
        let config = &meta.config;
        lines.push(render::field("Config hash", &meta.config_hash, ansi));
        lines.push(render::field("Modified", &meta.modified.to_rfc3339(), ansi));
        lines.push(render::field(
            "Function",
            config.function_name.as_deref().unwrap_or("-"),
            ansi,
        ));
        // list the accepted models in priority order
        let models = if config.models.is_empty() {
            "any".to_owned()
        } else {
            config
                .models
                .iter()
                .map(|matcher| matcher.model_id.clone())
                .collect::<Vec<String>>()
                .join(", ")
        };
        lines.push(render::field("Models", &models, ansi));
        lines.push(render::field(
            "Model requirements",
            &serde_json::to_string(&config.model_requirements).unwrap_or_default(),
            ansi,
        ));
        lines.push(render::field(
            "Limits",
            &format!(
                "max tools {}, max turns {}, max swarm size {}",
                limit(config.max_tools),
                limit(config.max_turns),
                limit(config.max_swarm_size)
            ),
            ansi,
        ));
        lines.push(render::field(
            "Tool choice",
            &format!("{:?}", config.tool_choice),
            ansi,
        ));
        lines.push(render::field(
            "Parallel tool calls",
            &config
                .parallel_tool_calls
                .map_or_else(|| "default".to_owned(), |set| set.to_string()),
            ansi,
        ));
        lines.push(render::field(
            "Tools (latest)",
            &names(&meta.latest_tools),
            ansi,
        ));
        lines.push(render::field(
            "Tools (pinned)",
            &names(&meta.pinned_tools),
            ansi,
        ));
        lines.push(render::field("Toolsets", &names(&meta.tool_sets), ansi));
        lines.push(render::field("Sub-agents", &names(&meta.sub_agents), ansi));
        let harness = meta.harness.as_ref().map_or_else(
            || "-".to_owned(),
            |harness| format!("{} ({})", harness.name, harness.id),
        );
        lines.push(render::field("Harness", &harness, ansi));
    }
    // add the description if there is one
    lines.push(render::field(
        "Description",
        agent.description.as_deref().unwrap_or("-"),
        ansi,
    ));
    lines.join("\n")
}

/// One AI agent described in full
#[derive(Serialize)]
#[serde(transparent)]
struct AgentView(
    /// The AI agent as the API returned it
    Entity,
);

impl View for AgentView {
    /// Render the AI agent as a one row table
    fn table(&self) -> String {
        // reuse the list table for a single AI agent
        AgentsView(vec![self.0.clone()]).table()
    }

    /// Render the AI agent as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        describe_text(&self.0, ansi)
    }
}

/// Read an AI agent config from a YAML or JSON file, or from stdin
///
/// # Arguments
///
/// * `path` - The path to the config, or `-` for stdin
fn read_config(path: &Path) -> Result<AgentConfig, Error> {
    // read the config file, or stdin for `-`
    let source = path.to_string_lossy();
    let raw = spec_io::read_text(&source)?;
    // read the document as YAML, which also reads JSON
    let value: serde_json::Value = serde_norway::from_str(&raw)
        .map_err(|err| Error::new(format!("Failed to parse {source}: {err}")))?;
    // check that the document is an AI agent config
    serde_json::from_value(value)
        .map_err(|err| Error::new(format!("{source} is not a valid AI agent config: {err}")))
}

/// Find an entity of one kind by its ID or name
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind of entity to find
/// * `label` - What the entity is called in errors
/// * `value` - The entity's ID or name
pub(crate) async fn find(
    thorium: &Thorium,
    kind: EntityKinds,
    label: &str,
    value: &str,
) -> Result<Entity, Error> {
    // look in every group the caller can see
    find_in(thorium, kind, label, value, &[]).await
}

/// Find an entity of one kind by its ID or name, looking only in some groups
///
/// A name that is also used in a group outside `groups` is not ambiguous.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind of entity to find
/// * `label` - What the entity is called in errors
/// * `value` - The entity's ID or name
/// * `groups` - The groups to look in (every group the caller can see when empty)
pub(crate) async fn find_in(
    thorium: &Thorium,
    kind: EntityKinds,
    label: &str,
    value: &str,
    groups: &[String],
) -> Result<Entity, Error> {
    // an ID is read directly, and must be of the right kind
    if let Ok(id) = Uuid::parse_str(value) {
        let found = thorium.entities.get(id).await?;
        if found.kind != kind {
            return Err(Error::new(kind_refusal(value, found.kind, label)));
        }
        return Ok(found);
    }
    // otherwise find the entities of this kind with this name
    let opts = EntityListOpts::default()
        .groups(groups.to_vec())
        .kind(kind)
        .limit(10_000);
    let mut cursor = thorium.entities.list(&opts).await?;
    let mut matches = Vec::new();
    loop {
        // keep the entities with this exact name
        matches.extend(
            cursor
                .data
                .drain(..)
                .filter(|line| line.name == value)
                .map(|line| line.id),
        );
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        // get the next page
        cursor.refill().await?;
    }
    // the name must name exactly one entity
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [id] => thorium.entities.get(*id).await,
        ids => Err(Error::new(name_refusal(label, value, ids))),
    }
}

/// Explain why an ID names an entity of the wrong kind
///
/// # Arguments
///
/// * `value` - The ID that was looked up
/// * `found` - The kind of entity the ID names
/// * `label` - What the wanted entity is called in errors, with its article
fn kind_refusal(value: &str, found: EntityKinds, label: &str) -> String {
    // name the found kind in words with its article
    format!("{value} is {}, not {label}", kind_label(found))
}

/// Name a kind of entity in words, with its article
///
/// # Arguments
///
/// * `kind` - The kind to name
fn kind_label(kind: EntityKinds) -> String {
    // the AI kinds have their own names, and every other kind splits into lower case words
    let noun = match kind {
        EntityKinds::Agent => "AI agent".to_owned(),
        EntityKinds::ToolSet => "toolset".to_owned(),
        EntityKinds::ToolSetInstance => "toolset instance".to_owned(),
        other => {
            let mut words = String::new();
            for letter in other.to_string().chars() {
                // start a new word at each capital after the first
                if letter.is_uppercase() && !words.is_empty() {
                    words.push(' ');
                }
                words.push(letter.to_ascii_lowercase());
            }
            words
        }
    };
    // a noun starting with a vowel takes "an"
    let article = if noun.starts_with(['a', 'e', 'i', 'o', 'u', 'A', 'E', 'I', 'O', 'U']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {noun}")
}

/// Explain why a name doesn't name exactly one entity of a kind
///
/// # Arguments
///
/// * `label` - What the entity is called in errors, with its article
/// * `value` - The name that was looked up
/// * `ids` - The entities of the kind with that name, other than exactly one
fn name_refusal(label: &str, value: &str, ids: &[Uuid]) -> String {
    // these sentences use the label's noun without its article
    let noun = label
        .strip_prefix("an ")
        .or_else(|| label.strip_prefix("a "))
        .unwrap_or(label);
    // no entity has the name, or several do
    if ids.is_empty() {
        format!("No {noun} named {value} was found")
    } else {
        format!(
            "Several of your entities named {value} are {noun}s; use one of their IDs: {}",
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

/// Build an AI agent update that only adds edges
///
/// # Arguments
///
/// * `add_tools` - The tools to add
/// * `add_tool_sets` - The toolsets to add
/// * `add_sub_agents` - The sub-agents to add
/// * `extend_target_groups` - Whether to widen targets that miss some of the agent's groups
fn edge_update(
    add_tools: Vec<Uuid>,
    add_tool_sets: Vec<Uuid>,
    add_sub_agents: Vec<Uuid>,
    extend_target_groups: bool,
) -> EntityUpdate {
    // an update with only the added edges
    EntityUpdate::default().metadata(EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools,
        remove_tools: Vec::new(),
        add_tool_sets,
        remove_tool_sets: Vec::new(),
        add_sub_agents,
        remove_sub_agents: Vec::new(),
        extend_target_groups,
    })
}

/// Build an AI agent update that only removes edges
///
/// # Arguments
///
/// * `remove_tools` - The tools to stop calling
/// * `remove_tool_sets` - The toolsets to stop calling
/// * `remove_sub_agents` - The sub-agents to stop delegating to
fn removal_update(
    remove_tools: Vec<Uuid>,
    remove_tool_sets: Vec<Uuid>,
    remove_sub_agents: Vec<Uuid>,
) -> EntityUpdate {
    // an update with only the removed edges
    EntityUpdate::default().metadata(EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: Vec::new(),
        remove_tools,
        add_tool_sets: Vec::new(),
        remove_tool_sets,
        add_sub_agents: Vec::new(),
        remove_sub_agents,
        extend_target_groups: false,
    })
}

/// Build an AI agent update that only clears the harness
fn clear_harness_update() -> EntityUpdate {
    // an update that removes the harness edge and changes nothing else
    EntityUpdate::default().metadata(EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: Some(true),
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    })
}

/// Whether an edge target is named by an ID or a name
///
/// # Arguments
///
/// * `target` - The entity at the end of an edge
/// * `value` - The ID or name a command was given
fn is_named(target: &Entity, value: &str) -> bool {
    // an edge target is named by its ID or its name
    target.name == value || target.id.to_string() == value
}

/// Find the edge to a tool an AI agent has, at its latest version or pinned to one
///
/// # Arguments
///
/// * `agent` - The AI agent, with its edges filled in
/// * `tool` - The ID or name of the tool
/// * `pin` - The tool instance the edge is pinned to, if it is
fn tool_edge(agent: &AgentEntity, tool: &str, pin: Option<Uuid>) -> Result<Uuid, String> {
    // a pin names a version of the tool the agent is pinned to
    if let Some(pin) = pin {
        let found = agent.pinned_tools.iter().find(|target| {
            target.id == pin
                && match &target.metadata {
                    EntityMetadata::ToolInstance(meta) => {
                        meta.tool.to_string() == tool || ToolRef::from_entity(target).tool == tool
                    }
                    _ => false,
                }
        });
        return found.map(|target| target.id).ok_or_else(|| {
            format!("The AI agent is not pinned to {pin} as a version of tool {tool}")
        });
    }
    // otherwise the agent follows the tool at its latest version
    if let Some(target) = agent
        .latest_tools
        .iter()
        .find(|target| is_named(target, tool))
    {
        return Ok(target.id);
    }
    // a pinned tool needs its pin to be named
    let pinned = agent
        .pinned_tools
        .iter()
        .any(|target| ToolRef::from_entity(target).tool == tool);
    if pinned {
        Err(format!(
            "The AI agent calls tool {tool} pinned to a version, not at its latest version; name the version's ID with --pin"
        ))
    } else {
        Err(format!("The AI agent does not call tool {tool}"))
    }
}

/// Find the edge to a toolset an AI agent has, at its latest instance or pinned to one
///
/// # Arguments
///
/// * `agent` - The AI agent, with its edges filled in
/// * `toolset` - The ID or name of the toolset
/// * `pin` - The toolset instance the edge is pinned to, if it is
fn toolset_edge(agent: &AgentEntity, toolset: &str, pin: Option<Uuid>) -> Result<Uuid, String> {
    // a pin names an instance of the toolset the agent is pinned to
    if let Some(pin) = pin {
        let found = agent.tool_sets.iter().find(|target| {
            target.id == pin
                && target.kind == EntityKinds::ToolSetInstance
                && match &target.metadata {
                    EntityMetadata::ToolSetInstance(meta) => {
                        meta.tool_set.to_string() == toolset
                            || ToolSetRef::from_entity(target).toolset == toolset
                    }
                    _ => false,
                }
        });
        return found.map(|target| target.id).ok_or_else(|| {
            format!("The AI agent is not pinned to {pin} as an instance of toolset {toolset}")
        });
    }
    // otherwise the agent follows the toolset at its latest instance
    if let Some(target) = agent
        .tool_sets
        .iter()
        .find(|target| target.kind == EntityKinds::ToolSet && is_named(target, toolset))
    {
        return Ok(target.id);
    }
    // a pinned toolset needs its pin to be named
    let pinned = agent.tool_sets.iter().any(|target| {
        target.kind == EntityKinds::ToolSetInstance
            && ToolSetRef::from_entity(target).toolset == toolset
    });
    if pinned {
        Err(format!(
            "The AI agent calls toolset {toolset} pinned to an instance, not at its latest instance; name the instance's ID with --pin"
        ))
    } else {
        Err(format!("The AI agent does not call toolset {toolset}"))
    }
}

/// Find the edge to a sub-agent an AI agent has
///
/// # Arguments
///
/// * `agent` - The AI agent, with its edges filled in
/// * `sub_agent` - The ID or name of the sub-agent
fn sub_agent_edge(agent: &AgentEntity, sub_agent: &str) -> Result<Uuid, String> {
    // the sub-agent is named by ID or name
    agent
        .sub_agents
        .iter()
        .find(|target| is_named(target, sub_agent))
        .map(|target| target.id)
        .ok_or_else(|| format!("The AI agent does not delegate to {sub_agent}"))
}

/// Get an AI agent's metadata, refusing an entity that isn't one
///
/// # Arguments
///
/// * `agent` - The entity to read
fn require_meta(agent: &Entity) -> Result<&AgentEntity, Error> {
    // an agent found by kind always has agent metadata
    agent_meta(agent).ok_or_else(|| Error::new(format!("{} is not an AI agent", agent.name)))
}

/// List AI agents
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetAgents) -> Result<(), Error> {
    // list AI agents in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // list the details of the AI agents in the requested groups
        let opts = EntityListOpts::default()
            .groups(cmd.group.clone())
            .kind(EntityKinds::Agent)
            .limit(cmd.limit);
        let mut cursor = thorium.entities.list_details(&opts).await?;
        // read pages until the limit or the last AI agent
        let mut agents = std::mem::take(&mut cursor.data);
        while !cursor.exhausted() {
            cursor.refill().await?;
            agents.append(&mut cursor.data);
        }
        Ok(AgentsView(agents))
    })
    .await
}

/// Describe an AI agent
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeAgent) -> Result<(), Error> {
    // describe the AI agent in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // find the AI agent with its edges filled in
        let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
        Ok(AgentView(agent))
    })
    .await
}

/// Create an AI agent from a config that was already read
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command to execute
/// * `config` - The AI agent's config
async fn create(thorium: &Thorium, cmd: &CreateAgent, config: AgentConfig) -> Result<(), Error> {
    // build the AI agent request with its config
    let mut agent = AgentEntityRequest::new(config);
    // find each tool the AI agent calls, at latest or pinned to a version
    for tool in &cmd.tool {
        let target = ToolRef::parse(tool)
            .resolve(thorium, &cmd.group, "a tool")
            .await?;
        agent = agent.tool(target);
    }
    // find each toolset the AI agent calls, at latest or pinned to an instance
    for toolset in &cmd.toolset {
        let target = ToolSetRef::parse(toolset)
            .resolve(thorium, &cmd.group)
            .await?;
        agent = agent.tool_set(target);
    }
    // find each AI agent it delegates to
    for sub_agent in &cmd.sub_agent {
        agent = agent.sub_agent(refs::resolve_agent(thorium, &cmd.group, sub_agent).await?);
    }
    // find the harness it runs in
    if let Some(harness) = &cmd.harness {
        let target = ToolRef::parse(harness)
            .resolve(thorium, &cmd.group, "a harness")
            .await?;
        agent = agent.harness(target);
    }
    // limit the levels that may supply model auth
    if !cmd.model_auth.is_empty() {
        agent = agent.model_auth(ModelAuthSource {
            levels: cmd.model_auth.iter().copied().map(Into::into).collect(),
        });
    }
    // widen the targets of the edges only when asked
    if cmd.extend_target_groups {
        agent = agent.extend_target_groups();
    }
    // create the AI agent
    let mut req = EntityRequest::new(
        &cmd.name,
        EntityMetadataRequest::Agent(agent),
        cmd.group.clone(),
    );
    req.description.clone_from(&cmd.description);
    let resp = thorium.entities.create(req).await?;
    println!("Created AI agent {} ({})", resp.name, resp.id);
    Ok(())
}

/// Let an AI agent call a tool at its latest version or pinned to one version
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The link-tool command to execute
async fn link_tool(thorium: &Thorium, cmd: &LinkTool) -> Result<(), Error> {
    // find the AI agent and the tool
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let tool = find(thorium, EntityKinds::Tool, "a tool", &cmd.tool).await?;
    // a pinned version must be one of the tool's instances
    let (target, how) = match cmd.pin {
        Some(pin) => {
            let instance = thorium.entities.get(pin).await?;
            match &instance.metadata {
                EntityMetadata::ToolInstance(meta) if meta.tool == tool.id => {
                    (pin, format!("pinned to {}", instance.name))
                }
                _ => {
                    return Err(Error::new(format!(
                        "{pin} is not a version of tool {}",
                        tool.name
                    )));
                }
            }
        }
        None => (tool.id, "at its latest version".to_owned()),
    };
    // add the call edge
    let update = edge_update(
        vec![target],
        Vec::new(),
        Vec::new(),
        cmd.extend_target_groups,
    );
    thorium.entities.update(agent.id, update).await?;
    println!("AI agent {} now calls {} {how}", agent.name, tool.name);
    Ok(())
}

/// Let an AI agent delegate to another AI agent
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delegate command to execute
async fn delegate(thorium: &Thorium, cmd: &Delegate) -> Result<(), Error> {
    // find the AI agent
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    // an ID is sent as is so the API checks its kind; a name must be an AI agent
    let sub_agent = match Uuid::parse_str(&cmd.sub_agent) {
        Ok(id) => id,
        Err(_) => {
            find(thorium, EntityKinds::Agent, "an AI agent", &cmd.sub_agent)
                .await?
                .id
        }
    };
    // add the delegation edge
    let update = edge_update(
        Vec::new(),
        Vec::new(),
        vec![sub_agent],
        cmd.extend_target_groups,
    );
    thorium.entities.update(agent.id, update).await?;
    println!("AI agent {} now delegates to {}", agent.name, cmd.sub_agent);
    Ok(())
}

/// Run an AI agent in a harness at its latest version or pinned to one version
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The set-harness command to execute
async fn set_harness(thorium: &Thorium, cmd: &SetHarness) -> Result<(), Error> {
    // find the AI agent and the harness tool
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let harness = find(thorium, EntityKinds::Tool, "a harness", &cmd.harness).await?;
    // a pinned version must be one of the harness's instances
    let (target, how) = match cmd.pin {
        Some(pin) => {
            let instance = thorium.entities.get(pin).await?;
            match &instance.metadata {
                EntityMetadata::ToolInstance(meta) if meta.tool == harness.id => {
                    (pin, format!("pinned to {}", instance.name))
                }
                _ => {
                    return Err(Error::new(format!(
                        "{pin} is not a version of harness {}",
                        harness.name
                    )));
                }
            }
        }
        None => (harness.id, "at its latest version".to_owned()),
    };
    // add the harness edge, which the API refuses when the agent already runs in one
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: Some(target),
        clear_harness: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: cmd.extend_target_groups,
    });
    thorium.entities.update(agent.id, update).await?;
    println!("AI agent {} now runs in {} {how}", agent.name, harness.name);
    Ok(())
}

/// Let an AI agent call a toolset at its latest instance or pinned to one instance
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The link-toolset command to execute
async fn link_toolset(thorium: &Thorium, cmd: &LinkToolset) -> Result<(), Error> {
    // find the AI agent and the toolset
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let tool_set = find(thorium, EntityKinds::ToolSet, "a toolset", &cmd.toolset).await?;
    // a pinned instance must be one of the toolset's instances
    let (target, how) = match cmd.pin {
        Some(pin) => {
            let instance = thorium.entities.get(pin).await?;
            match &instance.metadata {
                EntityMetadata::ToolSetInstance(meta) if meta.tool_set == tool_set.id => {
                    (pin, format!("pinned to {}", instance.name))
                }
                _ => {
                    return Err(Error::new(format!(
                        "{pin} is not an instance of toolset {}",
                        tool_set.name
                    )));
                }
            }
        }
        None => (tool_set.id, "at its latest instance".to_owned()),
    };
    // add the call edge
    let update = edge_update(
        Vec::new(),
        vec![target],
        Vec::new(),
        cmd.extend_target_groups,
    );
    thorium.entities.update(agent.id, update).await?;
    println!(
        "AI agent {} now calls toolset {} {how}",
        agent.name, tool_set.name
    );
    Ok(())
}

/// Stop an AI agent calling a tool
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The unlink-tool command to execute
async fn unlink_tool(thorium: &Thorium, cmd: &UnlinkTool) -> Result<(), Error> {
    // find the AI agent and the edge to remove
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let target = tool_edge(require_meta(&agent)?, &cmd.tool, cmd.pin)
        .map_err(|err| Error::new(format!("{}: {err}", agent.name)))?;
    // remove the call edge
    let update = removal_update(vec![target], Vec::new(), Vec::new());
    thorium.entities.update(agent.id, update).await?;
    println!("AI agent {} no longer calls {}", agent.name, cmd.tool);
    Ok(())
}

/// Stop an AI agent calling a toolset
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The unlink-toolset command to execute
async fn unlink_toolset(thorium: &Thorium, cmd: &UnlinkToolset) -> Result<(), Error> {
    // find the AI agent and the edge to remove
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let target = toolset_edge(require_meta(&agent)?, &cmd.toolset, cmd.pin)
        .map_err(|err| Error::new(format!("{}: {err}", agent.name)))?;
    // remove the call edge
    let update = removal_update(Vec::new(), vec![target], Vec::new());
    thorium.entities.update(agent.id, update).await?;
    println!(
        "AI agent {} no longer calls toolset {}",
        agent.name, cmd.toolset
    );
    Ok(())
}

/// Stop an AI agent delegating to another AI agent
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The undelegate command to execute
async fn undelegate(thorium: &Thorium, cmd: &Undelegate) -> Result<(), Error> {
    // find the AI agent and the edge to remove
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let target = sub_agent_edge(require_meta(&agent)?, &cmd.sub_agent)
        .map_err(|err| Error::new(format!("{}: {err}", agent.name)))?;
    // remove the delegation edge
    let update = removal_update(Vec::new(), Vec::new(), vec![target]);
    thorium.entities.update(agent.id, update).await?;
    println!(
        "AI agent {} no longer delegates to {}",
        agent.name, cmd.sub_agent
    );
    Ok(())
}

/// Stop an AI agent running in its harness
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The clear-harness command to execute
async fn clear_harness(thorium: &Thorium, cmd: &ClearHarness) -> Result<(), Error> {
    // find the AI agent, which must run in a harness
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    if require_meta(&agent)?.harness.is_none() {
        return Err(Error::new(format!(
            "{}: The AI agent does not run in a harness",
            agent.name
        )));
    }
    // remove the harness edge
    thorium
        .entities
        .update(agent.id, clear_harness_update())
        .await?;
    println!("AI agent {} no longer runs in a harness", agent.name);
    Ok(())
}

/// Edit an AI agent in an editor, or from `--file`, `--set`, `--unset` and `--patch`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The edit command to execute
async fn edit_agent(thorium: &Thorium, conf: &CtlConf, cmd: &EditAgent) -> Result<(), Error> {
    // find the AI agent
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let target = AgentTarget {
        id: agent.id,
        extend_target_groups: cmd.extend_target_groups,
    };
    // run the shared edit flow, which prints the result
    edit_with::<AgentEditable>(thorium, conf, &target, &cmd.edit).await?;
    Ok(())
}

/// Update an AI agent from flags, `--file`, `--set`, `--unset` and `--patch`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command to execute
async fn update_agent(thorium: &Thorium, cmd: &UpdateAgent) -> Result<(), Error> {
    // find the AI agent
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let target = AgentTarget {
        id: agent.id,
        extend_target_groups: cmd.extend_target_groups,
    };
    // turn the typed flags into edits that apply before the `--set` and `--unset` edits
    let view = AgentEditable::to_view(&agent);
    let mut args = cmd.edit.clone();
    let mut ops = edit::typed_ops(cmd, &view)?;
    ops.append(&mut args.ops);
    args.ops = ops;
    // run the shared update flow, which prints the result
    update_with::<AgentEditable>(thorium, &target, &args).await?;
    Ok(())
}

/// Print an AI agent as a spec
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The spec command to execute
async fn spec(thorium: &Thorium, cmd: &SpecAgent) -> Result<(), Error> {
    // find the AI agent and build its view
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    let view = AgentEditable::to_view(&agent);
    // write the view as JSON or, by default, YAML
    let text = match cmd.format.format {
        Some(OutputFormat::Json) => utils::curated_json(&view, AgentEditable::FIELD_ORDER)?,
        _ => utils::curated_yaml(&view, AgentEditable::FIELD_ORDER)?,
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

/// Ask whether to delete an entity, unless the question was skipped
///
/// # Arguments
///
/// * `what` - What the entity is, for the prompt (`AI agent`)
/// * `name` - The entity's name
/// * `skip_confirm` - Whether `--skip-confirm` was given
pub(crate) fn confirm_delete(what: &str, name: &str, skip_confirm: bool) -> Result<bool, Error> {
    // the flag skips the question
    if skip_confirm {
        return Ok(true);
    }
    // fail clearly, not with a raw dialoguer error, when there is no terminal to ask on
    utils::require_confirm_terminal("--skip-confirm")?;
    // deleting is irreversible, so the default answer is no
    dialoguer::Confirm::new()
        .with_prompt(format!("Delete {what} {name}?"))
        .default(false)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))
}

/// Delete an AI agent
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete command to execute
async fn delete_agent(thorium: &Thorium, cmd: &DeleteAgent) -> Result<(), Error> {
    // find the AI agent
    let agent = find(thorium, EntityKinds::Agent, "an AI agent", &cmd.agent).await?;
    // a dry run only says what would be deleted
    if cmd.dry_run {
        println!(
            "Dry run: would delete AI agent {} ({})",
            agent.name, agent.id
        );
        return Ok(());
    }
    // confirm the deletion
    if !confirm_delete("AI agent", &agent.name, cmd.skip_confirm)? {
        println!("Cancelled.");
        return Ok(());
    }
    // delete the AI agent, which the API refuses while something still needs it
    thorium
        .entities
        .delete(agent.id)
        .await
        .map_err(utils::delete_refusal)?;
    println!("Deleted AI agent {} ({})", agent.name, agent.id);
    Ok(())
}

/// Handle all AI agent commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The agents command to execute
pub async fn handle(args: &Args, cmd: &Agents) -> Result<(), Error> {
    // read a create's config before making any request
    let config = match cmd {
        Agents::Create(create) => Some(read_config(&create.config)?),
        _ => None,
    };
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // call the right agents handler
    let result = match (cmd, config) {
        (Agents::Get(cmd), _) => get(&thorium, cmd).await,
        (Agents::Describe(cmd), _) => describe(&thorium, cmd).await,
        (Agents::Create(cmd), Some(config)) => create(&thorium, cmd, config).await,
        (Agents::Create(_), None) => Err(Error::new("the AI agent config wasn't read")),
        (Agents::LinkTool(cmd), _) => link_tool(&thorium, cmd).await,
        (Agents::Delegate(cmd), _) => delegate(&thorium, cmd).await,
        (Agents::LinkToolset(cmd), _) => link_toolset(&thorium, cmd).await,
        (Agents::SetHarness(cmd), _) => set_harness(&thorium, cmd).await,
        (Agents::UnlinkTool(cmd), _) => unlink_tool(&thorium, cmd).await,
        (Agents::UnlinkToolset(cmd), _) => unlink_toolset(&thorium, cmd).await,
        (Agents::Undelegate(cmd), _) => undelegate(&thorium, cmd).await,
        (Agents::ClearHarness(cmd), _) => clear_harness(&thorium, cmd).await,
        (Agents::Update(cmd), _) => update_agent(&thorium, cmd).await,
        (Agents::Edit(cmd), _) => edit_agent(&thorium, &conf, cmd).await,
        (Agents::Spec(cmd), _) => spec(&thorium, cmd).await,
        (Agents::Delete(cmd), _) => delete_agent(&thorium, cmd).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use super::fixtures::{agent, tool, tool_instance, toolset};
    use super::{
        clear_harness_update, kind_refusal, name_refusal, sub_agent_edge, tool_edge, toolset_edge,
    };
    use thorium::models::{AgentEntity, EntityKinds, EntityMetadata, EntityMetadataUpdate};
    use uuid::Uuid;

    /// Get an agent's metadata from a test entity
    ///
    /// # Arguments
    ///
    /// * `entity` - The entity to read
    fn meta(entity: &thorium::models::Entity) -> &AgentEntity {
        // fixtures always build agents
        match &entity.metadata {
            EntityMetadata::Agent(meta) => meta,
            _ => panic!("expected an agent"),
        }
    }

    /// Unlinking a tool finds the latest edge by name or ID and the pinned edge by its instance
    #[test]
    fn tool_edges_are_found() {
        // an agent that follows one tool and is pinned to a version of another
        let followed = tool("yara");
        let other = tool("capa");
        let pinned = tool_instance(&other, "1.0.0");
        let lead = agent("lead", |meta| {
            meta.latest_tools.push(followed.clone());
            meta.pinned_tools.push(pinned.clone());
        });
        // the latest edge is found by name or ID
        assert_eq!(tool_edge(meta(&lead), "yara", None).unwrap(), followed.id);
        let id = followed.id.to_string();
        assert_eq!(tool_edge(meta(&lead), &id, None).unwrap(), followed.id);
        // the pinned edge is found by its instance and the tool's name or ID
        assert_eq!(
            tool_edge(meta(&lead), "capa", Some(pinned.id)).unwrap(),
            pinned.id
        );
        let other_id = other.id.to_string();
        assert_eq!(
            tool_edge(meta(&lead), &other_id, Some(pinned.id)).unwrap(),
            pinned.id
        );
        // a tool the agent doesn't call is refused
        let err = tool_edge(meta(&lead), "nmap", None).unwrap_err();
        assert!(err.contains("nmap"), "{err}");
        // a pinned tool needs its pin, and says so
        let err = tool_edge(meta(&lead), "capa", None).unwrap_err();
        assert!(err.contains("--pin"), "{err}");
        // a pin to another tool's instance is refused
        let err = tool_edge(meta(&lead), "yara", Some(pinned.id)).unwrap_err();
        assert!(err.contains("yara"), "{err}");
    }

    /// Unlinking a toolset or sub-agent finds the edge or says there isn't one
    #[test]
    fn toolset_and_sub_agent_edges_are_found() {
        // an agent that calls a toolset and delegates to another agent
        let kit = toolset("ghidra");
        let scout = agent("scout", |_| {});
        let lead = agent("lead", |meta| {
            meta.tool_sets.push(kit.clone());
            meta.sub_agents.push(scout.clone());
        });
        // the toolset and the sub-agent are found by name or ID
        assert_eq!(toolset_edge(meta(&lead), "ghidra", None).unwrap(), kit.id);
        assert_eq!(
            sub_agent_edge(meta(&lead), &scout.id.to_string()).unwrap(),
            scout.id
        );
        assert_eq!(sub_agent_edge(meta(&lead), "scout").unwrap(), scout.id);
        // anything else is refused by name
        let err = toolset_edge(meta(&lead), "angr", None).unwrap_err();
        assert!(err.contains("angr"), "{err}");
        let err = sub_agent_edge(meta(&lead), "ghost").unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    /// Clearing the harness sends only the clear flag
    #[test]
    fn clear_harness_changes_only_the_harness() {
        // the update clears the harness and nothing else
        let update = clear_harness_update();
        let Some(EntityMetadataUpdate::Agent {
            harness,
            clear_harness,
            add_tools,
            remove_tools,
            remove_tool_sets,
            remove_sub_agents,
            config,
            ..
        }) = update.metadata
        else {
            panic!("expected an agent update");
        };
        assert_eq!(clear_harness, Some(true));
        assert!(harness.is_none() && config.is_none());
        assert!(add_tools.is_empty() && remove_tools.is_empty());
        assert!(remove_tool_sets.is_empty() && remove_sub_agents.is_empty());
    }

    /// An ID of the wrong kind is refused with readable kind names and their articles
    #[test]
    fn kind_refusals_read_naturally() {
        // an agent where a tool was wanted
        assert_eq!(
            kind_refusal("abc", EntityKinds::Agent, "a tool"),
            "abc is an AI agent, not a tool"
        );
        // a tool instance where an agent was wanted
        assert_eq!(
            kind_refusal("abc", EntityKinds::ToolInstance, "an AI agent"),
            "abc is a tool instance, not an AI agent"
        );
        // a toolset instance where a toolset was wanted
        assert_eq!(
            kind_refusal("abc", EntityKinds::ToolSetInstance, "a toolset"),
            "abc is a toolset instance, not a toolset"
        );
        // any other kind gets its words in lower case after the right article
        assert_eq!(
            kind_refusal("abc", EntityKinds::Incident, "a toolset"),
            "abc is an incident, not a toolset"
        );
    }

    /// A name lookup's refusals read naturally for a label with its article
    #[test]
    fn name_refusals_drop_the_article() {
        // a name nobody has
        assert_eq!(
            name_refusal("an AI agent", "grabber", &[]),
            "No AI agent named grabber was found"
        );
        assert_eq!(
            name_refusal("a toolset", "kit", &[]),
            "No toolset named kit was found"
        );
        // a name several entities share
        let ids = [Uuid::nil(), Uuid::max()];
        assert_eq!(
            name_refusal("an AI agent", "grabber", &ids),
            format!(
                "Several of your entities named grabber are AI agents; use one of their IDs: {}, {}",
                ids[0], ids[1]
            )
        );
    }
}
