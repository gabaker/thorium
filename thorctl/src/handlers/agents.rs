//! Handles AI agent commands

use serde::Serialize;
use std::path::Path;
use thorium::models::{
    AgentConfig, AgentEntity, AgentEntityRequest, Entity, EntityKinds, EntityListOpts,
    EntityMetadata, EntityMetadataRequest, EntityMetadataUpdate, EntityRequest, EntityUpdate,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::agents::{
    Agents, CreateAgent, Delegate, DescribeAgent, GetAgents, LinkTool, LinkToolset, SetHarness,
};
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::watch;

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

/// Read an AI agent config from a YAML or JSON file
///
/// # Arguments
///
/// * `path` - The path to the config
fn read_config(path: &Path) -> Result<AgentConfig, Error> {
    // read the config file
    let raw = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))?;
    // read the document as YAML, which also reads JSON
    let value: serde_json::Value = serde_norway::from_str(&raw)
        .map_err(|err| Error::new(format!("Failed to parse {}: {err}", path.display())))?;
    // check that the document is an AI agent config
    serde_json::from_value(value).map_err(|err| {
        Error::new(format!(
            "{} is not a valid AI agent config: {err}",
            path.display()
        ))
    })
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
    // an ID is read directly, and must be of the right kind
    if let Ok(id) = Uuid::parse_str(value) {
        let found = thorium.entities.get(id).await?;
        if found.kind != kind {
            return Err(Error::new(kind_refusal(value, found.kind, label)));
        }
        return Ok(found);
    }
    // otherwise find the entities of this kind with this name
    let opts = EntityListOpts::default().kind(kind).limit(10_000);
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
    // build the AI agent request
    let mut req = EntityRequest::new(
        &cmd.name,
        EntityMetadataRequest::Agent(AgentEntityRequest::new(config)),
        cmd.group.clone(),
    );
    req.description.clone_from(&cmd.description);
    // create the AI agent
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
    let (_, thorium) = utils::get_client(args).await?;
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
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use super::{kind_refusal, name_refusal};
    use thorium::models::EntityKinds;
    use uuid::Uuid;

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
