//! Remove a toolbox's pipelines, workflows, agents, toolsets, and tools from Thorium
//!
//! The manifest is the source of truth for what to remove, and the removal follows the
//! dependency order: pipelines, workflows, agents, and toolsets first, then each tool's versions
//! and the tool, tools that others depend on last. A tool that something outside the toolbox
//! still uses is left in place and reported, as is one the API refuses to delete. Groups and
//! network policies are never deleted.

use colored::Colorize;
use http::StatusCode;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::IsTerminal;
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use super::export::live::{LiveView, load_manifest_for, read_live};
use super::v2::graph::{dependencies, waves};
use super::v2::plan_types::{ItemKind, ItemRef};
use super::v2::types::{ToolManifest, ToolboxManifestV2};
use crate::args::toolbox::RemoveToolbox;
use crate::handlers::progress::{Bar, BarKind};
use crate::utils;

#[cfg(test)]
mod tests;

/// One deletion in a removal
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Step {
    /// Delete a pipeline
    Pipeline {
        /// The pipeline's group
        group: String,
        /// The pipeline's name
        name: String,
    },
    /// Delete a toolset, agent, or workflow
    Entity {
        /// What kind of item it is
        kind: ItemKind,
        /// The entity's id
        id: Uuid,
        /// The entity's name
        name: String,
        /// The entity's first group
        group: String,
    },
    /// Delete one version of a tool
    ToolVersion {
        /// The tool's name
        tool: String,
        /// The version's label
        label: String,
        /// The tool instance's id
        id: Uuid,
        /// The tool's first group
        group: String,
    },
    /// Delete a tool once its versions are deleted
    Tool {
        /// The tool's name
        name: String,
        /// The tool's id
        id: Uuid,
        /// The tool's first group
        group: String,
    },
}

impl Step {
    /// Name what this step deletes
    pub fn label(&self) -> String {
        // each kind names itself with its group
        match self {
            Step::Pipeline { group, name } => {
                format!("pipeline {}", utils::resource_id(group, name))
            }
            Step::Entity {
                kind, name, group, ..
            } => format!("{kind} {}", utils::resource_id(group, name)),
            Step::ToolVersion {
                tool, label, group, ..
            } => format!("tool {}@{label}", utils::resource_id(group, tool)),
            Step::Tool { name, group, .. } => {
                format!("tool {}", utils::resource_id(group, name))
            }
        }
    }
}

/// Something a removal leaves in place
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    /// What is left, such as `tool static/clamav`
    pub item: String,
    /// Why it is left
    pub why: String,
}

/// What a removal deletes, leaves, and finds already gone
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemovalPlan {
    /// The deletions, in the order they are made
    pub steps: Vec<Step>,
    /// The items of the toolbox that Thorium doesn't have
    pub missing: Vec<String>,
    /// The items that are left in place
    pub kept: Vec<Kept>,
}

impl RemovalPlan {
    /// Whether nothing is deleted
    pub fn deletes_nothing(&self) -> bool {
        // no steps means no deletions
        self.steps.is_empty()
    }

    /// Print the plan
    pub fn print(&self) {
        // what is deleted, in order
        println!("{}", "Resources to remove:".bold());
        for step in &self.steps {
            if !matches!(step, Step::ToolVersion { .. }) {
                println!("  {}", step.label().red());
            }
        }
        // the versions of each tool, under the tools
        let versions: Vec<String> = self
            .steps
            .iter()
            .filter(|step| matches!(step, Step::ToolVersion { .. }))
            .map(Step::label)
            .collect();
        if !versions.is_empty() {
            println!("{}", "Tool versions to remove:".bold());
            for version in versions {
                println!("  {}", version.red());
            }
        }
        // what is already gone
        if !self.missing.is_empty() {
            println!("{}", "Not found in Thorium (skipped):".bold());
            for item in &self.missing {
                println!("  {item}");
            }
        }
        // what is left, and why
        if !self.kept.is_empty() {
            println!("{}", "Left in place:".bold());
            for kept in &self.kept {
                println!("  {} ({})", kept.item.yellow(), kept.why);
            }
        }
    }
}

/// Whether a live item shares one of the groups an item of the manifest is in
///
/// # Arguments
///
/// * `live` - The groups the live item is in
/// * `wanted` - The groups the manifest puts the item in (any when empty)
fn shares_group(live: &[String], wanted: &[String]) -> bool {
    // no groups in the manifest matches any live group
    wanted.is_empty() || live.iter().any(|group| wanted.contains(group))
}

/// Get the label a published conflict takes (`LABEL+` and a prefix of the spec hash) starts with
///
/// # Arguments
///
/// * `label` - The label in the manifest
fn conflict_prefix(label: &str) -> String {
    // a conflicting version is published as LABEL+hash
    format!("{label}+")
}

/// Plan the removal of a tool's versions and the tool
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `tool` - The tool in the manifest
/// * `live` - What Thorium holds
/// * `removed_pipelines` - The `(group, name)` of the pipelines the removal deletes
/// * `kept_tools` - The tools already left in place
/// * `dependents` - The tools of the manifest that depend on each tool
/// * `plan` - The plan being built
fn plan_tool(
    name: &str,
    tool: &ToolManifest,
    live: &LiveView,
    removed_pipelines: &HashSet<(String, String)>,
    kept_tools: &mut BTreeSet<String>,
    dependents: &BTreeMap<String, BTreeSet<String>>,
    plan: &mut RemovalPlan,
) {
    let item = format!("tool {name}");
    // a tool Thorium doesn't have is already gone
    let Some(found) = live.tool(name, &tool.groups) else {
        plan.missing.push(item);
        return;
    };
    let group = found.groups.first().cloned().unwrap_or_default();
    let item = format!("tool {}", utils::resource_id(&group, name));
    // a pipeline outside the toolbox that runs the tool or one of its versions keeps the tool
    let used_by: Vec<String> = live
        .pipelines
        .iter()
        .filter(|pipeline| found.groups.contains(&pipeline.group))
        .filter(|pipeline| {
            !removed_pipelines.contains(&(pipeline.group.clone(), pipeline.name.clone()))
        })
        .filter(|pipeline| {
            pipeline.stage_tools.values().any(|id| {
                *id == found.id || found.instances.iter().any(|instance| instance.id == *id)
            })
        })
        .map(|pipeline| {
            format!(
                "pipeline {}",
                utils::resource_id(&pipeline.group, &pipeline.name)
            )
        })
        .collect();
    if !used_by.is_empty() {
        kept_tools.insert(name.to_owned());
        plan.kept.push(Kept {
            item,
            why: format!("used by {}", used_by.join(", ")),
        });
        return;
    }
    // a tool that a tool left in place depends on is left too
    let kept_dependents: Vec<&String> = dependents
        .get(name)
        .into_iter()
        .flatten()
        .filter(|dependent| kept_tools.contains(*dependent))
        .collect();
    if !kept_dependents.is_empty() {
        kept_tools.insert(name.to_owned());
        let names: Vec<&str> = kept_dependents.iter().map(|name| name.as_str()).collect();
        plan.kept.push(Kept {
            item,
            why: format!(
                "tool {} depends on it and is left in place",
                names.join(", ")
            ),
        });
        return;
    }
    // the versions to delete are the ones the toolbox names, and the ones published beside them
    let mut others = Vec::new();
    let mut versions = Vec::new();
    for instance in &found.instances {
        let named = tool.versions.keys().any(|label| {
            instance.label == *label || instance.label.starts_with(&conflict_prefix(label))
        });
        if named {
            versions.push(Step::ToolVersion {
                tool: name.to_owned(),
                label: instance.label.clone(),
                id: instance.id,
                group: group.clone(),
            });
        } else {
            others.push(instance.label.clone());
        }
    }
    plan.steps.extend(versions);
    // a tool that still has other versions can't be deleted
    if others.is_empty() {
        plan.steps.push(Step::Tool {
            name: name.to_owned(),
            id: found.id,
            group,
        });
    } else {
        kept_tools.insert(name.to_owned());
        plan.kept.push(Kept {
            item,
            why: format!(
                "it has versions that are not in the toolbox: {}",
                others.join(", ")
            ),
        });
    }
}

/// Invert the dependency graph of a manifest's tools: who depends on each tool
///
/// # Arguments
///
/// * `manifest` - The manifest
fn tool_dependents(manifest: &ToolboxManifestV2) -> BTreeMap<String, BTreeSet<String>> {
    // an edge from each tool to the tools it depends on, turned around
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (item, edges) in dependencies(manifest) {
        if item.kind != ItemKind::Tool {
            continue;
        }
        for edge in edges.iter().filter(|edge| edge.kind == ItemKind::Tool) {
            dependents
                .entry(edge.name.clone())
                .or_default()
                .insert(item.name.clone());
        }
    }
    dependents
}

/// Plan what removing a manifest's items from Thorium deletes
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `live` - What Thorium holds
#[must_use]
pub fn plan_removal(manifest: &ToolboxManifestV2, live: &LiveView) -> RemovalPlan {
    // the items in dependency order, dependents first
    let mut order: Vec<ItemRef> = match waves(manifest) {
        Ok(waves) => waves.into_iter().flatten().collect(),
        Err(_) => super::export::items_of(manifest),
    };
    order.reverse();
    let mut plan = RemovalPlan::default();
    // pipelines come first
    let mut removed_pipelines = HashSet::new();
    for item in order.iter().filter(|item| item.kind == ItemKind::Pipeline) {
        let groups: Vec<String> = manifest.pipelines[&item.name]
            .versions
            .values()
            .filter_map(|version| version.config.as_ref())
            .map(|config| config.group.clone())
            .collect();
        match live.pipelines.iter().find(|pipeline| {
            pipeline.name == item.name
                && shares_group(std::slice::from_ref(&pipeline.group), &groups)
        }) {
            Some(pipeline) => {
                removed_pipelines.insert((pipeline.group.clone(), pipeline.name.clone()));
                plan.steps.push(Step::Pipeline {
                    group: pipeline.group.clone(),
                    name: pipeline.name.clone(),
                });
            }
            None => plan.missing.push(format!("pipeline {}", item.name)),
        }
    }
    // then workflows, agents, and toolsets
    for kind in [ItemKind::Workflow, ItemKind::Agent, ItemKind::Toolset] {
        for item in order.iter().filter(|item| item.kind == kind) {
            let groups = match kind {
                ItemKind::Workflow => manifest.workflows[&item.name].groups.clone(),
                ItemKind::Agent => manifest.agents[&item.name].groups.clone(),
                _ => manifest.toolsets[&item.name].groups.clone(),
            };
            match live.entities.iter().find(|entity| {
                entity.kind == kind
                    && entity.name == item.name
                    && shares_group(&entity.groups, &groups)
            }) {
                Some(entity) => plan.steps.push(Step::Entity {
                    kind,
                    id: entity.id,
                    name: entity.name.clone(),
                    group: entity.groups.first().cloned().unwrap_or_default(),
                }),
                None => plan.missing.push(format!("{kind} {}", item.name)),
            }
        }
    }
    // finally the tools, each after the tools that depend on it
    let dependents = tool_dependents(manifest);
    let mut kept_tools = BTreeSet::new();
    for item in order.iter().filter(|item| item.kind == ItemKind::Tool) {
        plan_tool(
            &item.name,
            &manifest.tools[&item.name],
            live,
            &removed_pipelines,
            &mut kept_tools,
            &dependents,
            &mut plan,
        );
    }
    plan
}

/// What carrying out a removal did
#[derive(Debug, Default)]
pub struct Outcome {
    /// The number of things deleted
    pub removed: usize,
    /// The things that were already gone
    pub gone: Vec<String>,
    /// The things the API refused to delete because something still refers to them
    pub refused: Vec<String>,
    /// The things that failed to delete for another reason
    pub failures: Vec<String>,
}

/// Delete one thing
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `step` - What to delete
async fn delete_step(thorium: &Thorium, step: &Step) -> Result<(), Error> {
    // each kind has its own delete
    match step {
        Step::Pipeline { group, name } => thorium.pipelines.delete(group, name).await.map(drop),
        Step::Entity { id, .. } | Step::ToolVersion { id, .. } | Step::Tool { id, .. } => {
            thorium.entities.delete(*id).await.map(drop)
        }
    }
}

/// Carry out a removal plan
///
/// One failure never aborts the rest, except that a tool whose version could not be deleted is
/// not itself deleted.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `plan` - The plan to carry out
/// * `progress` - The progress bar to report through
pub async fn execute(thorium: &Thorium, plan: &RemovalPlan, progress: &Bar) -> Outcome {
    let mut outcome = Outcome::default();
    // tools with a version that could not be deleted
    let mut stuck: HashSet<String> = HashSet::new();
    progress.refresh("Removing", BarKind::Bound(plan.steps.len() as u64));
    for step in &plan.steps {
        let label = step.label();
        // a tool still holding a version can't be deleted
        if let Step::Tool { name, .. } = step
            && stuck.contains(name)
        {
            progress.inc(1);
            continue;
        }
        match delete_step(thorium, step).await {
            Ok(()) => outcome.removed += 1,
            // a missing item is treated as already removed, so a re-run never stops early
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => outcome.gone.push(label),
            // the API refuses to delete what something still refers to
            Err(err) if err.status() == Some(StatusCode::CONFLICT) => {
                if let Step::ToolVersion { tool, .. } = step {
                    stuck.insert(tool.clone());
                }
                outcome
                    .refused
                    .push(format!("{label}: {}", utils::delete_refusal(err)));
            }
            Err(err) => {
                if let Step::ToolVersion { tool, .. } = step {
                    stuck.insert(tool.clone());
                }
                progress.warning(format!("Failed to delete {label}: {err}"));
                outcome.failures.push(label);
            }
        }
        progress.inc(1);
    }
    outcome
}

/// Confirm the removal with the user
///
/// # Arguments
///
/// * `conf` - The Thorctl config (used to display the API URL)
fn confirm_remove(conf: &CtlConf) -> Result<bool, Error> {
    // a blank line separates the listing from the prompt, and the default is No
    println!();
    let response = dialoguer::Confirm::new()
        .with_prompt(format!(
            "Delete the resources listed above from '{}'?",
            conf.keys.api.bright_green()
        ))
        .default(false)
        .interact()?;
    Ok(response)
}

/// Remove a toolbox's items from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The toolbox remove command that was run
/// * `workers` - The most reads in flight at once
pub async fn remove(
    thorium: Thorium,
    conf: CtlConf,
    cmd: &RemoveToolbox,
    workers: usize,
) -> Result<(), Error> {
    // read the manifest and what Thorium holds of it
    let manifest = load_manifest_for(
        "toolbox remove",
        &cmd.manifest,
        cmd.group_override.as_deref(),
    )
    .await?;
    let progress = Bar::new("toolbox remove", "Reading from Thorium", BarKind::Timer);
    let live = read_live(&thorium, &manifest, workers).await?;
    let plan = plan_removal(&manifest, &live);
    // nothing deletable and nothing left means the toolbox is already gone
    if plan.deletes_nothing() && plan.kept.is_empty() {
        progress.finish_and_clear();
        println!("Nothing to remove: no resources from this toolbox exist in the instance");
        return Ok(());
    }
    // always list the plan before acting, so non-interactive runs leave a record too
    progress.suspend(|| plan.print());
    // a dry run stops after the listing
    if cmd.dry_run {
        progress.finish_and_clear();
        println!("Dry run: nothing was deleted");
        return Ok(());
    }
    // deleting is irreversible, so confirm exactly what will be removed
    if !cmd.skip_confirm && !plan.deletes_nothing() {
        // dialoguer reads stdin and draws on stderr, so both must be terminals to prompt
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            progress.finish_and_clear();
            return Err(Error::new(
                "No terminal available to confirm this removal; pass --skip-confirm to proceed \
                 non-interactively, or --dry-run to preview it",
            ));
        }
        let confirmed = progress.suspend(|| confirm_remove(&conf))?;
        if !confirmed {
            progress.finish_and_clear();
            println!("Removal cancelled; nothing was deleted");
            return Ok(());
        }
    }
    // delete everything in the plan
    let outcome = execute(&thorium, &plan, &progress).await;
    progress.finish_and_clear();
    println!("Removed {} resource(s)", outcome.removed);
    // anything left or failed means the toolbox wasn't fully removed, so exit non-zero
    let mut problems = Vec::new();
    if !outcome.failures.is_empty() {
        problems.push(format!(
            "failed to delete {} resource(s): {}",
            outcome.failures.len(),
            outcome.failures.join(", ")
        ));
    }
    if !outcome.refused.is_empty() || !plan.kept.is_empty() {
        let mut left: Vec<String> = plan.kept.iter().map(|kept| kept.item.clone()).collect();
        left.extend(outcome.refused.iter().cloned());
        problems.push(format!(
            "left {} resource(s) in place because something outside this toolbox still uses \
             them: {}",
            left.len(),
            left.join("; ")
        ));
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "Toolbox was not fully removed: {}",
        problems.join("; ")
    )))
}
