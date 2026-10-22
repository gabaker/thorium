//! Handles toolset commands

use serde::Serialize;
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, ToolSetEntity, ToolSetEntityRequest,
    ToolSetSelector,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::toolsets::{
    CreateToolSet, DescribeToolSet, GetToolSets, IncludeTool, RemoveTool, SetReadOnly, SetSelector,
    Switch, ToolSets,
};
use crate::handlers::agents::find;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::watch;

/// Get a toolset's metadata, if the entity is a toolset
///
/// # Arguments
///
/// * `tool_set` - The entity to read
fn tool_set_meta(tool_set: &Entity) -> Option<&ToolSetEntity> {
    // only toolsets have toolset metadata
    match &tool_set.metadata {
        EntityMetadata::ToolSet(meta) => Some(meta),
        _ => None,
    }
}

/// Describe how a toolset includes a tool: at its latest version or pinned
///
/// # Arguments
///
/// * `tool` - The included tool or tool instance
fn included(tool: &Entity) -> String {
    // a tool instance is a pin and a tool follows its latest version
    let how = if tool.kind == EntityKinds::ToolInstance {
        "pinned"
    } else {
        "latest"
    };
    format!("{} ({}, {how})", tool.name, tool.id)
}

/// Show an optional selector, or `-` when there is none
///
/// # Arguments
///
/// * `selector` - The selector to show
fn selector(selector: Option<&ToolSetSelector>) -> String {
    // a toolset without a selector has only explicit members
    selector.map_or_else(|| "-".to_owned(), ToString::to_string)
}

/// The toolsets one command lists
#[derive(Serialize)]
#[serde(transparent)]
struct ToolSetsView(
    /// The toolsets as the API returned them
    Vec<Entity>,
);

impl View for ToolSetsView {
    /// Render the toolsets as a table
    fn table(&self) -> String {
        // build one row per toolset
        let rows = self
            .0
            .iter()
            .map(|tool_set| {
                let meta = tool_set_meta(tool_set);
                vec![
                    tool_set.name.clone(),
                    tool_set.id.to_string(),
                    meta.map_or(0, |meta| meta.tools.len()).to_string(),
                    selector(meta.and_then(|meta| meta.selector.as_ref())),
                    meta.is_some_and(|meta| meta.read_only).to_string(),
                    meta.and_then(|meta| meta.latest_instance)
                        .map_or_else(|| "-".to_owned(), |id| id.to_string()),
                    tool_set.groups.join(", "),
                ]
            })
            .collect();
        output::table_from(
            &[
                "NAME",
                "ID",
                "TOOLS",
                "SELECTOR",
                "READ-ONLY",
                "LATEST INSTANCE",
                "GROUPS",
            ],
            rows,
        )
    }

    /// Render the toolsets as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each toolset, separated by rules
        self.0
            .iter()
            .map(|tool_set| describe_text(tool_set, ansi))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// Render a toolset's details as text
///
/// # Arguments
///
/// * `tool_set` - The toolset to describe
/// * `ansi` - Whether to style the text
fn describe_text(tool_set: &Entity, ansi: bool) -> String {
    // start with the toolset's name and groups
    let mut lines = vec![render::header(
        &tool_set.name,
        &tool_set.groups.join(", "),
        ansi,
    )];
    lines.push(render::field("ID", &tool_set.id.to_string(), ansi));
    lines.push(render::field("Groups", &tool_set.groups.join(", "), ansi));
    // add the settings, included tools, and latest instance
    if let Some(meta) = tool_set_meta(tool_set) {
        lines.push(render::field(
            "Selector",
            &selector(meta.selector.as_ref()),
            ansi,
        ));
        lines.push(render::field(
            "Read only",
            &meta.read_only.to_string(),
            ansi,
        ));
        lines.push(render::field(
            "Auto instance",
            &meta.auto_instance.to_string(),
            ansi,
        ));
        lines.push(render::field("Modified", &meta.modified.to_rfc3339(), ansi));
        let tools = if meta.tools.is_empty() {
            "-".to_owned()
        } else {
            meta.tools
                .iter()
                .map(included)
                .collect::<Vec<String>>()
                .join(", ")
        };
        lines.push(render::field("Tools", &tools, ansi));
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
        tool_set.description.as_deref().unwrap_or("-"),
        ansi,
    ));
    lines.join("\n")
}

/// One toolset described in full
#[derive(Serialize)]
#[serde(transparent)]
struct ToolSetView(
    /// The toolset as the API returned it
    Entity,
);

impl View for ToolSetView {
    /// Render the toolset as a one row table
    fn table(&self) -> String {
        // reuse the list table for a single toolset
        ToolSetsView(vec![self.0.clone()]).table()
    }

    /// Render the toolset as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        describe_text(&self.0, ansi)
    }
}

/// Build a toolset update that changes only the given fields
///
/// # Arguments
///
/// * `change` - Sets the fields of an empty toolset update
fn toolset_update(change: impl FnOnce(&mut EntityMetadataUpdate)) -> EntityUpdate {
    // start from an update that changes nothing
    let mut update = EntityMetadataUpdate::ToolSet {
        read_only: None,
        selector: None,
        clear_selector: None,
        auto_instance: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        extend_target_groups: false,
    };
    change(&mut update);
    EntityUpdate::default().metadata(update)
}

/// Check that a tool instance is a version of a tool
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `tool` - The tool
/// * `pin` - The tool instance that must be one of the tool's versions
async fn version_of(thorium: &Thorium, tool: &Entity, pin: Uuid) -> Result<Entity, Error> {
    // read the instance and check its tool
    let instance = thorium.entities.get(pin).await?;
    match &instance.metadata {
        EntityMetadata::ToolInstance(meta) if meta.tool == tool.id => Ok(instance),
        _ => Err(Error::new(format!(
            "{pin} is not a version of tool {}",
            tool.name
        ))),
    }
}

/// List toolsets
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetToolSets) -> Result<(), Error> {
    // list toolsets in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // list the details of the toolsets in the requested groups
        let opts = EntityListOpts::default()
            .groups(cmd.group.clone())
            .kind(EntityKinds::ToolSet)
            .limit(cmd.limit);
        let mut cursor = thorium.entities.list_details(&opts).await?;
        // read pages until the limit or the last toolset
        let mut tool_sets = std::mem::take(&mut cursor.data);
        while !cursor.exhausted() {
            cursor.refill().await?;
            tool_sets.append(&mut cursor.data);
        }
        Ok(ToolSetsView(tool_sets))
    })
    .await
}

/// Describe a toolset
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeToolSet) -> Result<(), Error> {
    // describe the toolset in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // find the toolset with its tools and latest instance filled in
        let tool_set = find(thorium, EntityKinds::ToolSet, "a toolset", &cmd.toolset).await?;
        Ok(ToolSetView(tool_set))
    })
    .await
}

/// Create a toolset
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command to execute
async fn create(thorium: &Thorium, cmd: &CreateToolSet) -> Result<(), Error> {
    // find each tool to include at latest
    let mut req = ToolSetEntityRequest::default();
    for tool in &cmd.tool {
        req = req.tool(find(thorium, EntityKinds::Tool, "a tool", tool).await?.id);
    }
    // pin each tool instance as it was given
    for pin in &cmd.pin {
        req = req.tool(*pin);
    }
    // set the selector and settings
    if let Some(selector) = &cmd.selector {
        req = req.selector(selector.clone());
    }
    if cmd.read_only {
        req = req.read_only(true);
    }
    if cmd.no_auto_instance {
        req = req.auto_instance(false);
    }
    if cmd.extend_target_groups {
        req = req.extend_target_groups();
    }
    // create the toolset
    let mut entity = EntityRequest::new(
        &cmd.name,
        EntityMetadataRequest::ToolSet(req),
        cmd.group.clone(),
    );
    entity.description.clone_from(&cmd.description);
    let resp = thorium.entities.create(entity).await?;
    println!("Created toolset {} ({})", resp.name, resp.id);
    Ok(())
}

/// Include a tool in a toolset at its latest version or pinned to one version
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The include command to execute
async fn include(thorium: &Thorium, cmd: &IncludeTool) -> Result<(), Error> {
    // find the toolset and the tool
    let tool_set = find(thorium, EntityKinds::ToolSet, "a toolset", &cmd.toolset).await?;
    let tool = find(thorium, EntityKinds::Tool, "a tool", &cmd.tool).await?;
    // a pinned version must be one of the tool's instances
    let (target, how) = match cmd.pin {
        Some(pin) => {
            let instance = version_of(thorium, &tool, pin).await?;
            (pin, format!("pinned to {}", instance.name))
        }
        None => (tool.id, "at its latest version".to_owned()),
    };
    // add the include edge
    let extend = cmd.extend_target_groups;
    let update = toolset_update(|update| {
        if let EntityMetadataUpdate::ToolSet {
            add_tools,
            extend_target_groups,
            ..
        } = update
        {
            add_tools.push(target);
            *extend_target_groups = extend;
        }
    });
    thorium.entities.update(tool_set.id, update).await?;
    println!("Toolset {} now includes {} {how}", tool_set.name, tool.name);
    Ok(())
}

/// Stop including a tool in a toolset
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The remove-tool command to execute
async fn remove_tool(thorium: &Thorium, cmd: &RemoveTool) -> Result<(), Error> {
    // find the toolset and the tool
    let tool_set = find(thorium, EntityKinds::ToolSet, "a toolset", &cmd.toolset).await?;
    let tool = find(thorium, EntityKinds::Tool, "a tool", &cmd.tool).await?;
    // remove the pin when one is named, otherwise the latest edge
    let (target, how) = match cmd.pin {
        Some(pin) => {
            let instance = version_of(thorium, &tool, pin).await?;
            (pin, format!("pinned to {}", instance.name))
        }
        None => (tool.id, "at its latest version".to_owned()),
    };
    let update = toolset_update(|update| {
        if let EntityMetadataUpdate::ToolSet { remove_tools, .. } = update {
            remove_tools.push(target);
        }
    });
    thorium.entities.update(tool_set.id, update).await?;
    println!(
        "Toolset {} no longer includes {} {how}",
        tool_set.name, tool.name
    );
    Ok(())
}

/// Set or clear a toolset's selector
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The selector command to execute
async fn set_selector(thorium: &Thorium, cmd: &SetSelector) -> Result<(), Error> {
    // find the toolset
    let tool_set = find(thorium, EntityKinds::ToolSet, "a toolset", &cmd.toolset).await?;
    // replace or clear the selector
    let (new, clear) = (cmd.selector.clone(), cmd.clear);
    let update = toolset_update(|update| {
        if let EntityMetadataUpdate::ToolSet {
            selector,
            clear_selector,
            ..
        } = update
        {
            *selector = new;
            *clear_selector = clear.then_some(true);
        }
    });
    thorium.entities.update(tool_set.id, update).await?;
    match &cmd.selector {
        Some(selector) => println!("Toolset {} now selects {selector}", tool_set.name),
        None => println!("Toolset {} no longer has a selector", tool_set.name),
    }
    Ok(())
}

/// Turn a toolset's read-only filter on or off
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The read-only command to execute
async fn set_read_only(thorium: &Thorium, cmd: &SetReadOnly) -> Result<(), Error> {
    // find the toolset
    let tool_set = find(thorium, EntityKinds::ToolSet, "a toolset", &cmd.toolset).await?;
    // set the filter
    let on = cmd.state == Switch::On;
    let update = toolset_update(|update| {
        if let EntityMetadataUpdate::ToolSet { read_only, .. } = update {
            *read_only = Some(on);
        }
    });
    thorium.entities.update(tool_set.id, update).await?;
    let state = if on { "on" } else { "off" };
    println!("Toolset {} read-only filter is {state}", tool_set.name);
    Ok(())
}

/// Handle all toolset commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The toolsets command to execute
pub async fn handle(args: &Args, cmd: &ToolSets) -> Result<(), Error> {
    // load our config and instance our client
    let (_, thorium) = utils::get_client(args).await?;
    // call the right toolsets handler
    let result = match cmd {
        ToolSets::Get(cmd) => get(&thorium, cmd).await,
        ToolSets::Describe(cmd) => describe(&thorium, cmd).await,
        ToolSets::Create(cmd) => create(&thorium, cmd).await,
        ToolSets::Include(cmd) => include(&thorium, cmd).await,
        ToolSets::RemoveTool(cmd) => remove_tool(&thorium, cmd).await,
        ToolSets::Selector(cmd) => set_selector(&thorium, cmd).await,
        ToolSets::ReadOnly(cmd) => set_read_only(&thorium, cmd).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}
