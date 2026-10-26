//! Export tools, toolsets, agents, workflows, and pipelines from Thorium as a toolbox
//!
//! The export reads the selected items (and every tool, toolset, and agent they refer to) from a
//! running Thorium, turns ids into names, and writes a toolbox directory: one directory per item
//! with a `manifest.toml` and the spec or config it points at, a `config.toml` of toolbox
//! settings, and the assembled v2 `toolbox.json` that holds everything inline. Container images
//! stay in their registry, and model endpoints, profiles, and credentials are never exported.

use futures::stream::{self, StreamExt, TryStreamExt};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use thorium::models::EntityKinds;
use thorium::{CtlConf, Error, Thorium};

use super::v2::load::ConvertReport;
use super::v2::plan_types::{ItemKind, ItemRef};
use super::v2::types::{Counts, FORMAT_VERSION, ToolboxManifestV2};
use super::v2::validate::{Severity, validate_manifest};
use crate::args::Args;
use crate::args::toolbox::{ExportToolbox, ResourceSpec};
use crate::handlers::container;
use crate::handlers::imports::editor::{resolve_editor, review_config_in_editor};
use crate::handlers::progress::{Bar, BarKind};
use crate::utils::output::{self, OutputFormat, View};

mod files;
pub(super) mod live;
pub(super) mod snapshot;
#[cfg(test)]
pub(in crate::handlers::toolbox) mod tests;

use files::OutFile;
use live::VersionFilter;
use snapshot::{BuildOptions, Snapshot, build_manifest};

/// The toolbox name used when none is given for a new toolbox
const DEFAULT_NAME: &str = "My Toolbox";

/// What the export does when an item already exists in the output with other contents
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnConflict {
    /// Replace the item
    Overwrite,
    /// Leave the item as it is and warn
    Skip,
    /// Stop before anything is written
    Fail,
}

/// What the export does with one item
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// The item is new to the output and is written
    Create,
    /// The item differed and is replaced
    Replace,
    /// The item is already in the output as exported; only missing files are written
    Unchanged,
    /// The item differs and is left as it is
    Skipped,
}

/// The items of each kind an export names
#[derive(Debug, Default)]
struct Selection {
    /// Export everything of the default kinds from this group
    whole_group: Option<String>,
    /// Also export toolsets, agents, and workflows of the whole group
    all_kinds: bool,
    /// The tools to export
    tools: Vec<ResourceSpec>,
    /// The pipelines to export
    pipelines: Vec<ResourceSpec>,
    /// The toolsets to export
    toolsets: Vec<ResourceSpec>,
    /// The agents to export
    agents: Vec<ResourceSpec>,
    /// The workflows to export
    workflows: Vec<ResourceSpec>,
}

/// Parse a list of `group/name[=dir]` references
///
/// # Arguments
///
/// * `raw` - The references as given on the command line
/// * `default_group` - The group for references that name none
fn parse_specs(raw: &[String], default_group: Option<&str>) -> Result<Vec<ResourceSpec>, Error> {
    // parse each reference, turning the parser's message into an error
    raw.iter()
        .map(|spec| ResourceSpec::parse(spec, default_group).map_err(Error::new))
        .collect()
}

impl Selection {
    /// Read the selection from the command line
    ///
    /// # Arguments
    ///
    /// * `cmd` - The export command
    fn from_cmd(cmd: &ExportToolbox) -> Result<Self, Error> {
        let group = cmd.group.as_deref();
        let mut selection = Selection {
            whole_group: None,
            all_kinds: cmd.all_kinds,
            tools: parse_specs(&cmd.tools, group)?,
            pipelines: parse_specs(&cmd.pipelines, group)?,
            toolsets: parse_specs(&cmd.toolsets, group)?,
            agents: parse_specs(&cmd.agents, group)?,
            workflows: parse_specs(&cmd.workflows, group)?,
        };
        // with nothing named, a group selects everything of the default kinds
        if selection.is_empty() {
            selection.whole_group.clone_from(&cmd.group);
        }
        Ok(selection)
    }

    /// Whether no item is named
    fn is_empty(&self) -> bool {
        // every list must be empty
        self.tools.is_empty()
            && self.pipelines.is_empty()
            && self.toolsets.is_empty()
            && self.agents.is_empty()
            && self.workflows.is_empty()
    }

    /// Get the specs a kind selects by name
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of item
    fn specs(&self, kind: ItemKind) -> &[ResourceSpec] {
        // each kind has its own list
        match kind {
            ItemKind::Tool => &self.tools,
            ItemKind::Pipeline => &self.pipelines,
            ItemKind::Toolset => &self.toolsets,
            ItemKind::Agent => &self.agents,
            ItemKind::Workflow => &self.workflows,
        }
    }

    /// Whether a kind of item is exported
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of item
    fn wants(&self, kind: ItemKind) -> bool {
        // a whole group exports tools and pipelines, and the rest with `--all-kinds`
        if self.whole_group.is_some() {
            return matches!(kind, ItemKind::Tool | ItemKind::Pipeline) || self.all_kinds;
        }
        !self.specs(kind).is_empty()
    }
}

/// Get where an item's files are placed when its reference named a directory
///
/// # Arguments
///
/// * `selection` - The items the export names
fn dir_overrides(selection: &Selection) -> Result<HashMap<ItemRef, String>, Error> {
    let mut overrides = HashMap::new();
    for kind in [
        ItemKind::Tool,
        ItemKind::Pipeline,
        ItemKind::Toolset,
        ItemKind::Agent,
        ItemKind::Workflow,
    ] {
        for spec in selection.specs(kind) {
            let Some(dest) = &spec.dest else {
                continue;
            };
            // a placement must stay inside the toolbox
            let path = Path::new(dest);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
            {
                return Err(Error::new(format!(
                    "'{dest}' is not a directory inside the toolbox (use a relative path with no \
                     '..')"
                )));
            }
            overrides.insert(
                ItemRef::new(kind, &spec.name),
                dest.trim_end_matches('/').to_owned(),
            );
        }
    }
    Ok(overrides)
}

/// Get the entity kind a manifest item kind is stored as
///
/// # Arguments
///
/// * `kind` - The kind of item
fn entity_kind(kind: ItemKind) -> EntityKinds {
    // each item kind is one entity kind
    match kind {
        ItemKind::Tool => EntityKinds::Tool,
        ItemKind::Toolset => EntityKinds::ToolSet,
        ItemKind::Agent => EntityKinds::Agent,
        ItemKind::Workflow => EntityKinds::WorkflowCatalog,
        ItemKind::Pipeline => EntityKinds::Other,
    }
}

/// Find the entities a selection names for one kind
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `selection` - The items the export names
/// * `kind` - The kind of entity to find
async fn select_entities(
    thorium: &Thorium,
    selection: &Selection,
    kind: ItemKind,
) -> Result<Vec<thorium::models::Entity>, Error> {
    // the groups to list are the whole group, or the groups of the named items
    let specs = selection.specs(kind);
    let groups: Vec<String> = match &selection.whole_group {
        Some(group) => vec![group.clone()],
        None => specs
            .iter()
            .map(|spec| spec.group.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let listed = live::list_entities(thorium, &groups, entity_kind(kind)).await?;
    // a whole group takes everything listed
    if selection.whole_group.is_some() {
        return Ok(listed);
    }
    // named items must each be found
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for spec in specs {
        match listed
            .iter()
            .find(|entity| entity.name == spec.name && entity.groups.contains(&spec.group))
        {
            Some(entity) => found.push(entity.clone()),
            None => missing.push(format!("{} {}/{}", kind, spec.group, spec.name)),
        }
    }
    if !missing.is_empty() {
        return Err(Error::new(format!("Not found: {}", missing.join(", "))));
    }
    Ok(found)
}

/// Read the toolbox settings the export writes `config.toml` from
#[derive(Debug)]
struct Settings {
    /// The whole `config.toml`
    table: toml::Table,
    /// Whether `config.toml` is written (a new toolbox, or `--overwrite-config`)
    write: bool,
}

impl Settings {
    /// Get the toolbox name
    fn name(&self) -> String {
        // the name is the table's name, or the default
        self.table
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or(DEFAULT_NAME)
            .to_owned()
    }

    /// Get a string from the settings
    ///
    /// # Arguments
    ///
    /// * `key` - The key to read
    fn text(&self, key: &str) -> Option<String> {
        // read a string value
        self.table
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    }

    /// Get the registry the toolbox declares
    fn registry(&self) -> Option<String> {
        // the registry is optional
        self.text("registry")
            .filter(|registry| !registry.is_empty())
    }
}

/// Where the toolbox is written
///
/// # Arguments
///
/// * `cmd` - The export command
fn resolve_output(cmd: &ExportToolbox) -> PathBuf {
    // an explicit output wins, then the directory of the config, then ./toolbox
    if let Some(output) = &cmd.output {
        return output.clone();
    }
    match &cmd.config {
        Some(config) => config
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        None => PathBuf::from("toolbox"),
    }
}

/// Read a `config.toml`
///
/// # Arguments
///
/// * `path` - The file to read
fn read_config(path: &Path) -> Result<toml::Table, Error> {
    // read and parse the file
    let text = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))?;
    text.parse::<toml::Table>()
        .map_err(|err| Error::new(format!("Failed to parse {}: {err}", path.display())))
}

/// Work out the toolbox settings from the output's config, `--config`, and the flags
///
/// The settings come from a kept `<output>/config.toml` first, then `--config`, then
/// `--name`/`--registry`.
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `output` - The toolbox root
/// * `warnings` - Where to add what is worth telling the user
fn resolve_settings(
    cmd: &ExportToolbox,
    output: &Path,
    warnings: &mut Vec<String>,
) -> Result<Settings, Error> {
    let existing = output.join("config.toml");
    let kept = existing.is_file() && !cmd.overwrite_config;
    let mut table = if kept {
        // a kept config wins over every other source
        if cmd
            .config
            .as_ref()
            .is_some_and(|config| config != &existing)
            || cmd.name.is_some()
            || cmd.registry.is_some()
        {
            warnings.push(format!(
                "{} is kept, so --config, --name, and --registry are ignored (use \
                 --overwrite-config to replace it)",
                existing.display()
            ));
        }
        read_config(&existing)?
    } else if let Some(config) = &cmd.config {
        // seed the settings from another toolbox's config
        if config.is_file() {
            read_config(config)?
        } else {
            warnings.push(format!(
                "{} does not exist; creating a new toolbox with default settings",
                config.display()
            ));
            toml::Table::new()
        }
    } else {
        toml::Table::new()
    };
    // explicit name and registry apply to a config that is written
    if !kept {
        if let Some(name) = &cmd.name {
            table.insert("name".to_owned(), toml::Value::String(name.clone()));
        }
        if let Some(registry) = &cmd.registry {
            table.insert("registry".to_owned(), toml::Value::String(registry.clone()));
        }
        table
            .entry("name".to_owned())
            .or_insert_with(|| toml::Value::String(DEFAULT_NAME.to_owned()));
        table.insert(
            "format_version".to_owned(),
            toml::Value::Integer(i64::from(FORMAT_VERSION)),
        );
        // bundling always follows --with-images
        table.remove("bundled_images");
        if cmd.with_images {
            table.insert("bundled_images".to_owned(), toml::Value::Boolean(true));
        }
    }
    Ok(Settings {
        table,
        write: !kept,
    })
}

/// Read the toolbox already in the output directory, if there is one
///
/// # Arguments
///
/// * `output` - The toolbox root
fn read_existing(output: &Path) -> Result<Option<ToolboxManifestV2>, Error> {
    let path = output.join("toolbox.json");
    if !path.is_file() {
        return Ok(None);
    }
    // read the document, refusing a legacy toolbox since the export only writes the v2 format
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))?;
    let document: Value = serde_json::from_str(&text)
        .map_err(|err| Error::new(format!("Failed to parse {}: {err}", path.display())))?;
    let legacy = document.get("images").is_some()
        || document
            .get("pipelines")
            .and_then(Value::as_object)
            .is_some_and(|pipelines| {
                pipelines
                    .values()
                    .any(|pipeline| pipeline.get("versions").is_none())
            });
    if legacy {
        return Err(Error::new(format!(
            "{} is a legacy toolbox, and an export writes the v2 format only; convert it with \
             `thorctl toolbox convert` or export to another directory with --output",
            path.display()
        )));
    }
    serde_json::from_value(document)
        .map(Some)
        .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))
}

/// The items of a manifest, in the order they are written
///
/// # Arguments
///
/// * `manifest` - The manifest to list the items of
pub(in crate::handlers::toolbox) fn items_of(manifest: &ToolboxManifestV2) -> Vec<ItemRef> {
    // tools first, then everything that refers to them
    let mut items = Vec::new();
    items.extend(
        manifest
            .tools
            .keys()
            .map(|n| ItemRef::new(ItemKind::Tool, n)),
    );
    items.extend(
        manifest
            .toolsets
            .keys()
            .map(|n| ItemRef::new(ItemKind::Toolset, n)),
    );
    items.extend(
        manifest
            .agents
            .keys()
            .map(|n| ItemRef::new(ItemKind::Agent, n)),
    );
    items.extend(
        manifest
            .workflows
            .keys()
            .map(|n| ItemRef::new(ItemKind::Workflow, n)),
    );
    items.extend(
        manifest
            .pipelines
            .keys()
            .map(|n| ItemRef::new(ItemKind::Pipeline, n)),
    );
    items
}

/// Get an item of a manifest as JSON
///
/// # Arguments
///
/// * `manifest` - The manifest the item is in
/// * `item` - The item
fn item_value(manifest: &ToolboxManifestV2, item: &ItemRef) -> Option<Value> {
    // serialize the item's own entry
    let name = &item.name;
    match item.kind {
        ItemKind::Tool => manifest.tools.get(name).map(serde_json::to_value),
        ItemKind::Toolset => manifest.toolsets.get(name).map(serde_json::to_value),
        ItemKind::Agent => manifest.agents.get(name).map(serde_json::to_value),
        ItemKind::Workflow => manifest.workflows.get(name).map(serde_json::to_value),
        ItemKind::Pipeline => manifest.pipelines.get(name).map(serde_json::to_value),
    }
    .and_then(Result::ok)
}

/// Copy an item of one manifest into another
///
/// # Arguments
///
/// * `from` - The manifest to copy from
/// * `into` - The manifest to copy into
/// * `item` - The item
fn copy_item(from: &ToolboxManifestV2, into: &mut ToolboxManifestV2, item: &ItemRef) {
    // copy the item's entry by name
    let name = &item.name;
    match item.kind {
        ItemKind::Tool => {
            if let Some(found) = from.tools.get(name) {
                into.tools.insert(name.clone(), found.clone());
            }
        }
        ItemKind::Toolset => {
            if let Some(found) = from.toolsets.get(name) {
                into.toolsets.insert(name.clone(), found.clone());
            }
        }
        ItemKind::Agent => {
            if let Some(found) = from.agents.get(name) {
                into.agents.insert(name.clone(), found.clone());
            }
        }
        ItemKind::Workflow => {
            if let Some(found) = from.workflows.get(name) {
                into.workflows.insert(name.clone(), found.clone());
            }
        }
        ItemKind::Pipeline => {
            if let Some(found) = from.pipelines.get(name) {
                into.pipelines.insert(name.clone(), found.clone());
            }
        }
    }
}

/// Decide what to do with every exported item given what the output already holds
///
/// The items to keep are copied into `merged`, which starts as the existing toolbox.
///
/// # Arguments
///
/// * `new` - The manifest just built from Thorium
/// * `merged` - The existing toolbox, which receives the new and replaced items
/// * `on_conflict` - What to do with a differing item
fn reconcile(
    new: &ToolboxManifestV2,
    merged: &mut ToolboxManifestV2,
    on_conflict: OnConflict,
) -> Result<Vec<(ItemRef, Action)>, Error> {
    let mut actions = Vec::new();
    let mut conflicts = Vec::new();
    for item in items_of(new) {
        let action = match item_value(merged, &item) {
            None => Action::Create,
            Some(existing) if Some(&existing) == item_value(new, &item).as_ref() => {
                Action::Unchanged
            }
            Some(_) => match on_conflict {
                OnConflict::Overwrite => Action::Replace,
                OnConflict::Skip => Action::Skipped,
                OnConflict::Fail => {
                    conflicts.push(format!("{} {}", item.kind, item.name));
                    Action::Skipped
                }
            },
        };
        if matches!(action, Action::Create | Action::Replace) {
            copy_item(new, merged, &item);
        }
        actions.push((item, action));
    }
    // a conflict without a policy stops the export before anything is written
    if !conflicts.is_empty() {
        return Err(Error::new(format!(
            "These items already exist in the output and differ: {}. Rerun with --overwrite to \
             replace them or --skip-conflicts to leave them",
            conflicts.join(", ")
        )));
    }
    Ok(actions)
}

/// Add the network policies and groups of a new manifest to the merged one
///
/// # Arguments
///
/// * `new` - The manifest just built from Thorium
/// * `merged` - The manifest being written
fn merge_shared(new: &ToolboxManifestV2, merged: &mut ToolboxManifestV2) {
    // policies are kept by name, with the existing definition winning
    for policy in &new.network_policies {
        if !merged
            .network_policies
            .iter()
            .any(|p| p.name == policy.name)
        {
            merged.network_policies.push(policy.clone());
        }
    }
    merged.network_policies.sort_by(|a, b| a.name.cmp(&b.name));
    // groups are the union, keeping an existing description
    for (group, manifest) in &new.groups {
        merged
            .groups
            .entry(group.clone())
            .or_insert_with(|| manifest.clone());
    }
}

/// Open each new or replaced item in an editor before it is written
///
/// # Arguments
///
/// * `manifest` - The manifest holding the items
/// * `actions` - What is done with each item
/// * `editor` - The editor command
/// * `bar` - The progress bar to suspend while the editor is open
async fn review_items(
    manifest: &mut ToolboxManifestV2,
    actions: &[(ItemRef, Action)],
    editor: &str,
    bar: &Bar,
) -> Result<(), Error> {
    for (item, action) in actions {
        if !matches!(action, Action::Create | Action::Replace) {
            continue;
        }
        let Some(json) = item_value(manifest, item) else {
            continue;
        };
        let label = format!("{}-{}", item.kind, item.name);
        // the editor shows the item as YAML and the result must still parse as one
        let text = serde_json::to_string(&json)
            .map_err(|err| Error::new(format!("Failed to review {label}: {err}")))?;
        let reviewed = bar
            .suspend_async(review_config_in_editor::<Value>(&text, &label, editor, &[]))
            .await?;
        // put the reviewed item back into the manifest
        let name = item.name.clone();
        let parse = |err: serde_json::Error| {
            Error::new(format!("The reviewed {label} is not valid: {err}"))
        };
        match item.kind {
            ItemKind::Tool => {
                manifest
                    .tools
                    .insert(name, serde_json::from_str(&reviewed).map_err(parse)?);
            }
            ItemKind::Toolset => {
                manifest
                    .toolsets
                    .insert(name, serde_json::from_str(&reviewed).map_err(parse)?);
            }
            ItemKind::Agent => {
                manifest
                    .agents
                    .insert(name, serde_json::from_str(&reviewed).map_err(parse)?);
            }
            ItemKind::Workflow => {
                manifest
                    .workflows
                    .insert(name, serde_json::from_str(&reviewed).map_err(parse)?);
            }
            ItemKind::Pipeline => {
                manifest
                    .pipelines
                    .insert(name, serde_json::from_str(&reviewed).map_err(parse)?);
            }
        }
    }
    Ok(())
}

/// Write a file, creating its directories
///
/// # Arguments
///
/// * `root` - The toolbox root
/// * `file` - The file to write
/// * `only_if_missing` - Leave the file alone when it already exists
fn write_file(root: &Path, file: &OutFile, only_if_missing: bool) -> Result<(), Error> {
    let path = root.join(&file.path);
    // an unchanged item only gets the files it lacks
    if only_if_missing && path.exists() {
        return Ok(());
    }
    // create the directory and write the file
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| Error::new(format!("Failed to create {}: {err}", parent.display())))?;
    }
    std::fs::write(&path, &file.content)
        .map_err(|err| Error::new(format!("Failed to write {}: {err}", path.display())))
}

/// The container image url of an image tool version that runs from a container
///
/// # Arguments
///
/// * `spec` - The version's spec as JSON
fn container_url(spec: &Value) -> Option<String> {
    // only K8s image tools run from a container image
    spec.pointer("/Image/environment/K8s/image")
        .or_else(|| spec.pointer("/Harness/workload/environment/K8s/image"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Pull and save the container image of a tool version into its directory
///
/// # Arguments
///
/// * `root` - The toolbox root
/// * `dir` - The version's directory
/// * `name` - The tool's name
/// * `url` - The container image url
/// * `quiet` - Whether `--quiet` is set, which hides the bundling bar
async fn bundle_image(
    root: &Path,
    dir: &str,
    name: &str,
    url: &str,
    quiet: bool,
) -> Result<(), Error> {
    // a url is needed to pull the image
    if url.is_empty() {
        return Err(Error::new(format!(
            "tool {name} has no container image url to bundle"
        )));
    }
    // pull the image, then save it as a tarball beside the version's manifest
    let bar = Bar::new_or_quiet(name, "Bundling image", BarKind::Timer, quiet);
    let tar = root.join(dir).join(format!("{name}.tar.gz"));
    let result = match container::pull(url, &bar).await {
        Ok(()) => container::save(url, &tar, &bar).await,
        Err(err) => Err(err),
    };
    bar.finish_and_clear();
    result
}

/// What an export wrote, as printed
#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    /// The toolbox directory
    pub output: String,
    /// The assembled manifest
    pub toolbox: String,
    /// How many of each kind the manifest now holds
    pub counts: Counts,
    /// The items written for the first time
    pub created: Vec<String>,
    /// The items replaced
    pub replaced: Vec<String>,
    /// The items already in the output as exported
    pub unchanged: Vec<String>,
    /// The items left as they were because they differ
    pub skipped: Vec<String>,
    /// Things worth a look
    pub warnings: Vec<String>,
    /// The number of errors `toolbox validate` finds in the manifest
    pub validation_errors: usize,
}

impl View for ExportReport {
    /// Render the report as a table of items
    fn table(&self) -> String {
        // one row per item with what was done
        let mut rows = Vec::new();
        for (action, items) in [
            ("created", &self.created),
            ("replaced", &self.replaced),
            ("unchanged", &self.unchanged),
            ("skipped", &self.skipped),
        ] {
            rows.extend(
                items
                    .iter()
                    .map(|item| vec![action.to_owned(), item.clone()]),
            );
        }
        output::table_from(&["ACTION", "ITEM"], rows)
    }

    /// Render the report as text for people
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text
    fn human(&self, _ansi: bool) -> String {
        // the summary line, then the items that were not simply created
        let mut lines = vec![format!(
            "Exported {} tool(s) ({} version(s)), {} toolset(s), {} agent(s), {} workflow(s), \
             and {} pipeline(s) to {}",
            self.counts.tools,
            self.counts.tool_versions,
            self.counts.toolsets,
            self.counts.agents,
            self.counts.workflows,
            self.counts.pipelines,
            self.toolbox
        )];
        lines.push(format!(
            "{} created, {} replaced, {} unchanged, {} skipped",
            self.created.len(),
            self.replaced.len(),
            self.unchanged.len(),
            self.skipped.len()
        ));
        for item in &self.skipped {
            lines.push(format!("skipped (differs from the output): {item}"));
        }
        for warning in &self.warnings {
            lines.push(format!("warning: {warning}"));
        }
        if self.validation_errors > 0 {
            lines.push(format!(
                "{} validation error(s); run `thorctl toolbox validate {}` for details",
                self.validation_errors, self.toolbox
            ));
        }
        lines.join("\n")
    }
}

/// Export tools, pipelines, toolsets, agents, and workflows from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The export command
/// * `args` - The top-level thorctl args
/// * `conf` - The thorctl config
pub async fn export(
    thorium: Thorium,
    cmd: &ExportToolbox,
    args: &Args,
    conf: &CtlConf,
) -> Result<(), Error> {
    // --review opens an editor per item, which needs a real terminal
    crate::handlers::exports::require_review_terminal(cmd.review)?;
    let bar = Bar::new_or_quiet("toolbox export", "Exporting", BarKind::Timer, args.quiet);
    let mut warnings = Vec::new();
    // work out where the toolbox is and what is already there
    let output = resolve_output(cmd);
    let settings = resolve_settings(cmd, &output, &mut warnings)?;
    if cmd.strip_registry && settings.registry().is_none() {
        return Err(Error::new(
            "--strip-registry clears each image's url so a build derives it from config.toml, but \
             the toolbox config has no registry; pass --registry <REGISTRY> (with \
             --overwrite-config when the output already has a config.toml)",
        ));
    }
    let existing = read_existing(&output)?;
    // decide what to export: the named items, a whole group, or a refresh of the output's items
    let mut selection = Selection::from_cmd(cmd)?;
    if selection.is_empty() && selection.whole_group.is_none() {
        selection = refresh_selection(cmd, existing.as_ref())?;
    }
    let overrides = dir_overrides(&selection)?;
    // read the selected items and everything they refer to
    bar.set_message("Reading from Thorium");
    let filter = VersionFilter {
        include_deprecated: cmd.include_deprecated,
        latest_only: cmd.latest_only,
    };
    let snapshot = read_snapshot(&thorium, &selection, filter, args.workers).await?;
    // build the manifest and fill in where each item is written
    let options = BuildOptions {
        group_override: cmd.group_override.clone(),
        strip_registry: cmd.strip_registry,
    };
    let built = build_manifest(&settings.name(), &snapshot, &options)?;
    let mut manifest = built.manifest;
    warnings.extend(built.warnings);
    settings_into_manifest(&settings, &mut manifest, cmd.with_images);
    files::assign_dirs(&mut manifest, &overrides)?;
    // reconcile with what the output already holds
    let on_conflict = if cmd.overwrite {
        OnConflict::Overwrite
    } else if cmd.skip_conflicts {
        OnConflict::Skip
    } else {
        OnConflict::Fail
    };
    let mut merged = existing.unwrap_or_else(|| ToolboxManifestV2::new(settings.name()));
    let actions = reconcile(&manifest, &mut merged, on_conflict)?;
    // let a person tweak each item before it is written
    if cmd.review {
        let editor = resolve_editor(cmd.editor.as_deref(), conf).to_owned();
        review_items(&mut manifest, &actions, &editor, &bar).await?;
        merged = merge_reviewed(&manifest, merged, &actions);
    }
    merge_shared(&manifest, &mut merged);
    settings_into_manifest(&settings, &mut merged, cmd.with_images);
    // write everything the export produced
    bar.set_message("Writing the toolbox");
    let layout = Layout {
        output: &output,
        settings: &settings,
        overrides: &overrides,
        with_images: cmd.with_images,
    };
    write_toolbox(&layout, &merged, &actions, args, &bar).await?;
    bar.finish_and_clear();
    // check what was written and report
    let issues = validate_manifest(&merged, &ConvertReport::default(), &[]);
    let report = ExportReport {
        output: output.display().to_string(),
        toolbox: output.join("toolbox.json").display().to_string(),
        counts: merged.counts(),
        created: names_with(&actions, Action::Create),
        replaced: names_with(&actions, Action::Replace),
        unchanged: names_with(&actions, Action::Unchanged),
        skipped: names_with(&actions, Action::Skipped),
        warnings,
        validation_errors: issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .count(),
    };
    output::print(&report, cmd.format.format.unwrap_or(OutputFormat::Human))
}

/// Where and how a toolbox is written
struct Layout<'a> {
    /// The toolbox root
    output: &'a Path,
    /// The toolbox settings
    settings: &'a Settings,
    /// The directories items were asked to be placed in
    overrides: &'a HashMap<ItemRef, String>,
    /// Whether container images are bundled
    with_images: bool,
}

/// Write the files of every item, `config.toml`, and the assembled `toolbox.json`
///
/// # Arguments
///
/// * `layout` - Where and how the toolbox is written
/// * `merged` - The manifest being written
/// * `actions` - What is done with each item
/// * `args` - The top-level thorctl args
/// * `bar` - The export's progress bar
async fn write_toolbox(
    layout: &Layout<'_>,
    merged: &ToolboxManifestV2,
    actions: &[(ItemRef, Action)],
    args: &Args,
    bar: &Bar,
) -> Result<(), Error> {
    let output = layout.output;
    // write the files of every item that is new, replaced, or missing files
    std::fs::create_dir_all(output)
        .map_err(|err| Error::new(format!("Failed to create {}: {err}", output.display())))?;
    for (item, action) in actions {
        if *action == Action::Skipped {
            continue;
        }
        for file in files::item_files(merged, item, layout.overrides)? {
            write_file(output, &file, *action == Action::Unchanged)?;
        }
    }
    // bundle the container images of the tools that were written
    if layout.with_images {
        bundle_images(output, merged, actions, args, bar).await?;
    }
    // write config.toml when this export owns it
    if layout.settings.write {
        let text = toml::to_string_pretty(&layout.settings.table)
            .map_err(|err| Error::new(format!("Failed to write config.toml: {err}")))?;
        let file = OutFile {
            path: PathBuf::from("config.toml"),
            content: text,
        };
        write_file(output, &file, false)?;
    }
    // write the assembled manifest last, so a failed export leaves the old one in place
    let mut json = serde_json::to_string_pretty(merged)
        .map_err(|err| Error::new(format!("Failed to write toolbox.json: {err}")))?;
    json.push('\n');
    let file = OutFile {
        path: PathBuf::from("toolbox.json"),
        content: json,
    };
    write_file(output, &file, false)
}

/// List the items that got an action
///
/// # Arguments
///
/// * `actions` - What is done with each item
/// * `wanted` - The action to list
fn names_with(actions: &[(ItemRef, Action)], wanted: Action) -> Vec<String> {
    // name each matching item as `kind name`
    actions
        .iter()
        .filter(|(_, action)| *action == wanted)
        .map(|(item, _)| item.to_string())
        .collect()
}

/// Copy the toolbox-wide settings from `config.toml` into a manifest
///
/// # Arguments
///
/// * `settings` - The toolbox settings
/// * `manifest` - The manifest to fill in
/// * `with_images` - Whether container images are bundled
fn settings_into_manifest(
    settings: &Settings,
    manifest: &mut ToolboxManifestV2,
    with_images: bool,
) {
    // the name, registries, and image path come from the config
    manifest.name = settings.name();
    manifest.registry = settings.registry();
    manifest.registries = settings
        .table
        .get("registries")
        .and_then(toml::Value::as_array)
        .map_or_else(Vec::new, |list| {
            list.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        });
    manifest.image_path_prefix = settings.text("image_path_prefix");
    manifest.bundled_images |= with_images;
}

/// Put the reviewed items back into the merged manifest
///
/// # Arguments
///
/// * `reviewed` - The manifest whose items were reviewed
/// * `merged` - The manifest being written
/// * `actions` - What is done with each item
fn merge_reviewed(
    reviewed: &ToolboxManifestV2,
    mut merged: ToolboxManifestV2,
    actions: &[(ItemRef, Action)],
) -> ToolboxManifestV2 {
    // copy back every item that is written
    for (item, action) in actions {
        if matches!(action, Action::Create | Action::Replace) {
            copy_item(reviewed, &mut merged, item);
        }
    }
    merged
}

/// Bundle the container images of the tool versions that were written
///
/// # Arguments
///
/// * `output` - The toolbox root
/// * `merged` - The manifest that was written
/// * `actions` - What was done with each item
/// * `args` - The top-level thorctl args
/// * `bar` - The export's progress bar
async fn bundle_images(
    output: &Path,
    merged: &ToolboxManifestV2,
    actions: &[(ItemRef, Action)],
    args: &Args,
    bar: &Bar,
) -> Result<(), Error> {
    // every version of a written tool that runs from a container
    let mut jobs = Vec::new();
    for (item, action) in actions {
        if item.kind != ItemKind::Tool || !matches!(action, Action::Create | Action::Replace) {
            continue;
        }
        for version in merged.tools[&item.name].versions.values() {
            if let Some(url) = version.spec.as_ref().and_then(container_url) {
                jobs.push((item.name.clone(), version.dir.clone(), url));
            }
        }
    }
    bar.set_message("Bundling images");
    // bundle a few at a time
    stream::iter(jobs)
        .map(|(name, dir, url)| async move {
            bundle_image(output, &dir, &name, &url, args.quiet)
                .await
                .map_err(|err| Error::new(format!("Failed to bundle tool {name}: {err}")))
        })
        .buffer_unordered(args.workers.max(1))
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

/// Select everything already in the output, to refresh it
///
/// # Arguments
///
/// * `cmd` - The export command
/// * `existing` - The toolbox in the output directory, if there is one
fn refresh_selection(
    cmd: &ExportToolbox,
    existing: Option<&ToolboxManifestV2>,
) -> Result<Selection, Error> {
    // with nothing to refresh there is nothing to say what to export
    let Some(existing) = existing else {
        return Err(Error::new(
            "Nothing to export: name items with --tools, --pipelines, --toolsets, --agents, or \
             --workflows, or select a whole group with --group",
        ));
    };
    // a refresh rewrites items, so it is only done when asked for
    if !cmd.overwrite {
        return Err(Error::new(
            "Nothing to export: name items, select a group with --group, or pass --overwrite to \
             refresh the items already in the output",
        ));
    }
    // every item names its first group and its own name
    let spec = |group: Option<&String>, name: &String| -> Result<ResourceSpec, Error> {
        let group = group.ok_or_else(|| Error::new(format!("{name} has no group")))?;
        Ok(ResourceSpec {
            group: group.clone(),
            name: name.clone(),
            dest: None,
        })
    };
    let mut selection = Selection::default();
    for (name, tool) in &existing.tools {
        selection.tools.push(spec(tool.groups.first(), name)?);
    }
    for (name, toolset) in &existing.toolsets {
        selection.toolsets.push(spec(toolset.groups.first(), name)?);
    }
    for (name, agent) in &existing.agents {
        selection.agents.push(spec(agent.groups.first(), name)?);
    }
    for (name, workflow) in &existing.workflows {
        selection
            .workflows
            .push(spec(workflow.groups.first(), name)?);
    }
    for (name, pipeline) in &existing.pipelines {
        let group = pipeline
            .versions
            .values()
            .find_map(|version| version.config.as_ref())
            .map(|config| &config.group);
        selection.pipelines.push(spec(group, name)?);
    }
    Ok(selection)
}

/// Read the selected items from Thorium into a snapshot, with everything they refer to
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `selection` - The items the export names
/// * `filter` - Which tool versions to read
/// * `workers` - The most reads in flight at once
async fn read_snapshot(
    thorium: &Thorium,
    selection: &Selection,
    filter: VersionFilter,
    workers: usize,
) -> Result<Snapshot, Error> {
    let mut snapshot = Snapshot::default();
    // the selected tools and their versions
    if selection.wants(ItemKind::Tool) {
        let entities = select_entities(thorium, selection, ItemKind::Tool).await?;
        snapshot.tools = live::read_tools(thorium, &entities, workers).await?;
        let wanted: BTreeSet<_> = snapshot
            .tools
            .iter()
            .flat_map(|tool| live::wanted_instances(tool, filter))
            .collect();
        live::load_specs(thorium, &mut snapshot.tools, &wanted, workers).await?;
    }
    // the selected toolsets, agents, and workflows are read by id so their edges are filled in
    for (kind, target) in [
        (ItemKind::Toolset, 0),
        (ItemKind::Agent, 1),
        (ItemKind::Workflow, 2),
    ] {
        if !selection.wants(kind) {
            continue;
        }
        let entities = select_entities(thorium, selection, kind).await?;
        let ids: Vec<_> = entities.iter().map(|entity| entity.id).collect();
        let read = live::read_entities(thorium, &ids, workers).await?;
        match target {
            0 => snapshot.toolsets = read,
            1 => snapshot.agents = read,
            _ => snapshot.workflows = read,
        }
    }
    // the selected pipelines
    if selection.wants(ItemKind::Pipeline) {
        let groups: Vec<String> = match &selection.whole_group {
            Some(group) => vec![group.clone()],
            None => selection
                .pipelines
                .iter()
                .map(|spec| spec.group.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
        let listed = live::list_pipelines(thorium, &groups).await?;
        snapshot.pipelines = if selection.whole_group.is_some() {
            listed
        } else {
            pick_pipelines(&listed, &selection.pipelines)?
        };
    }
    // follow every reference, then read the policies and groups the items use
    live::complete(thorium, &mut snapshot, filter, workers).await?;
    live::read_policies(thorium, &mut snapshot).await?;
    let groups = snapshot.item_groups();
    let descriptions: BTreeMap<String, Option<String>> =
        live::read_group_descriptions(thorium, &groups)
            .await
            .into_iter()
            .collect();
    for (group, description) in descriptions {
        snapshot
            .groups
            .insert(group, super::v2::types::GroupManifest { description });
    }
    Ok(snapshot)
}

/// Pick the named pipelines out of the pipelines of their groups
///
/// # Arguments
///
/// * `listed` - Every pipeline of the groups
/// * `specs` - The pipelines to pick
fn pick_pipelines(
    listed: &[thorium::models::Pipeline],
    specs: &[ResourceSpec],
) -> Result<Vec<thorium::models::Pipeline>, Error> {
    // every named pipeline must exist
    let mut picked = Vec::new();
    let mut missing = Vec::new();
    for spec in specs {
        match listed
            .iter()
            .find(|pipeline| pipeline.group == spec.group && pipeline.name == spec.name)
        {
            Some(pipeline) => picked.push(pipeline.clone()),
            None => missing.push(format!("pipeline {}/{}", spec.group, spec.name)),
        }
    }
    if !missing.is_empty() {
        return Err(Error::new(format!("Not found: {}", missing.join(", "))));
    }
    Ok(picked)
}
