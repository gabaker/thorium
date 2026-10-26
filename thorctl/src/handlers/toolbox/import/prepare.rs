//! Shaping a loaded manifest before it is planned: the group override, bundled image paths,
//! selecting items, and working out what has to be read from Thorium

use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};

use super::world::Wanted;
use crate::args::toolbox::{ImportOpts, OnlyKind};
use crate::handlers::toolbox::v2::graph::dependencies;
use crate::handlers::toolbox::v2::plan_types::{ItemKind, ItemRef};
use crate::handlers::toolbox::v2::refs::{Ref, SlotValue, find_slots};
use crate::handlers::toolbox::v2::types::{GroupManifest, ToolboxManifestV2};

/// Put every item of a manifest in one group
///
/// # Arguments
///
/// * `manifest` - The manifest to change
/// * `group` - The group everything goes in
pub fn override_group(manifest: &mut ToolboxManifestV2, group: &str) {
    let only = vec![group.to_owned()];
    // every kind of item names its groups
    for tool in manifest.tools.values_mut() {
        tool.groups.clone_from(&only);
        for version in tool.versions.values_mut() {
            for policy in &mut version.network_policies {
                policy.groups.clone_from(&only);
            }
        }
    }
    for toolset in manifest.toolsets.values_mut() {
        toolset.groups.clone_from(&only);
    }
    for agent in manifest.agents.values_mut() {
        agent.groups.clone_from(&only);
    }
    for workflow in manifest.workflows.values_mut() {
        workflow.groups.clone_from(&only);
    }
    // a pipeline is in the group its config names
    for pipeline in manifest.pipelines.values_mut() {
        for version in pipeline.versions.values_mut() {
            if let Some(config) = &mut version.config {
                group.clone_into(&mut config.group);
            }
        }
    }
    for policy in &mut manifest.network_policies {
        policy.groups.clone_from(&only);
    }
    // the group's description is the first one any group had
    let description = manifest
        .groups
        .values()
        .find_map(|group| group.description.clone());
    manifest.groups.clear();
    manifest
        .groups
        .insert(group.to_owned(), GroupManifest { description });
}

/// Get the tag of a container image reference, `latest` when it has none
///
/// # Arguments
///
/// * `image` - The image reference
fn image_tag(image: &str) -> &str {
    // the tag follows the last colon of the last path segment, ignoring any digest
    let last = image.rsplit('/').next().unwrap_or(image);
    let last = last.split_once('@').map_or(last, |(repo, _)| repo);
    match last.rsplit_once(':') {
        Some((_, tag)) if !tag.is_empty() => tag,
        _ => "latest",
    }
}

/// Point the container image of every tool at a registry path, as bundled toolboxes need
///
/// Each image becomes `<prefix>/<group>/<name>:<tag>`. Returns how many images were rewritten.
///
/// # Arguments
///
/// * `manifest` - The manifest to change
/// * `prefix` - The registry base path
pub fn rewrite_images(manifest: &mut ToolboxManifestV2, prefix: &str) -> usize {
    let prefix = prefix.trim_end_matches('/');
    let mut rewritten = 0;
    // the image of a K8s workload sits in the same place for images and harnesses
    for (name, tool) in &mut manifest.tools {
        let group = tool.groups.first().cloned().unwrap_or_default();
        for version in tool.versions.values_mut() {
            let Some(spec) = &mut version.spec else {
                continue;
            };
            for path in [
                "/Image/environment/K8s/image",
                "/Harness/workload/environment/K8s/image",
            ] {
                if let Some(Value::String(image)) = spec.pointer_mut(path)
                    && !image.is_empty()
                {
                    *image = format!("{prefix}/{group}/{name}:{}", image_tag(image));
                    rewritten += 1;
                }
            }
        }
    }
    rewritten
}

/// Get the names of the tools a manifest refers to, defined by it or not
///
/// # Arguments
///
/// * `manifest` - The manifest to read
#[must_use]
pub fn referenced_tools(manifest: &ToolboxManifestV2) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = manifest.tools.keys().cloned().collect();
    // the references in specs, provides, toolsets, agents, and pipeline stages
    for tool in manifest.tools.values() {
        for version in tool.versions.values() {
            if let Some(spec) = &version.spec {
                for slot in find_slots(spec).0 {
                    if let SlotValue::Ref(Ref::Tool { name, .. }) = slot.value {
                        names.insert(name);
                    }
                }
            }
            names.extend(version.provides.iter().map(|tool| tool.tool.clone()));
        }
    }
    for toolset in manifest.toolsets.values() {
        names.extend(toolset.tools.iter().map(|tool| tool.tool.clone()));
    }
    for agent in manifest.agents.values() {
        names.extend(agent.tools.iter().map(|tool| tool.tool.clone()));
        names.extend(agent.harness.iter().map(|tool| tool.tool.clone()));
    }
    for pipeline in manifest.pipelines.values() {
        for version in pipeline.versions.values() {
            names.extend(version.stage_tools.values().map(|stage| stage.tool.clone()));
        }
    }
    names
}

/// Work out what has to be read from Thorium to plan an import of a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest to import
#[must_use]
pub fn wanted(manifest: &ToolboxManifestV2) -> Wanted {
    let mut groups: BTreeSet<String> = manifest.named_groups().into_iter().collect();
    // the policies of the manifest name groups too
    let mut has_policies = !manifest.network_policies.is_empty();
    groups.extend(
        manifest
            .network_policies
            .iter()
            .flat_map(|policy| policy.groups.clone()),
    );
    for version in manifest
        .tools
        .values()
        .flat_map(|tool| tool.versions.values())
    {
        has_policies |= !version.network_policies.is_empty();
        groups.extend(
            version
                .network_policies
                .iter()
                .flat_map(|policy| policy.groups.clone()),
        );
    }
    Wanted {
        groups,
        tools: referenced_tools(manifest),
        entities: !manifest.toolsets.is_empty()
            || !manifest.agents.is_empty()
            || !manifest.workflows.is_empty(),
        pipelines: !manifest.pipelines.is_empty(),
        policies: has_policies,
    }
}

/// Keep only the items an import was asked for, and what they need
///
/// With no selection every item is kept (except pipelines under `--no-pipelines`).
///
/// # Arguments
///
/// * `manifest` - The manifest to cut down
/// * `opts` - The import's settings
pub fn select(manifest: &mut ToolboxManifestV2, opts: &ImportOpts) -> Result<(), String> {
    // pipelines are dropped first, so nothing can select one
    if opts.no_pipelines {
        manifest.pipelines.clear();
    }
    // gather what was asked for
    let mut roots: BTreeSet<ItemRef> = BTreeSet::new();
    let mut missing: Vec<String> = Vec::new();
    for name in &opts.tools {
        if manifest.tools.contains_key(name) {
            roots.insert(ItemRef::new(ItemKind::Tool, name));
        } else {
            missing.push(format!("tool {name}"));
        }
    }
    for name in &opts.pipelines {
        if manifest.pipelines.contains_key(name) {
            roots.insert(ItemRef::new(ItemKind::Pipeline, name));
        } else {
            missing.push(format!("pipeline {name}"));
        }
    }
    for only in &opts.only {
        let (kind, found) = match only.kind {
            OnlyKind::Tool => (ItemKind::Tool, manifest.tools.contains_key(&only.name)),
            OnlyKind::Toolset => (
                ItemKind::Toolset,
                manifest.toolsets.contains_key(&only.name),
            ),
            OnlyKind::Agent => (ItemKind::Agent, manifest.agents.contains_key(&only.name)),
            OnlyKind::Workflow => (
                ItemKind::Workflow,
                manifest.workflows.contains_key(&only.name),
            ),
            OnlyKind::Pipeline => (
                ItemKind::Pipeline,
                manifest.pipelines.contains_key(&only.name),
            ),
        };
        if found {
            roots.insert(ItemRef::new(kind, &only.name));
        } else {
            missing.push(format!("{kind} {}", only.name));
        }
    }
    if !missing.is_empty() {
        return Err(format!("the toolbox has no {}", missing.join(", no ")));
    }
    // with nothing selected everything is kept
    if opts.tools.is_empty() && opts.pipelines.is_empty() && opts.only.is_empty() {
        return Ok(());
    }
    // keep the items asked for and everything they depend on
    let graph = dependencies(manifest);
    let mut keep: BTreeSet<ItemRef> = BTreeSet::new();
    let mut queue: VecDeque<ItemRef> = roots.into_iter().collect();
    while let Some(item) = queue.pop_front() {
        if keep.insert(item.clone())
            && let Some(edges) = graph.get(&item)
        {
            queue.extend(edges.iter().cloned());
        }
    }
    manifest
        .tools
        .retain(|name, _| keep.contains(&ItemRef::new(ItemKind::Tool, name)));
    manifest
        .toolsets
        .retain(|name, _| keep.contains(&ItemRef::new(ItemKind::Toolset, name)));
    manifest
        .agents
        .retain(|name, _| keep.contains(&ItemRef::new(ItemKind::Agent, name)));
    manifest
        .workflows
        .retain(|name, _| keep.contains(&ItemRef::new(ItemKind::Workflow, name)));
    manifest
        .pipelines
        .retain(|name, _| keep.contains(&ItemRef::new(ItemKind::Pipeline, name)));
    // keep the network policies the kept tools name, and the groups still in use
    let specs: String = manifest
        .tools
        .values()
        .flat_map(|tool| tool.versions.values())
        .filter_map(|version| version.spec.as_ref())
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join("\n");
    manifest
        .network_policies
        .retain(|policy| specs.contains(&format!("\"{}\"", policy.name)));
    let mut probe = manifest.clone();
    probe.groups.clear();
    let used: BTreeSet<String> = probe.named_groups().into_iter().collect();
    manifest.groups.retain(|group, _| used.contains(group));
    Ok(())
}
