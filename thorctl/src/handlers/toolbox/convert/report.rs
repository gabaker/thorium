//! What a toolbox conversion reports

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thorium::client::migrations::MigrationReason;

use crate::handlers::toolbox::v2::load::{SourceFormat, StageMapping};
use crate::utils::output::{View, table_from};

/// Get the code a conversion issue is shown with
///
/// A dependency name that matches no converted tool is shown as `dropped_dependency:KIND`; every
/// other reason keeps the migration's code.
///
/// # Arguments
///
/// * `reason` - The migration reason behind the issue
#[must_use]
pub fn issue_code(reason: &MigrationReason) -> String {
    // a dropped name in a dependency list has its own code
    let code = reason.code();
    match code
        .strip_prefix("invalid_value:dependencies.")
        .and_then(|rest| rest.strip_suffix(".images.*"))
    {
        Some(kind) => format!("dropped_dependency:{kind}"),
        None => code,
    }
}

/// Say what an issue's code means for a converted toolbox
///
/// # Arguments
///
/// * `code` - The issue's code
#[must_use]
pub fn issue_meaning(code: &str) -> String {
    // each code the conversion raises has a fixed meaning
    let meaning = match code {
        "missing_field:image" => "names no container image, so it was not converted into a tool",
        "missing_field:config" => "has no config to convert, so it was left out",
        "invalid_value:name" | "invalid_value:version" => {
            "has a name or version label that can't be used, so it was not converted"
        }
        "invalid_value:stages.*" => "runs a stage whose tool was not converted",
        _ if code.starts_with("dropped_dependency:") => {
            "names a tool that was not converted in its dependency list; the name was removed"
        }
        _ if code.starts_with("invalid_value:dependencies.") => {
            "depends only on tools that were not converted, which would allow every tool, so it \
             was not converted"
        }
        _ => "was not converted",
    };
    meaning.to_owned()
}

/// A problem found while converting a toolbox
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvertIssue {
    /// The code of the problem, such as `missing_field:image`
    pub code: String,
    /// The group the item is in (empty when it has none)
    pub group: String,
    /// The item the problem is about, such as `image clamav@latest`
    pub item: String,
    /// What the problem means for the converted toolbox
    pub message: String,
    /// Whether the item was left out of the converted toolbox
    pub skipped: bool,
}

/// A note or a warning about an item of the converted toolbox
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvertNote {
    /// The code of the note, such as `warning:secret-env` or `note:filled:image`
    pub code: String,
    /// The item the note is about
    pub item: String,
    /// What the note says
    pub message: String,
}

/// What a conversion produced and everything it reported
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertSummary {
    /// The toolbox's name
    pub name: String,
    /// The format the input was read in
    pub source: SourceFormat,
    /// Where the converted manifest was (or would be) written, when it is a file
    pub output: Option<String>,
    /// The number of tools in the converted manifest
    pub tools: usize,
    /// The number of tool versions across every tool
    pub tool_versions: usize,
    /// The number of build-only image labels
    pub builds: usize,
    /// The number of pipelines in the converted manifest
    pub pipelines: usize,
    /// The number of network policies in the converted manifest
    pub network_policies: usize,
    /// The tool each converted pipeline stage runs
    pub mappings: Vec<StageMapping>,
    /// The problems found, including every item that was skipped
    pub issues: Vec<ConvertIssue>,
    /// Warnings that skip nothing
    pub warnings: Vec<ConvertNote>,
    /// Informational notes
    pub notes: Vec<ConvertNote>,
    /// The names more than one section of the input defines
    pub collisions: Vec<String>,
}

impl ConvertSummary {
    /// Whether anything was skipped, warned about, or raised as an issue
    #[must_use]
    pub fn has_issues(&self) -> bool {
        // a collision is an issue too, since it stops the conversion
        !self.issues.is_empty() || !self.warnings.is_empty() || !self.collisions.is_empty()
    }
}

impl ConvertSummary {
    /// Count the issues that left an item out of the converted toolbox
    #[must_use]
    pub fn skipped(&self) -> usize {
        // an issue is a skip when its item was left out
        self.issues.iter().filter(|issue| issue.skipped).count()
    }
}

impl View for ConvertSummary {
    /// Render every issue, warning, and note as a table
    fn table(&self) -> String {
        // one row per entry, most serious first
        let rows = self
            .issues
            .iter()
            .map(|issue| ("issue", &issue.code, &issue.item, &issue.message))
            .chain(
                self.warnings
                    .iter()
                    .map(|note| ("warning", &note.code, &note.item, &note.message)),
            )
            .chain(
                self.notes
                    .iter()
                    .map(|note| ("note", &note.code, &note.item, &note.message)),
            )
            .map(|(kind, code, item, message)| {
                vec![kind.to_owned(), code.clone(), item.clone(), message.clone()]
            })
            .collect();
        table_from(&["KIND", "CODE", "ITEM", "MESSAGE"], rows)
    }

    /// Render the summary as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text
    fn human(&self, _ansi: bool) -> String {
        // the header says what was read and what it became
        let mut lines = vec![
            format!(
                "Converted toolbox '{}' ({} format)",
                self.name,
                self.source.as_str()
            ),
            format!(
                "  {} tools ({} versions), {} builds, {} pipelines, {} network policies",
                self.tools, self.tool_versions, self.builds, self.pipelines, self.network_policies
            ),
        ];
        // each pipeline with the tool every stage runs
        let mut stages: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for mapping in &self.mappings {
            let pinned = mapping
                .version
                .as_ref()
                .map_or_else(String::new, |version| format!("@{version}"));
            stages
                .entry(mapping.pipeline.as_str())
                .or_default()
                .push(format!("{} -> {}{pinned}", mapping.stage, mapping.tool));
        }
        for (pipeline, stages) in stages {
            lines.push(format!("  pipeline {pipeline}: {}", stages.join(", ")));
        }
        // then everything worth a look, and the verdict
        lines.extend(
            self.issues
                .iter()
                .map(|issue| format!("issue[{}] {}: {}", issue.code, issue.item, issue.message)),
        );
        lines.extend(
            self.warnings
                .iter()
                .map(|note| format!("warning[{}] {}: {}", note.code, note.item, note.message)),
        );
        lines.extend(
            self.notes
                .iter()
                .map(|note| format!("note[{}] {}: {}", note.code, note.item, note.message)),
        );
        for collision in &self.collisions {
            lines.push(format!(
                "issue[collision:{collision}] {collision}: is defined more than once, in more \
                 than one group or section"
            ));
        }
        lines.push(format!(
            "Result: {} issues ({} skipped), {} warnings",
            self.issues.len() + self.collisions.len(),
            self.skipped(),
            self.warnings.len()
        ));
        lines.join("\n")
    }
}
