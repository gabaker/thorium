//! The directory layout an exported toolbox is written in
//!
//! Every tool version, toolset, agent, workflow, and pipeline gets a directory holding a
//! `manifest.toml` and the file it points at (`spec_from` or `config_from`), plus the network
//! policy files a tool version bundles (`network_policies_from`), next to the assembled
//! `toolbox.json` that holds everything inline.

use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use thorium::Error;

use super::snapshot::spec_policies;
use crate::handlers::toolbox::build::spec_json;
use crate::handlers::toolbox::v2::plan_types::{ItemKind, ItemRef};
use crate::handlers::toolbox::v2::types::{ToolRef, ToolboxManifestV2};

/// A file to write into the exported toolbox
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OutFile {
    /// The file's path relative to the toolbox root
    pub path: PathBuf,
    /// The file's contents
    pub content: String,
}

/// Check that a name or label can be one component of a path
///
/// # Arguments
///
/// * `kind` - What the component names, for the error
/// * `component` - The name or label
fn check_component(kind: &str, component: &str) -> Result<(), Error> {
    // a component can't climb out of the toolbox or hide
    if component.is_empty() || component.starts_with('.') || component.contains(['/', '\\', '\0']) {
        return Err(Error::new(format!(
            "The {kind} '{component}' can't be used as a directory name in a toolbox"
        )));
    }
    Ok(())
}

/// Get the directory name of the group an item is written under
///
/// # Arguments
///
/// * `groups` - The groups the item is in
fn group_dir(groups: &[String]) -> Result<String, Error> {
    // items are filed under their first group
    let group = groups.first().map_or("default", String::as_str);
    check_component("group", group)?;
    Ok(group.to_owned())
}

/// Get the relative directory of an item, or of one version of a tool
///
/// # Arguments
///
/// * `manifest` - The manifest the item is in
/// * `item` - The item
/// * `label` - The version label, for a tool or a pipeline
/// * `overrides` - The directories items were asked to be placed in
fn item_dir(
    manifest: &ToolboxManifestV2,
    item: &ItemRef,
    label: Option<&str>,
    overrides: &HashMap<ItemRef, String>,
) -> Result<String, Error> {
    check_component(item.kind.as_str(), &item.name)?;
    // find the groups and the directory root of this kind
    let (root, groups, multiple) = match item.kind {
        ItemKind::Tool => {
            let tool = &manifest.tools[&item.name];
            ("tools", tool.groups.clone(), tool.versions.len() > 1)
        }
        ItemKind::Toolset => (
            "toolsets",
            manifest.toolsets[&item.name].groups.clone(),
            false,
        ),
        ItemKind::Agent => ("agents", manifest.agents[&item.name].groups.clone(), false),
        ItemKind::Workflow => (
            "workflows",
            manifest.workflows[&item.name].groups.clone(),
            false,
        ),
        ItemKind::Pipeline => {
            let pipeline = &manifest.pipelines[&item.name];
            let groups = pipeline
                .versions
                .values()
                .filter_map(|version| version.config.as_ref())
                .map(|config| config.group.clone())
                .collect();
            ("pipelines", groups, pipeline.versions.len() > 1)
        }
    };
    // a placement named on the command line replaces the default directory
    let mut dir = match overrides.get(item) {
        Some(placed) => placed.clone(),
        None => format!("{root}/{}/{}", group_dir(&groups)?, item.name),
    };
    // a tool or pipeline with several versions keeps each in its own directory
    if multiple && let Some(label) = label {
        check_component("version label", label)?;
        dir = format!("{dir}/{label}");
    }
    Ok(dir)
}

/// Set the `dir` of every tool version and pipeline version of a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest to fill in
/// * `overrides` - The directories items were asked to be placed in
pub(super) fn assign_dirs(
    manifest: &mut ToolboxManifestV2,
    overrides: &HashMap<ItemRef, String>,
) -> Result<(), Error> {
    // work out every directory first, since they are read from the whole manifest
    let mut tool_dirs = Vec::new();
    for (name, tool) in &manifest.tools {
        for label in tool.versions.keys() {
            let item = ItemRef::new(ItemKind::Tool, name);
            let dir = item_dir(manifest, &item, Some(label), overrides)?;
            tool_dirs.push((name.clone(), label.clone(), dir));
        }
    }
    let mut pipeline_dirs = Vec::new();
    for (name, pipeline) in &manifest.pipelines {
        for label in pipeline.versions.keys() {
            let item = ItemRef::new(ItemKind::Pipeline, name);
            let dir = item_dir(manifest, &item, Some(label), overrides)?;
            pipeline_dirs.push((name.clone(), label.clone(), dir));
        }
    }
    // then write them onto the versions
    for (name, label, dir) in tool_dirs {
        if let Some(version) = manifest
            .tools
            .get_mut(&name)
            .and_then(|tool| tool.versions.get_mut(&label))
        {
            version.dir = dir;
        }
    }
    for (name, label, dir) in pipeline_dirs {
        if let Some(version) = manifest
            .pipelines
            .get_mut(&name)
            .and_then(|pipeline| pipeline.versions.get_mut(&label))
        {
            version.dir = dir;
        }
    }
    Ok(())
}

/// Render a TOML table as text
///
/// # Arguments
///
/// * `table` - The table to render
fn toml_text(table: &toml::Table) -> Result<String, Error> {
    // render the table as pretty TOML
    toml::to_string_pretty(table)
        .map_err(|err| Error::new(format!("Failed to write a manifest.toml: {err}")))
}

/// Render a value as YAML
///
/// # Arguments
///
/// * `value` - The value to render
fn yaml_text<T: Serialize>(value: &T) -> Result<String, Error> {
    // render the value as YAML
    serde_norway::to_string(value)
        .map_err(|err| Error::new(format!("Failed to write a YAML file: {err}")))
}

/// Build the TOML array of tables of tool references
///
/// # Arguments
///
/// * `tools` - The references
fn ref_tables(tools: &[ToolRef]) -> toml::Value {
    // one table per reference, leaving out an unpinned version
    toml::Value::Array(
        tools
            .iter()
            .map(|tool| {
                let mut table = toml::Table::new();
                table.insert("tool".to_owned(), toml::Value::String(tool.tool.clone()));
                if let Some(version) = &tool.version {
                    table.insert("version".to_owned(), toml::Value::String(version.clone()));
                }
                toml::Value::Table(table)
            })
            .collect(),
    )
}

/// Build a TOML list of strings
///
/// # Arguments
///
/// * `items` - The strings
fn string_list(items: &[String]) -> toml::Value {
    // an array of strings
    toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect())
}

/// Build the files of one tool version
///
/// # Arguments
///
/// * `manifest` - The manifest the tool is in
/// * `name` - The tool's name
/// * `label` - The version's label
fn tool_files(
    manifest: &ToolboxManifestV2,
    name: &str,
    label: &str,
) -> Result<Vec<OutFile>, Error> {
    let tool = &manifest.tools[name];
    let version = &tool.versions[label];
    let spec_file = format!("{name}.json");
    // the manifest.toml names the spec file and carries the tool's root settings
    let mut table = toml::Table::new();
    let mut put = |key: &str, value: toml::Value| {
        table.insert(key.to_owned(), value);
    };
    put("type", toml::Value::String("tool".to_owned()));
    put("name", toml::Value::String(name.to_owned()));
    put("version", toml::Value::String(label.to_owned()));
    if let Some(kind) = tool.kind {
        put("kind", toml::Value::String(kind.to_string()));
    }
    put("groups", string_list(&tool.groups));
    if let Some(description) = &tool.description {
        put("description", toml::Value::String(description.clone()));
    }
    put("spec_from", toml::Value::String(spec_file.clone()));
    if tool.latest.as_deref() == Some(label) {
        put("latest", toml::Value::Boolean(true));
    }
    if !tool.harness_env.is_empty() {
        put("harness_env", string_list(&tool.harness_env));
    }
    if !tool.disabled_models.is_empty() {
        put("disabled_models", string_list(&tool.disabled_models));
    }
    if !version.provides.is_empty() {
        put("provides", ref_tables(&version.provides));
    }
    if !tool.tags.is_empty() {
        let tags = tool
            .tags
            .iter()
            .map(|(key, values)| (key.clone(), string_list(values)))
            .collect();
        put("tags", toml::Value::Table(tags));
    }
    let dir = PathBuf::from(&version.dir);
    let spec = version
        .spec
        .as_ref()
        .ok_or_else(|| Error::new(format!("tool {name}@{label} has no spec to write")))?;
    // the network policies the spec names and the export holds travel with the version as policy
    // files its manifest bundles, so building the exported directory brings them along
    let names = spec_policies(spec);
    let policies: Vec<_> = manifest
        .network_policies
        .iter()
        .filter(|policy| names.contains(&policy.name))
        .collect();
    let mut policy_files = Vec::new();
    for policy in &policies {
        check_component("network policy", &policy.name)?;
        let json = serde_json::to_string_pretty(policy).map_err(|err| {
            Error::new(format!(
                "Failed to write the network policy {}: {err}",
                policy.name
            ))
        })?;
        policy_files.push(OutFile {
            path: dir.join(format!("{}.policy.json", policy.name)),
            content: format!("{json}\n"),
        });
    }
    if !policy_files.is_empty() {
        let bundled: Vec<String> = policies
            .iter()
            .map(|policy| format!("{}.policy.json", policy.name))
            .collect();
        table.insert("network_policies_from".to_owned(), string_list(&bundled));
    }
    let mut files = vec![
        OutFile {
            path: dir.join("manifest.toml"),
            content: toml_text(&table)?,
        },
        OutFile {
            path: dir.join(spec_file),
            content: spec_json(spec)?,
        },
    ];
    files.append(&mut policy_files);
    Ok(files)
}

/// Build the files of a toolset, agent, or workflow
///
/// # Arguments
///
/// * `manifest` - The manifest the item is in
/// * `item` - The item
/// * `overrides` - The directories items were asked to be placed in
fn entity_files(
    manifest: &ToolboxManifestV2,
    item: &ItemRef,
    overrides: &HashMap<ItemRef, String>,
) -> Result<Vec<OutFile>, Error> {
    // each kind has its own type, file suffix, groups, and document
    let (kind, groups, document) = match item.kind {
        ItemKind::Toolset => {
            let toolset = &manifest.toolsets[&item.name];
            ("toolset", toolset.groups.clone(), yaml_text(toolset)?)
        }
        ItemKind::Agent => {
            let agent = &manifest.agents[&item.name];
            ("agent", agent.groups.clone(), yaml_text(agent)?)
        }
        _ => {
            let workflow = &manifest.workflows[&item.name];
            ("workflow", workflow.groups.clone(), yaml_text(workflow)?)
        }
    };
    let file = format!("{}.{kind}.yaml", item.name);
    let mut table = toml::Table::new();
    table.insert("type".to_owned(), toml::Value::String(kind.to_owned()));
    table.insert("name".to_owned(), toml::Value::String(item.name.clone()));
    table.insert("groups".to_owned(), string_list(&groups));
    table.insert("spec_from".to_owned(), toml::Value::String(file.clone()));
    let dir = PathBuf::from(item_dir(manifest, item, None, overrides)?);
    Ok(vec![
        OutFile {
            path: dir.join("manifest.toml"),
            content: toml_text(&table)?,
        },
        OutFile {
            path: dir.join(file),
            content: document,
        },
    ])
}

/// Build the files of one pipeline version
///
/// # Arguments
///
/// * `manifest` - The manifest the pipeline is in
/// * `name` - The pipeline's name
/// * `label` - The version's label
fn pipeline_files(
    manifest: &ToolboxManifestV2,
    name: &str,
    label: &str,
) -> Result<Vec<OutFile>, Error> {
    let version = &manifest.pipelines[name].versions[label];
    let config = version
        .config
        .as_ref()
        .ok_or_else(|| Error::new(format!("pipeline {name}@{label} has no config to write")))?;
    let file = format!("{name}.json");
    // the manifest.toml names the config file and each stage's tool
    let mut table = toml::Table::new();
    table.insert(
        "type".to_owned(),
        toml::Value::String("pipeline".to_owned()),
    );
    table.insert("name".to_owned(), toml::Value::String(name.to_owned()));
    table.insert("version".to_owned(), toml::Value::String(label.to_owned()));
    table.insert("config_from".to_owned(), toml::Value::String(file.clone()));
    let mut stages = toml::Table::new();
    for (stage, tool) in &version.stage_tools {
        let mut entry = toml::Table::new();
        entry.insert("tool".to_owned(), toml::Value::String(tool.tool.clone()));
        if let Some(pinned) = &tool.version {
            entry.insert("version".to_owned(), toml::Value::String(pinned.clone()));
        }
        stages.insert(stage.clone(), toml::Value::Table(entry));
    }
    table.insert("stage_tools".to_owned(), toml::Value::Table(stages));
    // the config file is the pipeline request without its stage tools
    let mut request = config.to_request(std::collections::BTreeMap::new());
    request.stage_tools.clear();
    let json = serde_json::to_string_pretty(&request)
        .map_err(|err| Error::new(format!("Failed to write the config of {name}: {err}")))?;
    let dir = PathBuf::from(&version.dir);
    Ok(vec![
        OutFile {
            path: dir.join("manifest.toml"),
            content: toml_text(&table)?,
        },
        OutFile {
            path: dir.join(file),
            content: format!("{json}\n"),
        },
    ])
}

/// Build every file of one item of a manifest whose directories are assigned
///
/// # Arguments
///
/// * `manifest` - The manifest the item is in
/// * `item` - The item
/// * `overrides` - The directories items were asked to be placed in
pub(super) fn item_files(
    manifest: &ToolboxManifestV2,
    item: &ItemRef,
    overrides: &HashMap<ItemRef, String>,
) -> Result<Vec<OutFile>, Error> {
    // tools and pipelines write one set of files per version
    let mut files = Vec::new();
    match item.kind {
        ItemKind::Tool => {
            for label in manifest.tools[&item.name].versions.keys() {
                files.extend(tool_files(manifest, &item.name, label)?);
            }
        }
        ItemKind::Pipeline => {
            for label in manifest.pipelines[&item.name].versions.keys() {
                files.extend(pipeline_files(manifest, &item.name, label)?);
            }
        }
        _ => files.extend(entity_files(manifest, item, overrides)?),
    }
    Ok(files)
}
