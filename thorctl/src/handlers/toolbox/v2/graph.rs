//! The dependency graph between the items of a toolbox manifest
//!
//! An item depends on the items of the same manifest it names: a tool on the tools its spec
//! refers to and (for a harness) on the tools it provides, a toolset on its tools, an agent on its
//! tools, toolsets, harness, and sub-agents, a workflow on its agents, and a pipeline on its
//! stage tools. Names that aren't in the manifest are not edges (they may exist in Thorium).

use std::collections::{BTreeMap, BTreeSet};

use super::plan_types::{ItemKind, ItemRef};
use super::refs::{Ref, SlotValue, find_slots};
use super::types::ToolboxManifestV2;

/// The items each item of a manifest depends on
pub type Dependencies = BTreeMap<ItemRef, BTreeSet<ItemRef>>;

/// Add an edge to a tool when the manifest holds it
///
/// # Arguments
///
/// * `manifest` - The manifest the edge must land in
/// * `edges` - The edges of the item being built
/// * `name` - The name of the tool
fn add_tool(manifest: &ToolboxManifestV2, edges: &mut BTreeSet<ItemRef>, name: &str) {
    // only tools the manifest defines are edges
    if manifest.tools.contains_key(name) {
        edges.insert(ItemRef::new(ItemKind::Tool, name));
    }
}

/// Build the dependency graph of a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest to read
#[must_use]
pub fn dependencies(manifest: &ToolboxManifestV2) -> Dependencies {
    let mut graph = Dependencies::new();
    // tools depend on the tools their specs refer to and, for a harness, the tools it provides
    for (name, tool) in &manifest.tools {
        let mut edges = BTreeSet::new();
        for version in tool.versions.values() {
            if let Some(spec) = &version.spec {
                for slot in find_slots(spec).0 {
                    if let SlotValue::Ref(Ref::Tool { name, .. }) = slot.value {
                        add_tool(manifest, &mut edges, &name);
                    }
                }
            }
            for provided in &version.provides {
                add_tool(manifest, &mut edges, &provided.tool);
            }
        }
        graph.insert(ItemRef::new(ItemKind::Tool, name), edges);
    }
    // toolsets depend on their tools
    for (name, toolset) in &manifest.toolsets {
        let mut edges = BTreeSet::new();
        for tool in &toolset.tools {
            add_tool(manifest, &mut edges, &tool.tool);
        }
        graph.insert(ItemRef::new(ItemKind::Toolset, name), edges);
    }
    // agents depend on their harness, tools, toolsets, and sub-agents
    for (name, agent) in &manifest.agents {
        let mut edges = BTreeSet::new();
        for tool in agent.tools.iter().chain(agent.harness.iter()) {
            add_tool(manifest, &mut edges, &tool.tool);
        }
        for toolset in &agent.toolsets {
            if manifest.toolsets.contains_key(&toolset.toolset) {
                edges.insert(ItemRef::new(ItemKind::Toolset, &toolset.toolset));
            }
        }
        for sub_agent in &agent.sub_agents {
            if manifest.agents.contains_key(sub_agent) {
                edges.insert(ItemRef::new(ItemKind::Agent, sub_agent));
            }
        }
        graph.insert(ItemRef::new(ItemKind::Agent, name), edges);
    }
    // workflows depend on their agents
    for (name, workflow) in &manifest.workflows {
        let edges = workflow
            .agents
            .iter()
            .filter(|agent| manifest.agents.contains_key(*agent))
            .map(|agent| ItemRef::new(ItemKind::Agent, agent))
            .collect();
        graph.insert(ItemRef::new(ItemKind::Workflow, name), edges);
    }
    // pipelines depend on the tools behind their stages
    for (name, pipeline) in &manifest.pipelines {
        let mut edges = BTreeSet::new();
        for version in pipeline.versions.values() {
            for stage in version.stage_tools.values() {
                add_tool(manifest, &mut edges, &stage.tool);
            }
        }
        graph.insert(ItemRef::new(ItemKind::Pipeline, name), edges);
    }
    graph
}

/// Order the items of a graph into waves, where every item of a wave depends only on items of
/// earlier waves
///
/// Returns the items left over, which sit on or behind a cycle, when the graph has one.
///
/// # Arguments
///
/// * `graph` - The dependency graph
pub fn waves_of(graph: &Dependencies) -> Result<Vec<Vec<ItemRef>>, BTreeSet<ItemRef>> {
    let mut remaining: BTreeSet<ItemRef> = graph.keys().cloned().collect();
    let mut done: BTreeSet<ItemRef> = BTreeSet::new();
    let mut waves = Vec::new();
    // peel off every item whose dependencies are all done
    while !remaining.is_empty() {
        let wave: Vec<ItemRef> = remaining
            .iter()
            .filter(|item| graph[*item].iter().all(|dep| done.contains(dep)))
            .cloned()
            .collect();
        // no progress means the rest depends on a cycle
        if wave.is_empty() {
            return Err(remaining);
        }
        for item in &wave {
            remaining.remove(item);
            done.insert(item.clone());
        }
        waves.push(wave);
    }
    Ok(waves)
}

/// Order the items of a manifest into waves a write can be applied in
///
/// Within a wave the order is by kind then name. Returns the items on or behind a cycle when the
/// manifest has one.
///
/// # Arguments
///
/// * `manifest` - The manifest to order
pub fn waves(manifest: &ToolboxManifestV2) -> Result<Vec<Vec<ItemRef>>, BTreeSet<ItemRef>> {
    // order the manifest's own graph
    waves_of(&dependencies(manifest))
}

/// Find the cycles in a graph, each as the items on it in order, starting from its smallest item
///
/// # Arguments
///
/// * `graph` - The dependency graph
#[must_use]
pub fn cycles(graph: &Dependencies) -> Vec<Vec<ItemRef>> {
    // only the leftovers of the wave ordering can be on a cycle
    let Err(stuck) = waves_of(graph) else {
        return Vec::new();
    };
    let mut found: BTreeSet<Vec<ItemRef>> = BTreeSet::new();
    // from each stuck item, follow dependencies inside the stuck set until an item repeats
    for start in &stuck {
        let mut path: Vec<ItemRef> = Vec::new();
        let mut current = start.clone();
        while !path.contains(&current) {
            path.push(current.clone());
            // every stuck item has at least one dependency that is also stuck
            match graph[&current].iter().find(|dep| stuck.contains(*dep)) {
                Some(next) => current = next.clone(),
                None => break,
            }
        }
        // the cycle is the path from the repeated item onward
        if let Some(position) = path.iter().position(|item| *item == current) {
            let mut cycle = path.split_off(position);
            // rotate to the smallest item so each cycle has one spelling
            let smallest = cycle
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.cmp(b.1))
                .map_or(0, |(index, _)| index);
            cycle.rotate_left(smallest);
            found.insert(cycle);
        }
    }
    found.into_iter().collect()
}
