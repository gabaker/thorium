//! Editing toolsets: the view people edit and the update it becomes
//!
//! The view names the included tools instead of carrying IDs. An edit is turned into an update
//! by diffing the edited view against the view of the toolset as it was read; only tools that
//! are new are looked up, and tools that are gone are removed by the IDs the toolset already has.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thorium::models::{
    Entity, EntityMetadata, EntityMetadataUpdate, EntityUpdate, ToolSetEntity,
    ToolSetEntityRequest, ToolSetSelector,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::toolsets::{CreateToolSet, Switch, UpdateToolSet};
use crate::handlers::agents::refs::{ToolRef, diff, duplicate, set_key as set};
use crate::handlers::edit::Editable;
use crate::utils::patch::{EditOp, merge_patch};

#[cfg(test)]
mod tests;

/// The document people edit for a toolset
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetView {
    /// The toolset's ID, shown for context and ignored on save
    #[serde(rename = "*id*", default)]
    pub id: Option<String>,
    /// The toolset's name
    pub name: String,
    /// What the toolset is for
    #[serde(default)]
    pub description: Option<String>,
    /// The groups the toolset is in
    pub groups: Vec<String>,
    /// Whether only tools whose version declares no effects are included
    pub read_only: bool,
    /// The tag (`key=value`) that adds every agentic tool carrying it in all of the toolset's
    /// groups
    #[serde(default)]
    pub selector: Option<String>,
    /// Whether new toolset instances are built automatically when the toolset or its tools change
    pub auto_instance: bool,
    /// The tools the toolset includes, at their latest version or pinned to one
    pub tools: Vec<ToolRef>,
}

/// Which toolset to edit and how to widen the groups of its tools
#[derive(Debug, Clone, Copy)]
pub struct ToolSetTarget {
    /// The toolset's ID
    pub id: Uuid,
    /// Whether to add the toolset's groups to tools that miss some of them
    pub extend_target_groups: bool,
}

/// The IDs the tools an edit adds were found to name
#[derive(Debug, Default)]
pub struct Resolved {
    /// The ID of each added tool or tool instance
    pub tools: BTreeMap<ToolRef, Uuid>,
}

/// The editing of toolsets by the shared edit flow
pub struct ToolSetEditable;

/// Get a toolset's metadata, or an empty toolset for another kind of entity
///
/// # Arguments
///
/// * `data` - The entity to read
fn meta_of(data: &Entity) -> ToolSetEntity {
    // only toolsets have toolset metadata
    match &data.metadata {
        EntityMetadata::ToolSet(meta) => meta.clone(),
        _ => ToolSetEntity::default(),
    }
}

/// Check a view for mistakes that need no server to find
///
/// # Arguments
///
/// * `view` - The edited view
fn check(view: &ToolSetView) -> Result<(), String> {
    // a toolset needs a name and a group
    if view.name.trim().is_empty() {
        return Err("name must not be blank".to_owned());
    }
    if view.groups.is_empty() {
        return Err("a toolset needs at least one group".to_owned());
    }
    if let Some(group) = duplicate(&view.groups) {
        return Err(format!("group {group} is listed twice"));
    }
    // the selector is a key and a value
    if let Some(selector) = &view.selector {
        selector.parse::<ToolSetSelector>()?;
    }
    // a tool is listed once, and a pinned version names a version
    if let Some(tool) = duplicate(&view.tools) {
        return Err(format!("tool {} is listed twice", tool.tool));
    }
    if view
        .tools
        .iter()
        .any(|tool| tool.version.as_deref().is_some_and(|v| v.trim().is_empty()))
    {
        return Err(
            "a tool version must not be blank; leave it out to follow the latest".to_owned(),
        );
    }
    Ok(())
}

/// Build the toolset a create command describes from its spec and flags
///
/// A new toolset starts from the defaults of a new toolset, takes what the spec sets, and then
/// what the flags set: a flag replaces the spec's value (groups included), and tools from flags
/// are added to the spec's tools.
///
/// # Arguments
///
/// * `spec` - The spec document, if one was given
/// * `cmd` - The create command
pub fn create_view(spec: Option<&Value>, cmd: &CreateToolSet) -> Result<ToolSetView, Error> {
    // start from the defaults of a new toolset and lay the spec over them
    let mut doc = json!({
        "name": "",
        "groups": [],
        "read_only": false,
        "auto_instance": true,
        "tools": [],
    });
    if let Some(spec) = spec {
        merge_patch(&mut doc, spec);
    }
    // lay the flags over that
    if let Some(name) = &cmd.name {
        doc["name"] = json!(name);
    }
    if !cmd.group.is_empty() {
        doc["groups"] = json!(cmd.group);
    }
    if let Some(description) = &cmd.description {
        doc["description"] = json!(description);
    }
    if let Some(selector) = &cmd.selector {
        doc["selector"] = json!(selector.to_string());
    }
    if cmd.read_only {
        doc["read_only"] = json!(true);
    }
    if cmd.no_auto_instance {
        doc["auto_instance"] = json!(false);
    }
    // read the result as a view, which refuses unknown keys
    let mut view: ToolSetView = serde_json::from_value(doc)
        .map_err(|err| Error::new(format!("Invalid toolset spec: {err}")))?;
    // tools named by flags follow the spec's tools
    view.tools.extend(cmd.tool.iter().map(|tool| ToolRef {
        tool: tool.clone(),
        version: None,
    }));
    check(&view).map_err(Error::new)?;
    Ok(view)
}

/// Build the request that creates a toolset
///
/// Settings that match a new toolset's defaults are left out, so the API's own defaults apply.
///
/// # Arguments
///
/// * `view` - The toolset to create
/// * `tools` - The IDs of the view's tools, in order
/// * `pins` - The IDs of tool instances to include pinned
/// * `extend_target_groups` - Whether to add the toolset's groups to tools that miss some of them
pub fn create_request(
    view: &ToolSetView,
    tools: &[Uuid],
    pins: &[Uuid],
    extend_target_groups: bool,
) -> Result<ToolSetEntityRequest, Error> {
    // include every tool at latest or pinned
    let mut request = ToolSetEntityRequest::default();
    for tool in tools.iter().chain(pins) {
        request = request.tool(*tool);
    }
    // set only what differs from a new toolset
    if let Some(selector) = &view.selector {
        request = request.selector(selector.parse::<ToolSetSelector>().map_err(Error::new)?);
    }
    if view.read_only {
        request = request.read_only(true);
    }
    if !view.auto_instance {
        request = request.auto_instance(false);
    }
    if extend_target_groups {
        request = request.extend_target_groups();
    }
    Ok(request)
}

/// Find the tools an edit adds
///
/// # Arguments
///
/// * `before` - The view of the toolset as it was read
/// * `after` - The edited view
#[must_use]
pub fn wanted(before: &ToolSetView, after: &ToolSetView) -> Vec<ToolRef> {
    // the tools the edited view lists that the toolset didn't include
    diff(&before.tools, &after.tools).0
}

/// Look up the tools an edit adds
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups the toolset will be in, which the tools are looked up in
/// * `wants` - The tools to look up
pub async fn resolve(
    thorium: &Thorium,
    groups: &[String],
    wants: &[ToolRef],
) -> Result<Resolved, Error> {
    // find each tool, at latest or pinned to a version
    let mut resolved = Resolved::default();
    for tool in wants {
        let id = tool.resolve(thorium, groups, "a tool").await?;
        resolved.tools.insert(tool.clone(), id);
    }
    Ok(resolved)
}

/// Turn an edited view into the update that applies it
///
/// Returns `None` when the view holds no change.
///
/// # Arguments
///
/// * `data` - The toolset as it was read, whose tools' IDs removals use
/// * `before` - The view of the toolset as it was read
/// * `after` - The edited view
/// * `resolved` - The IDs of the tools the edit adds
pub fn build_update(
    data: &Entity,
    before: &ToolSetView,
    after: &ToolSetView,
    resolved: &Resolved,
) -> Result<Option<EntityUpdate>, Error> {
    // the tools the toolset includes now, by the references the view names them with
    let existing: BTreeMap<ToolRef, Uuid> = meta_of(data)
        .tools
        .iter()
        .map(|tool| (ToolRef::from_entity(tool), tool.id))
        .collect();
    // diff the tools into the IDs to add and remove
    let (added, removed) = diff(&before.tools, &after.tools);
    let mut add_tools = Vec::new();
    for tool in &added {
        let id = resolved
            .tools
            .get(tool)
            .ok_or_else(|| Error::new(format!("The new tool {} was not looked up", tool.tool)))?;
        add_tools.push(*id);
    }
    let mut remove_tools = Vec::new();
    for tool in &removed {
        let id = existing.get(tool).ok_or_else(|| {
            Error::new(format!("The toolset has no tool {} to remove", tool.tool))
        })?;
        remove_tools.push(*id);
    }
    // a new selector replaces the old one, and none clears it
    let selector = match (&before.selector, &after.selector) {
        (old, Some(new)) if old.as_ref() != Some(new) => {
            Some(new.parse::<ToolSetSelector>().map_err(Error::new)?)
        }
        _ => None,
    };
    let clear_selector = (before.selector.is_some() && after.selector.is_none()).then_some(true);
    // the settings that changed
    let read_only = (before.read_only != after.read_only).then_some(after.read_only);
    let auto_instance =
        (before.auto_instance != after.auto_instance).then_some(after.auto_instance);
    // the toolset's own fields go in the entity update
    let mut update = EntityUpdate::default();
    if before.name != after.name {
        update.name = Some(after.name.clone());
    }
    update.add_groups = after
        .groups
        .iter()
        .filter(|group| !before.groups.contains(group))
        .cloned()
        .collect();
    update.remove_groups = before
        .groups
        .iter()
        .filter(|group| !after.groups.contains(group))
        .cloned()
        .collect();
    if before.description != after.description {
        match &after.description {
            Some(description) => update.description = Some(description.clone()),
            None => update.clear_description = true,
        }
    }
    // the settings, selector and tools go in the metadata update
    let metadata_changed = read_only.is_some()
        || auto_instance.is_some()
        || selector.is_some()
        || clear_selector.is_some()
        || !add_tools.is_empty()
        || !remove_tools.is_empty();
    if metadata_changed {
        update.metadata = Some(EntityMetadataUpdate::ToolSet {
            read_only,
            selector,
            clear_selector,
            auto_instance,
            add_tools,
            remove_tools,
            extend_target_groups: false,
        });
    }
    // nothing changed when no field was set
    let unchanged = update.name.is_none()
        && update.add_groups.is_empty()
        && update.remove_groups.is_empty()
        && update.description.is_none()
        && !update.clear_description
        && update.metadata.is_none();
    Ok((!unchanged).then_some(update))
}

/// Turn the typed flags of `toolsets update` into edits of the view
///
/// # Arguments
///
/// * `cmd` - The update command
/// * `view` - The view of the toolset as it was read, which group changes are relative to
#[must_use]
pub fn typed_ops(cmd: &UpdateToolSet, view: &ToolSetView) -> Vec<EditOp> {
    let mut ops = Vec::new();
    // the entity fields
    if let Some(name) = &cmd.rename {
        ops.push(set("name", json!(name)));
    }
    if let Some(description) = &cmd.description {
        ops.push(set("description", json!(description)));
    }
    if cmd.clear_description {
        ops.push(set("description", Value::Null));
    }
    // group changes start from the groups the toolset is in
    if !cmd.add_group.is_empty() || !cmd.remove_group.is_empty() {
        let mut groups = view.groups.clone();
        groups.retain(|group| !cmd.remove_group.contains(group));
        for group in &cmd.add_group {
            if !groups.contains(group) {
                groups.push(group.clone());
            }
        }
        ops.push(set("groups", json!(groups)));
    }
    // the automatic instancing switch
    if let Some(state) = cmd.auto_instance {
        ops.push(set("auto_instance", json!(state == Switch::On)));
    }
    ops
}

impl Editable for ToolSetEditable {
    /// Toolsets are edited through the Thorium client
    type Client = Thorium;
    /// The toolset's id and whether to widen added tools' groups
    type Target = ToolSetTarget;
    /// The toolset entity as Thorium returns it
    type Data = Entity;
    /// The toolset document users edit, naming its tools
    type View = ToolSetView;
    /// An entity update carrying the toolset changes
    type Update = EntityUpdate;
    /// Toolsets are called `Toolset` in messages
    const KIND: &'static str = "Toolset";
    /// The toolset document's keys, the read-only id first
    const FIELD_ORDER: &'static [&'static str] = &[
        "*id*",
        "name",
        "description",
        "groups",
        "read_only",
        "selector",
        "auto_instance",
        "tools",
    ];

    /// Read the toolset with its tools filled in
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `target` - The toolset's id
    async fn fetch(client: &Thorium, target: &ToolSetTarget) -> Result<Entity, Error> {
        client.entities.get(target.id).await
    }

    /// Build the view people edit from a toolset
    ///
    /// # Arguments
    ///
    /// * `data` - The toolset as fetched
    fn to_view(data: &Entity) -> ToolSetView {
        // name each tool, keeping the lists in a fixed order
        let meta = meta_of(data);
        let mut tools: Vec<ToolRef> = meta.tools.iter().map(ToolRef::from_entity).collect();
        tools.sort();
        let mut groups = data.groups.clone();
        groups.sort();
        ToolSetView {
            id: Some(data.id.to_string()),
            name: data.name.clone(),
            description: data.description.clone(),
            groups,
            read_only: meta.read_only,
            selector: meta.selector.as_ref().map(ToString::to_string),
            auto_instance: meta.auto_instance,
            tools,
        }
    }

    /// The toolset's ID
    ///
    /// # Arguments
    ///
    /// * `data` - The toolset as fetched
    fn id(data: &Entity) -> String {
        data.id.to_string()
    }

    /// The toolset's name
    ///
    /// # Arguments
    ///
    /// * `data` - The toolset as fetched
    fn name(data: &Entity) -> String {
        data.name.clone()
    }

    /// Check the edited toolset view's own rules before anything is sent
    ///
    /// # Arguments
    ///
    /// * `view` - The edited toolset view
    /// * `data` - The toolset as fetched, which its view's checks don't need
    fn validate(view: &ToolSetView, data: &Entity) -> Result<(), Error> {
        // the view's own checks need nothing from the toolset
        let _ = data;
        check(view).map_err(Error::new)
    }

    /// Resolve the tools the edit adds, in the toolset's new groups, and build the update
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `data` - The toolset as fetched
    /// * `view` - The edited toolset view
    async fn calculate_update(
        client: &Thorium,
        data: &Entity,
        view: ToolSetView,
    ) -> Result<Option<EntityUpdate>, Error> {
        // find the tools the edit adds, in the groups the toolset will be in
        let before = Self::to_view(data);
        let resolved = resolve(client, &view.groups, &wanted(&before, &view)).await?;
        build_update(data, &before, &view, &resolved)
    }

    /// Send the update, widening added tools' groups when the target asks to
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `target` - The toolset's id and whether to widen added tools' groups
    /// * `data` - The toolset as fetched
    /// * `update` - The toolset update to send
    async fn send_update(
        client: &Thorium,
        target: &ToolSetTarget,
        data: &Entity,
        update: &EntityUpdate,
    ) -> Result<(), Error> {
        // widen the groups of added tools only when asked
        let _ = data;
        let mut update = update.clone();
        if target.extend_target_groups
            && let Some(EntityMetadataUpdate::ToolSet {
                extend_target_groups,
                ..
            }) = &mut update.metadata
        {
            *extend_target_groups = true;
        }
        client.entities.update(target.id, update).await?;
        Ok(())
    }
}
