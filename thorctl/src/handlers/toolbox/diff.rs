//! Diff a toolbox against what a Thorium instance holds
//!
//! The diff is the read-only plan an import of the toolbox would follow. A tool version is
//! compared by spec hash: it is unchanged when some instance of the tool already has its spec, a
//! label conflict when the label exists with another spec, and new otherwise. Toolsets, agents,
//! workflows, and pipelines are compared field by field after both sides are normalized, and a
//! legacy toolbox is converted in memory first, so its images are compared as the tools an import
//! would create from them.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use thorium::client::migrations::{ToolIdMap, convert_image_request};
use thorium::models::ToolVersion;
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use super::export::live::{load_manifest_for, read_group_states, read_live, read_policy_states};
use crate::args::toolbox::DiffToolbox;
use crate::handlers::toolbox::v2::plan_types::{
    GroupState, ItemState, Plan, PlanSummary, PolicyState, VersionClass,
};
use crate::handlers::toolbox::v2::refs::{Ref, unresolve_slots};
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;
use crate::utils::output::{self, OutputFormat, View};

mod classify;
#[cfg(test)]
mod tests;

/// How many tool versions fall in each class
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct VersionCounts {
    /// Versions of tools that don't exist yet
    pub new: usize,
    /// Versions an existing tool lacks
    pub new_version: usize,
    /// Versions whose label exists with another spec
    pub label_conflict: usize,
    /// Versions an instance of the tool already has
    pub unchanged: usize,
}

/// What a diff found, as printed
#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    /// Whether an import would change anything
    pub changed: bool,
    /// How many items are in each state
    pub summary: PlanSummary,
    /// How many tool versions are in each class
    pub versions: VersionCounts,
    /// The whole plan
    pub plan: Plan,
    /// What was left out of the comparison and why
    pub notes: Vec<String>,
}

/// Count the tool versions of a plan by class
///
/// # Arguments
///
/// * `plan` - The plan to count
fn count_versions(plan: &Plan) -> VersionCounts {
    // tally every version of every item
    let mut counts = VersionCounts::default();
    for version in plan.items.iter().flat_map(|item| &item.versions) {
        match version.class {
            VersionClass::New => counts.new += 1,
            VersionClass::NewVersion => counts.new_version += 1,
            VersionClass::LabelConflict => counts.label_conflict += 1,
            VersionClass::Unchanged => counts.unchanged += 1,
        }
    }
    counts
}

/// Whether an import of a plan would change anything
///
/// A network policy that differs counts too, though an import only replaces it when asked.
///
/// # Arguments
///
/// * `plan` - The plan to check
fn is_actionable(plan: &Plan) -> bool {
    // anything that is written, or a policy that differs
    plan.has_changes()
        || plan
            .policies
            .iter()
            .any(|policy| policy.state == PolicyState::Differs)
}

impl DiffReport {
    /// Build the report of a plan
    ///
    /// # Arguments
    ///
    /// * `plan` - The plan
    /// * `notes` - What was left out of the comparison
    fn new(plan: Plan, notes: Vec<String>) -> Self {
        // summarize the plan
        DiffReport {
            changed: is_actionable(&plan),
            summary: plan.summary(),
            versions: count_versions(&plan),
            plan,
            notes,
        }
    }

    /// Render the report's lines
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to color additions and removals
    fn lines(&self, ansi: bool) -> Vec<String> {
        let paint = |code: &str, text: String| {
            if ansi {
                format!("\x1b[{code}m{text}\x1b[0m")
            } else {
                text
            }
        };
        let mut lines = Vec::new();
        // groups that don't exist yet
        for group in &self.plan.groups {
            match group.state {
                GroupState::Missing => {
                    lines.push(paint("32", format!("+ group {} (missing)", group.name)));
                }
                GroupState::Forbidden => lines.push(paint(
                    "31",
                    format!("! group {} (you can't write to it)", group.name),
                )),
                GroupState::Exists => {}
            }
        }
        // network policies
        for policy in &self.plan.policies {
            match policy.state {
                PolicyState::Create => {
                    lines.push(paint(
                        "32",
                        format!("+ network policy {} (new)", policy.name),
                    ));
                }
                PolicyState::Differs => {
                    lines.push(paint(
                        "33",
                        format!("~ network policy {} (differs)", policy.name),
                    ));
                }
                PolicyState::Reuse => {}
            }
        }
        // each item: a line per changed tool version, a line or a hunk for the other kinds
        for item in &self.plan.items {
            for version in &item.versions {
                let name = &item.item.name;
                let label = &version.label;
                match version.class {
                    VersionClass::New => {
                        lines.push(paint("32", format!("+ {name}@{label} (new)")));
                    }
                    VersionClass::NewVersion => {
                        lines.push(paint("32", format!("+ {name}@{label} (new version)")));
                    }
                    VersionClass::LabelConflict => lines.push(paint(
                        "33",
                        format!("~ {name}@{label} (label conflict: Thorium has another spec under this label)"),
                    )),
                    VersionClass::Unchanged => {}
                }
            }
            if item.versions.is_empty() {
                match (item.state, &item.diff) {
                    (ItemState::Create, _) => {
                        lines.push(paint("32", format!("+ {} (new)", item.item)));
                    }
                    (ItemState::Update, Some(diff)) => {
                        for line in diff.lines() {
                            let code = if line.starts_with('+') {
                                "32"
                            } else if line.starts_with('-') {
                                "31"
                            } else {
                                "36"
                            };
                            lines.push(paint(code, line.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
        }
        for note in &self.notes {
            lines.push(format!("note: {note}"));
        }
        for note in &self.plan.notes {
            lines.push(format!("note: {note}"));
        }
        // the summary line
        lines.push(format!(
            "{} to create, {} to update, {} unchanged; tool versions: {} new, {} new version, {} \
             label conflict, {} unchanged",
            self.summary.create,
            self.summary.update,
            self.summary.unchanged,
            self.versions.new,
            self.versions.new_version,
            self.versions.label_conflict,
            self.versions.unchanged
        ));
        lines
    }
}

impl View for DiffReport {
    /// Render the plan as a table with a row per item
    fn table(&self) -> String {
        // one row per item with its state and what happens to each version
        let rows = self
            .plan
            .items
            .iter()
            .map(|item| {
                let versions = item
                    .versions
                    .iter()
                    .map(|version| format!("{}:{:?}", version.label, version.class))
                    .collect::<Vec<_>>()
                    .join(",");
                vec![
                    item.item.kind.to_string(),
                    item.item.name.clone(),
                    item.group.clone(),
                    format!("{:?}", item.state),
                    versions,
                ]
            })
            .collect();
        output::table_from(&["KIND", "NAME", "GROUP", "STATE", "VERSIONS"], rows)
    }

    /// Render the diff as text for people
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to color additions and removals
    fn human(&self, ansi: bool) -> String {
        // the lines, joined
        self.lines(ansi).join("\n")
    }
}

/// Build the specs of the tool versions that came from legacy images
///
/// A version whose image can't be converted is removed and noted.
///
/// # Arguments
///
/// * `manifest` - The manifest to fill in
/// * `notes` - Where to note what was left out
fn fill_legacy_specs(manifest: &mut ToolboxManifestV2, notes: &mut Vec<String>) {
    // every tool that came from an image has an id the conversion refers to it by
    let names: Vec<(String, String)> = manifest
        .tools
        .iter()
        .filter_map(|(name, tool)| Some((tool.groups.first()?.clone(), name.clone())))
        .collect();
    let map = ToolIdMap::from_names(
        names
            .iter()
            .map(|(group, name)| (group.as_str(), name.as_str())),
    );
    let by_id: HashMap<Uuid, String> = names
        .iter()
        .map(|(group, name)| (ToolIdMap::tool_id(group, name), name.clone()))
        .collect();
    let mut dropped = Vec::new();
    for (name, tool) in &mut manifest.tools {
        let Some(group) = tool.groups.first().cloned() else {
            continue;
        };
        for (label, version) in &mut tool.versions {
            // only a version with no spec that has a legacy config needs converting
            let (None, Some(request)) = (
                &version.spec,
                version
                    .legacy
                    .as_ref()
                    .and_then(|legacy| legacy.config.as_ref()),
            ) else {
                continue;
            };
            match convert_image_request(&group, name, ToolVersion::from(label), request, &map) {
                Ok(migration) => {
                    // the converted spec names its dependencies by the ids of the map
                    let mut spec =
                        serde_json::to_value(&migration.tool.spec).unwrap_or(Value::Null);
                    unresolve_slots(&mut spec, |id, _| {
                        by_id.get(&id).map(|tool| Ref::Tool {
                            name: tool.clone(),
                            label: None,
                        })
                    });
                    version.spec = Some(spec);
                    version.legacy = None;
                }
                Err(skipped) => {
                    notes.push(format!(
                        "image {name} ({label}) can't be compared: {}",
                        skipped.reason.code()
                    ));
                    dropped.push((name.clone(), label.clone()));
                }
            }
        }
    }
    // leave out what could not be converted
    for (name, label) in dropped {
        if let Some(tool) = manifest.tools.get_mut(&name) {
            tool.versions.remove(&label);
        }
    }
    manifest.tools.retain(|_, tool| !tool.versions.is_empty());
}

/// Rewrite the image url of the tools a toolbox bundles as an import with a prefix would
///
/// # Arguments
///
/// * `manifest` - The manifest to change
/// * `prefix` - The registry base path the images are pushed under
fn apply_image_prefix(manifest: &mut ToolboxManifestV2, prefix: &str) {
    // an image is stored at `<prefix>/<group>/<name>:<tag>`
    let prefix = prefix.trim_end_matches('/');
    for (name, tool) in &mut manifest.tools {
        let group = tool.groups.first().cloned().unwrap_or_default();
        for version in tool.versions.values_mut() {
            let Some(spec) = &mut version.spec else {
                continue;
            };
            for pointer in [
                "/Image/environment/K8s/image",
                "/Harness/workload/environment/K8s/image",
            ] {
                let Some(image) = spec.pointer_mut(pointer) else {
                    continue;
                };
                let tag = image
                    .as_str()
                    .and_then(|url| url.rsplit_once(':'))
                    .map_or("latest", |(_, tag)| tag)
                    .to_owned();
                *image = Value::String(format!("{prefix}/{group}/{name}:{tag}"));
            }
        }
    }
}

/// Load a manifest and put it in the form it is compared in
///
/// # Arguments
///
/// * `cmd` - The diff command
/// * `notes` - Where to note what was left out
async fn load_manifest(
    cmd: &DiffToolbox,
    notes: &mut Vec<String>,
) -> Result<ToolboxManifestV2, Error> {
    // read the manifest, in the group it was imported into if that was overridden
    let mut manifest =
        load_manifest_for("toolbox diff", &cmd.manifest, cmd.group_override.as_deref()).await?;
    // legacy images are compared as the tools they convert to
    fill_legacy_specs(&mut manifest, notes);
    // a toolbox that bundles its images is compared at the url an import would give them
    let prefix = cmd
        .image_path_prefix
        .clone()
        .or_else(|| manifest.image_path_prefix.clone());
    if manifest.bundled_images
        && let Some(prefix) = prefix
    {
        apply_image_prefix(&mut manifest, &prefix);
    }
    Ok(manifest)
}

/// Compare a toolbox with a Thorium instance and print the differences
///
/// Returns whether an import of the toolbox would change anything, so the caller can exit with
/// code 1 when `--exit-code` is set.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `_conf` - The thorctl config
/// * `cmd` - The diff command
/// * `workers` - The most reads in flight at once
pub async fn diff(
    thorium: Thorium,
    _conf: &CtlConf,
    cmd: &DiffToolbox,
    workers: usize,
) -> Result<bool, Error> {
    let mut notes = Vec::new();
    let manifest = load_manifest(cmd, &mut notes).await?;
    // read what Thorium holds that the manifest names
    let mut live = read_live(&thorium, &manifest, workers).await?;
    let policies: Vec<String> = manifest
        .network_policies
        .iter()
        .map(|policy| policy.name.clone())
        .collect();
    let groups = manifest.named_groups();
    live.policies = read_policy_states(&thorium, &policies, &groups).await?;
    live.groups = read_group_states(&thorium, &groups).await?;
    // classify every item and print the result
    let report = DiffReport::new(classify::plan(&manifest, &live), notes);
    output::print(&report, cmd.format.format.unwrap_or(OutputFormat::Human))?;
    Ok(cmd.exit_code && report.changed)
}
