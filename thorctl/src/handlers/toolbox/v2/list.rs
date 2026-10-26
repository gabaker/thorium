//! `thorctl toolbox list`: what a toolbox manifest contains

use serde::{Deserialize, Serialize};
use thorium::Error;

use super::load::load_any;
use super::types::ToolboxManifestV2;
use crate::args::toolbox::{ListKind, ListToolbox};
use crate::handlers::progress::{Bar, BarKind};
use crate::utils::output::{self, OutputFormat, View, table_from};

/// One item of a manifest as `toolbox list` shows it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    /// The kind of item: `tool`, `toolset`, `agent`, `workflow`, `pipeline`, or `build`
    pub kind: String,
    /// The item's name
    pub name: String,
    /// The groups the item is in
    pub groups: Vec<String>,
    /// The item's version labels (tools, pipelines, and builds)
    pub versions: Vec<String>,
    /// The item's description, when it has one
    pub description: Option<String>,
}

/// Every entry `toolbox list` shows, as one document
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ListView(pub Vec<ListEntry>);

impl View for ListView {
    /// Render the entries as a table
    fn table(&self) -> String {
        // one row per entry
        let rows = self
            .0
            .iter()
            .map(|entry| {
                vec![
                    entry.kind.clone(),
                    entry.name.clone(),
                    entry.groups.join(","),
                    entry.versions.join(","),
                ]
            })
            .collect();
        table_from(&["KIND", "NAME", "GROUPS", "VERSIONS"], rows)
    }

    /// Render the entries as text, one line each
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text
    fn human(&self, _ansi: bool) -> String {
        // one line per entry with its groups, versions, and description
        self.0
            .iter()
            .map(|entry| {
                let mut parts = vec![format!("{} {}", entry.kind, entry.name)];
                if !entry.groups.is_empty() {
                    parts.push(format!("[{}]", entry.groups.join(",")));
                }
                if !entry.versions.is_empty() {
                    parts.push(format!("versions: {}", entry.versions.join(",")));
                }
                // only the first non-blank line of a long description is shown
                let first = entry
                    .description
                    .iter()
                    .flat_map(|description| description.lines())
                    .find(|text| !text.trim().is_empty());
                if let Some(first) = first {
                    parts.push(format!("- {}", first.trim()));
                }
                parts.join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Build the entries of a manifest, optionally of one kind
///
/// Entries come in the order tools, toolsets, agents, workflows, pipelines, builds, each sorted
/// by name.
///
/// # Arguments
///
/// * `manifest` - The manifest to list
/// * `kind` - Only list this kind of item
#[must_use]
pub fn entries(manifest: &ToolboxManifestV2, kind: Option<ListKind>) -> Vec<ListEntry> {
    let wanted = |candidate: ListKind| kind.is_none_or(|kind| kind == candidate);
    let mut entries = Vec::new();
    // tools
    if wanted(ListKind::Tools) {
        for (name, tool) in &manifest.tools {
            entries.push(ListEntry {
                kind: "tool".to_owned(),
                name: name.clone(),
                groups: tool.groups.clone(),
                versions: tool.versions.keys().cloned().collect(),
                description: tool.description.clone(),
            });
        }
    }
    // toolsets
    if wanted(ListKind::Toolsets) {
        for (name, toolset) in &manifest.toolsets {
            entries.push(ListEntry {
                kind: "toolset".to_owned(),
                name: name.clone(),
                groups: toolset.groups.clone(),
                versions: Vec::new(),
                description: toolset.description.clone(),
            });
        }
    }
    // agents
    if wanted(ListKind::Agents) {
        for (name, agent) in &manifest.agents {
            entries.push(ListEntry {
                kind: "agent".to_owned(),
                name: name.clone(),
                groups: agent.groups.clone(),
                versions: Vec::new(),
                description: agent.description.clone(),
            });
        }
    }
    // workflows
    if wanted(ListKind::Workflows) {
        for (name, workflow) in &manifest.workflows {
            entries.push(ListEntry {
                kind: "workflow".to_owned(),
                name: name.clone(),
                groups: workflow.groups.clone(),
                versions: Vec::new(),
                description: workflow.description.clone(),
            });
        }
    }
    // pipelines, in the group of their first version's config
    if wanted(ListKind::Pipelines) {
        for (name, pipeline) in &manifest.pipelines {
            let groups: std::collections::BTreeSet<String> = pipeline
                .versions
                .values()
                .filter_map(|version| version.config.as_ref())
                .map(|config| config.group.clone())
                .collect();
            let description = pipeline
                .versions
                .values()
                .map(|version| version.description.clone())
                .find(|text| !text.is_empty());
            entries.push(ListEntry {
                kind: "pipeline".to_owned(),
                name: name.clone(),
                groups: groups.into_iter().collect(),
                versions: pipeline.versions.keys().cloned().collect(),
                description,
            });
        }
    }
    // build-only contexts
    if wanted(ListKind::Builds) {
        for (name, builds) in &manifest.builds {
            entries.push(ListEntry {
                kind: "build".to_owned(),
                name: name.clone(),
                groups: Vec::new(),
                versions: builds.keys().cloned().collect(),
                description: None,
            });
        }
    }
    entries
}

/// List what a toolbox manifest contains
///
/// # Arguments
///
/// * `cmd` - The `toolbox list` command
pub async fn list(cmd: &ListToolbox) -> Result<(), Error> {
    // read the manifest without drawing a progress bar, since the listing goes to stdout
    let bar = Bar::new_or_quiet(
        "toolbox list",
        "Reading manifest...",
        BarKind::UnboundIO,
        true,
    );
    let loaded = load_any(&cmd.manifest, &bar).await?;
    // print the entries in the requested format, as a table by default
    let view = ListView(entries(&loaded.manifest, cmd.kind));
    output::print(&view, cmd.format.format.unwrap_or(OutputFormat::Table))
}
