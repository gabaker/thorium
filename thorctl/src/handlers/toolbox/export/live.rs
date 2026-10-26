//! Reads tools, toolsets, agents, workflows, and pipelines from Thorium for a toolbox command
//!
//! Lists are paged with one request per page of [`PAGE_SIZE`] entities, and the per-tool and
//! per-instance reads run with at most `workers` requests in flight.

use futures::stream::{self, StreamExt, TryStreamExt};
use http::StatusCode;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, NetworkPolicyRequest, Pipeline,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::snapshot::{
    BuildOptions, SnapInstance, SnapTool, Snapshot, build_manifest, spec_policies,
};
use crate::args::toolbox::ManifestLocation;
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::policies::{fetch_existing_in_groups, find_in_groups};
use crate::handlers::toolbox::v2::load::{load_any, resolve_sources};
use crate::handlers::toolbox::v2::plan_types::{GroupState, ItemKind};
use crate::handlers::toolbox::v2::refs::SlotValue;
use crate::handlers::toolbox::v2::refs::{Ref, find_slots};
use crate::handlers::toolbox::v2::types::{ToolRef, ToolboxManifestV2};
use crate::utils::pipelines::list_all_pipelines;

/// The number of entities listed per request
pub(in crate::handlers::toolbox) const PAGE_SIZE: usize = 100;

/// The most times the set of ids a snapshot still needs is read before giving up
const MAX_ROUNDS: usize = 16;

/// Which tool versions an export reads
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::handlers::toolbox) struct VersionFilter {
    /// Also read deprecated versions
    pub include_deprecated: bool,
    /// Only read each tool's latest version
    pub latest_only: bool,
}

/// List the entities of one kind in some groups, with their details
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to list from (every group the caller can see when empty)
/// * `kind` - The kind of entity to list
pub(in crate::handlers::toolbox) async fn list_entities(
    thorium: &Thorium,
    groups: &[String],
    kind: EntityKinds,
) -> Result<Vec<Entity>, Error> {
    // page through the entities of this kind, one request per page
    let opts = EntityListOpts::default()
        .kinds([kind])
        .groups(groups.to_vec())
        .page_size(PAGE_SIZE);
    let mut cursor = thorium.entities.list_details(&opts).await?;
    let mut entities = Vec::new();
    loop {
        // keep this page and stop once every page was read
        entities.append(&mut cursor.data);
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    Ok(entities)
}

/// List the pipelines of some groups
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to list from
pub(in crate::handlers::toolbox) async fn list_pipelines(
    thorium: &Thorium,
    groups: &[String],
) -> Result<Vec<Pipeline>, Error> {
    // list each group's pipelines in turn
    let mut pipelines = Vec::new();
    for group in groups {
        pipelines.extend(list_all_pipelines(thorium, group).await?);
    }
    Ok(pipelines)
}

/// Read entities by id, each with its edges filled in
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `ids` - The ids to read
/// * `workers` - The most reads in flight at once
pub(in crate::handlers::toolbox) async fn read_entities(
    thorium: &Thorium,
    ids: &[Uuid],
    workers: usize,
) -> Result<Vec<Entity>, Error> {
    // read the entities concurrently and keep them in the order of the ids
    stream::iter(ids.iter().copied())
        .map(|id| async move { thorium.entities.get(id).await })
        .buffered(workers.max(1))
        .try_collect()
        .await
}

/// Read a tool's instances as summaries, with no specs
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `entity` - The tool's entity
async fn read_tool(thorium: &Thorium, entity: &Entity) -> Result<Option<SnapTool>, Error> {
    // list the tool's instances, newest first and including deprecated ones
    let summaries = thorium.entities.tool_instances(entity.id).await?;
    let instances = summaries
        .into_iter()
        .map(|summary| SnapInstance {
            id: summary.id,
            label: summary.version.to_string(),
            spec_hash: summary.spec_hash,
            deprecated: summary.deprecated,
            spec: None,
            provides: Vec::new(),
        })
        .collect();
    Ok(SnapTool::from_entity(entity, instances))
}

/// Read tools and their instance summaries
///
/// Entities that aren't tools are skipped.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `entities` - The tool entities
/// * `workers` - The most reads in flight at once
pub(in crate::handlers::toolbox) async fn read_tools(
    thorium: &Thorium,
    entities: &[Entity],
    workers: usize,
) -> Result<Vec<SnapTool>, Error> {
    // read each tool's instances concurrently
    let tools: Vec<Option<SnapTool>> = stream::iter(entities)
        .map(|entity| read_tool(thorium, entity))
        .buffered(workers.max(1))
        .try_collect()
        .await?;
    Ok(tools.into_iter().flatten().collect())
}

/// Pick the instances of a tool whose specs an export reads
///
/// Deprecated instances are left out unless asked for, and only the latest is kept when asked.
/// A tool with nothing left keeps its newest instance so it is still a tool in the manifest.
///
/// # Arguments
///
/// * `tool` - The tool and its instance summaries
/// * `filter` - Which versions to read
pub(in crate::handlers::toolbox) fn wanted_instances(
    tool: &SnapTool,
    filter: VersionFilter,
) -> BTreeSet<Uuid> {
    // keep the instances that pass the filter
    let wanted: BTreeSet<Uuid> = tool
        .instances
        .iter()
        .filter(|instance| filter.include_deprecated || !instance.deprecated)
        .filter(|instance| !filter.latest_only || Some(instance.id) == tool.latest)
        .map(|instance| instance.id)
        .collect();
    // a tool with every version filtered out keeps its newest one
    if wanted.is_empty() {
        return tool
            .instances
            .first()
            .map(|instance| instance.id)
            .into_iter()
            .collect();
    }
    wanted
}

/// Read the specs (and provided tools) of instances
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `tools` - The tools holding the instances
/// * `wanted` - The ids of the instances to read
/// * `workers` - The most reads in flight at once
pub(in crate::handlers::toolbox) async fn load_specs(
    thorium: &Thorium,
    tools: &mut [SnapTool],
    wanted: &BTreeSet<Uuid>,
    workers: usize,
) -> Result<(), Error> {
    // the instances that still need a spec
    let pending: Vec<Uuid> = tools
        .iter()
        .flat_map(|tool| &tool.instances)
        .filter(|instance| instance.spec.is_none() && wanted.contains(&instance.id))
        .map(|instance| instance.id)
        .collect();
    // read them concurrently
    let entities = read_entities(thorium, &pending, workers).await?;
    // put each spec and provides list on its instance
    for entity in entities {
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            continue;
        };
        let spec = serde_json::to_value(&meta.spec).map_err(|err| {
            Error::new(format!("Failed to read the spec of {}: {err}", entity.name))
        })?;
        if let Some(instance) = tools
            .iter_mut()
            .flat_map(|tool| tool.instances.iter_mut())
            .find(|instance| instance.id == entity.id)
        {
            instance.spec = Some(spec);
            instance.provides.clone_from(&meta.provides);
        }
    }
    Ok(())
}

/// Read what a snapshot refers to until it holds all of it
///
/// Tools named by pipelines, toolsets, agents, workflows, and other tools' specs are added with
/// their versions (the ones the filter keeps plus any version something pins), and so are the
/// toolsets and agents the agents and workflows name.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `snapshot` - The snapshot to complete
/// * `filter` - Which versions of an added tool to read
/// * `workers` - The most reads in flight at once
pub(in crate::handlers::toolbox) async fn complete(
    thorium: &Thorium,
    snapshot: &mut Snapshot,
    filter: VersionFilter,
    workers: usize,
) -> Result<(), Error> {
    // keep reading what the snapshot refers to until nothing is missing
    for _ in 0..MAX_ROUNDS {
        let mut needed = snapshot.needed();
        if needed.is_empty() {
            return Ok(());
        }
        // ids of unknown kind are read to find out what they are
        for id in std::mem::take(&mut needed.unknown) {
            match thorium.entities.get(id).await {
                Ok(entity) => match &entity.metadata {
                    EntityMetadata::Tool(_) => {
                        needed.tools.insert(entity.id);
                    }
                    EntityMetadata::ToolInstance(meta) => {
                        needed.tools.insert(meta.tool);
                        needed.instances.insert(entity.id);
                    }
                    EntityMetadata::ToolSet(_) => {
                        needed.toolsets.insert(entity.id);
                    }
                    EntityMetadata::ToolSetInstance(meta) => {
                        needed.toolsets.insert(meta.tool_set);
                    }
                    EntityMetadata::Agent(_) => {
                        needed.agents.insert(entity.id);
                    }
                    _ => {
                        snapshot.ignored.insert(id);
                        snapshot.notes.push(format!(
                            "{id} is a {}, which a toolbox can't hold",
                            entity.kind
                        ));
                    }
                },
                Err(err) => {
                    snapshot.ignored.insert(id);
                    snapshot
                        .notes
                        .push(format!("{id} could not be read: {err}"));
                }
            }
        }
        // read the tools that were named, with the versions the filter keeps
        let ids: Vec<Uuid> = needed.tools.iter().copied().collect();
        let entities = read_entities(thorium, &ids, workers).await?;
        let mut added = read_tools(thorium, &entities, workers).await?;
        let mut wanted = needed.instances.clone();
        for tool in &added {
            wanted.extend(wanted_instances(tool, filter));
        }
        snapshot.tools.append(&mut added);
        // read the specs of those versions and of the versions that are pinned
        load_specs(thorium, &mut snapshot.tools, &wanted, workers).await?;
        // read the toolsets and agents that were named
        let ids: Vec<Uuid> = needed.toolsets.iter().copied().collect();
        snapshot
            .toolsets
            .extend(read_entities(thorium, &ids, workers).await?);
        let ids: Vec<Uuid> = needed.agents.iter().copied().collect();
        snapshot
            .agents
            .extend(read_entities(thorium, &ids, workers).await?);
        // an instance that still has no spec can't be read, so stop asking for it
        for id in &needed.instances {
            let unread = snapshot
                .tools
                .iter()
                .flat_map(|tool| &tool.instances)
                .any(|instance| instance.id == *id && instance.spec.is_none());
            if unread {
                snapshot.ignored.insert(*id);
            }
        }
    }
    Err(Error::new(
        "Gave up following references between tools, toolsets, and agents".to_owned(),
    ))
}

/// Read the network policies the specs of a snapshot name
///
/// The lookup is scoped to the groups the snapshot's items are in: a global get by name is refused
/// when several groups hold distinct policies of one name, as they do once a toolbox is imported
/// into more than one group. A policy is the one in the first of those groups that has it. A
/// policy that can't be read is noted in the snapshot and left out.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `snapshot` - The snapshot whose specs are read
pub(in crate::handlers::toolbox) async fn read_policies(
    thorium: &Thorium,
    snapshot: &mut Snapshot,
) -> Result<(), Error> {
    // every policy a loaded spec names
    let names: BTreeSet<String> = snapshot
        .tools
        .iter()
        .flat_map(|tool| &tool.instances)
        .filter_map(|instance| instance.spec.as_ref())
        .flat_map(|spec: &Value| spec_policies(spec))
        .collect();
    // nothing to read when no spec names a policy
    if names.is_empty() {
        return Ok(());
    }
    // list the policies of each of the items' groups, keyed by group and name
    let groups = snapshot.item_groups();
    let existing = match fetch_existing_in_groups(thorium, &groups).await {
        Ok(existing) => existing,
        Err(err) => {
            snapshot
                .notes
                .push(format!("network policies could not be read: {err}"));
            return Ok(());
        }
    };
    // take each one from the first group that has it, noting the ones no group has
    for name in names {
        match find_in_groups(&existing, &groups, &name) {
            Some(policy) => snapshot.policies.push(NetworkPolicyRequest::from(policy)),
            None => snapshot.notes.push(format!(
                "network policy {name} could not be read: no group of the exported items has it"
            )),
        }
    }
    Ok(())
}

/// Read the description of each group the snapshot's items are in
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to read
pub(in crate::handlers::toolbox) async fn read_group_descriptions(
    thorium: &Thorium,
    groups: &[String],
) -> Vec<(String, Option<String>)> {
    // a group that can't be read is exported without a description
    let mut found = Vec::new();
    for group in groups {
        let description = thorium
            .groups
            .get(group)
            .await
            .ok()
            .and_then(|group| group.description);
        found.push((group.clone(), description));
    }
    found
}

/// A toolset, agent, or workflow in Thorium that a manifest names
#[derive(Debug, Clone)]
pub(in crate::handlers::toolbox) struct LiveEntity {
    /// What kind of item this is
    pub kind: ItemKind,
    /// The entity's id
    pub id: Uuid,
    /// The entity's name
    pub name: String,
    /// The groups the entity is in
    pub groups: Vec<String>,
}

/// What Thorium holds that a toolbox command compares a manifest with
///
/// Tools are read as instance summaries only (their hashes are all a comparison needs), and the
/// toolsets, agents, workflows, and pipelines the manifest names are read in full and turned into
/// manifest items so they compare field by field with the manifest's own.
#[derive(Debug, Clone)]
pub(in crate::handlers::toolbox) struct LiveView {
    /// The tools the manifest names or refers to that exist in its groups, with their instance
    /// summaries
    pub tools: Vec<SnapTool>,
    /// The toolsets, agents, workflows, and pipelines the manifest names that exist in its groups,
    /// as manifest items
    pub live: ToolboxManifestV2,
    /// The toolsets, agents, and workflows the manifest names that exist in its groups
    pub entities: Vec<LiveEntity>,
    /// Every pipeline of the manifest's groups, to find the ones that use a tool
    pub pipelines: Vec<Pipeline>,
    /// The tool each stage of the manifest's pipelines is backed by, by tool or instance id
    pub refs: HashMap<Uuid, ToolRef>,
    /// The network policies the manifest defines, as they are in Thorium (unset when missing)
    pub policies: BTreeMap<String, Option<NetworkPolicyRequest>>,
    /// Whether each group the manifest names exists
    pub groups: BTreeMap<String, GroupState>,
}

impl Default for LiveView {
    /// Build a view of a Thorium that holds nothing
    fn default() -> Self {
        // nothing is held, and the manifest of live items is empty
        LiveView {
            tools: Vec::new(),
            live: ToolboxManifestV2::new("live"),
            entities: Vec::new(),
            pipelines: Vec::new(),
            refs: HashMap::new(),
            policies: BTreeMap::new(),
            groups: BTreeMap::new(),
        }
    }
}

impl LiveView {
    /// Find a live tool by name among the tools in some groups
    ///
    /// With no groups to match, any tool of that name is found.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `groups` - The groups the tool must share one of
    pub(in crate::handlers::toolbox) fn tool(
        &self,
        name: &str,
        groups: &[String],
    ) -> Option<&SnapTool> {
        // the lowest id wins when several tools share the name
        self.tools
            .iter()
            .filter(|tool| tool.name == name)
            .filter(|tool| groups.is_empty() || tool.groups.iter().any(|g| groups.contains(g)))
            .min_by_key(|tool| tool.id)
    }

    /// Get the id a reference resolves to among the live tools
    ///
    /// # Arguments
    ///
    /// * `reference` - The reference to resolve
    /// * `pinned` - Whether the slot takes a tool instance id rather than a tool id
    pub(in crate::handlers::toolbox) fn resolve(
        &self,
        reference: &Ref,
        pinned: bool,
    ) -> Result<Uuid, String> {
        // only tool references stand in specs
        let Ref::Tool { name, label } = reference else {
            return Err(format!("{reference} is not a tool reference"));
        };
        let tool = self
            .tool(name, &[])
            .ok_or_else(|| format!("tool {name} does not exist in Thorium"))?;
        // a tool slot takes the tool, and an instance slot the instance with the label
        match (pinned, label) {
            (false, _) => Ok(tool.id),
            (true, Some(label)) => tool
                .instances
                .iter()
                .find(|instance| instance.label == *label)
                .map(|instance| instance.id)
                .ok_or_else(|| format!("tool {name} has no version {label} in Thorium")),
            (true, None) => Err(format!("{reference} names no version")),
        }
    }

    /// Get the spec hash of a live instance
    ///
    /// # Arguments
    ///
    /// * `id` - The instance's id
    pub(in crate::handlers::toolbox) fn instance_hash(&self, id: Uuid) -> Option<&str> {
        // look through every live tool's instances
        self.tools
            .iter()
            .flat_map(|tool| &tool.instances)
            .find(|instance| instance.id == id)
            .map(|instance| instance.spec_hash.as_str())
    }

    /// Get the spec hash of the instance a tool reference names
    ///
    /// A reference with no version names the tool's latest instance.
    ///
    /// # Arguments
    ///
    /// * `tool` - The reference
    pub(in crate::handlers::toolbox) fn ref_hash(&self, tool: &ToolRef) -> Option<&str> {
        // find the tool, then the pinned instance or the latest one
        let found = self.tool(&tool.tool, &[])?;
        let instance = match &tool.version {
            Some(label) => found
                .instances
                .iter()
                .find(|instance| instance.label == *label),
            None => found
                .instances
                .iter()
                .find(|instance| Some(instance.id) == found.latest),
        };
        instance.map(|instance| instance.spec_hash.as_str())
    }
}

/// Collect the names of the tools a manifest's specs and provided tools refer to
///
/// # Arguments
///
/// * `manifest` - The manifest to read
fn referenced_tools(manifest: &ToolboxManifestV2) -> BTreeSet<String> {
    // the tools the manifest defines, and the ones its specs and harnesses name
    let mut names: BTreeSet<String> = manifest.tools.keys().cloned().collect();
    for tool in manifest.tools.values() {
        for version in tool.versions.values() {
            names.extend(
                version
                    .provides
                    .iter()
                    .map(|provided| provided.tool.clone()),
            );
            if let Some(spec) = &version.spec {
                for slot in find_slots(spec).0 {
                    if let SlotValue::Ref(Ref::Tool { name, .. }) = slot.value {
                        names.insert(name);
                    }
                }
            }
        }
    }
    names
}

/// Get the groups the items of a kind are in, by name
///
/// # Arguments
///
/// * `manifest` - The manifest to read
/// * `kind` - The kind of item
fn item_groups(manifest: &ToolboxManifestV2, kind: ItemKind) -> BTreeMap<String, Vec<String>> {
    // each kind keeps its groups in its own place
    match kind {
        ItemKind::Tool => manifest
            .tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.groups.clone()))
            .collect(),
        ItemKind::Toolset => manifest
            .toolsets
            .iter()
            .map(|(name, item)| (name.clone(), item.groups.clone()))
            .collect(),
        ItemKind::Agent => manifest
            .agents
            .iter()
            .map(|(name, item)| (name.clone(), item.groups.clone()))
            .collect(),
        ItemKind::Workflow => manifest
            .workflows
            .iter()
            .map(|(name, item)| (name.clone(), item.groups.clone()))
            .collect(),
        ItemKind::Pipeline => manifest
            .pipelines
            .iter()
            .map(|(name, item)| {
                let groups = item
                    .versions
                    .values()
                    .filter_map(|version| version.config.as_ref())
                    .map(|config| config.group.clone())
                    .collect();
                (name.clone(), groups)
            })
            .collect(),
    }
}

/// Read the entities of one kind that a manifest names, in the groups the manifest puts them in
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `manifest` - The manifest
/// * `kind` - The kind of item
/// * `groups` - Every group the manifest names
/// * `workers` - The most reads in flight at once
async fn read_named(
    thorium: &Thorium,
    manifest: &ToolboxManifestV2,
    kind: ItemKind,
    groups: &[String],
    workers: usize,
) -> Result<Vec<Entity>, Error> {
    // nothing is listed for a kind the manifest has none of
    let wanted = item_groups(manifest, kind);
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let entity_kind = match kind {
        ItemKind::Toolset => EntityKinds::ToolSet,
        ItemKind::Agent => EntityKinds::Agent,
        ItemKind::Workflow => EntityKinds::WorkflowCatalog,
        _ => EntityKinds::Tool,
    };
    // keep the entities of the right name that share a group with the item
    let ids: Vec<Uuid> = list_entities(thorium, groups, entity_kind)
        .await?
        .into_iter()
        .filter(|entity| {
            wanted.get(&entity.name).is_some_and(|item_groups| {
                item_groups.is_empty() || entity.groups.iter().any(|g| item_groups.contains(g))
            })
        })
        .map(|entity| entity.id)
        .collect();
    // read them whole so their edges are filled in
    read_entities(thorium, &ids, workers).await
}

/// Resolve the tool behind a pipeline stage id without reading the whole tool
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `id` - The tool or tool instance id
async fn stage_ref(thorium: &Thorium, id: Uuid) -> Option<ToolRef> {
    // the entity's name is the tool, or `TOOL@LABEL` for an instance
    let entity = thorium.entities.get(id).await.ok()?;
    match &entity.metadata {
        EntityMetadata::Tool(_) => Some(ToolRef {
            tool: entity.name,
            version: None,
        }),
        EntityMetadata::ToolInstance(meta) => {
            let (tool, _) = entity.name.split_once('@')?;
            Some(ToolRef {
                tool: tool.to_owned(),
                version: Some(meta.version.to_string()),
            })
        }
        _ => None,
    }
}

/// Read what Thorium holds that a manifest is compared with
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `manifest` - The manifest to compare with
/// * `workers` - The most reads in flight at once
pub(in crate::handlers::toolbox) async fn read_live(
    thorium: &Thorium,
    manifest: &ToolboxManifestV2,
    workers: usize,
) -> Result<LiveView, Error> {
    let groups = manifest.named_groups();
    let mut view = LiveView::default();
    // the tools the manifest names or refers to, as instance summaries
    let names = referenced_tools(manifest);
    if !names.is_empty() {
        let entities: Vec<Entity> = list_entities(thorium, &groups, EntityKinds::Tool)
            .await?
            .into_iter()
            .filter(|entity| names.contains(&entity.name))
            .collect();
        view.tools = read_tools(thorium, &entities, workers).await?;
    }
    // the toolsets, agents, and workflows the manifest names
    let mut snapshot = Snapshot {
        tools: view.tools.clone(),
        ..Snapshot::default()
    };
    snapshot.toolsets = read_named(thorium, manifest, ItemKind::Toolset, &groups, workers).await?;
    snapshot.agents = read_named(thorium, manifest, ItemKind::Agent, &groups, workers).await?;
    snapshot.workflows =
        read_named(thorium, manifest, ItemKind::Workflow, &groups, workers).await?;
    // remember the id of each so a command can act on them
    for (kind, found) in [
        (ItemKind::Toolset, &snapshot.toolsets),
        (ItemKind::Agent, &snapshot.agents),
        (ItemKind::Workflow, &snapshot.workflows),
    ] {
        view.entities.extend(found.iter().map(|entity| LiveEntity {
            kind,
            id: entity.id,
            name: entity.name.clone(),
            groups: entity.groups.clone(),
        }));
    }
    // every pipeline of the groups, and the tool behind each stage of the ones the manifest names
    view.pipelines = list_pipelines(thorium, &groups).await?;
    let wanted = item_groups(manifest, ItemKind::Pipeline);
    snapshot.pipelines = view
        .pipelines
        .iter()
        .filter(|pipeline| {
            wanted.get(&pipeline.name).is_some_and(|item_groups| {
                item_groups.is_empty() || item_groups.contains(&pipeline.group)
            })
        })
        .cloned()
        .collect();
    for pipeline in &snapshot.pipelines {
        for id in pipeline.stage_tools.values() {
            let known = view.tools.iter().any(|tool| {
                tool.id == *id || tool.instances.iter().any(|instance| instance.id == *id)
            });
            if !known
                && !view.refs.contains_key(id)
                && let Some(found) = stage_ref(thorium, *id).await
            {
                view.refs.insert(*id, found);
            }
        }
    }
    snapshot.refs.clone_from(&view.refs);
    // turn them into manifest items to compare with the manifest's own
    view.live = build_manifest("live", &snapshot, &BuildOptions::default())?.manifest;
    Ok(view)
}

/// Read the network policies a manifest defines, as they are in Thorium
///
/// The lookup is scoped to the manifest's groups: a global get by name is refused when several
/// groups hold distinct policies of one name, as they do once a toolbox is imported into more than
/// one group. A policy a manifest names is the one in the first of its groups that has it.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `names` - The names of the policies
/// * `groups` - The groups the manifest's items are in
pub(in crate::handlers::toolbox) async fn read_policy_states(
    thorium: &Thorium,
    names: &[String],
    groups: &[String],
) -> Result<BTreeMap<String, Option<NetworkPolicyRequest>>, Error> {
    // list the policies of each of the manifest's groups, keyed by group and name
    let existing = fetch_existing_in_groups(thorium, groups).await?;
    // a policy no group has is unset
    let mut found = BTreeMap::new();
    for name in names {
        let policy = find_in_groups(&existing, groups, name);
        found.insert(name.clone(), policy.map(NetworkPolicyRequest::from));
    }
    Ok(found)
}

/// Read whether each group exists and can be reached
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to check
pub(in crate::handlers::toolbox) async fn read_group_states(
    thorium: &Thorium,
    groups: &[String],
) -> Result<BTreeMap<String, GroupState>, Error> {
    // a missing group is missing and a group the caller can't see is forbidden
    let mut found = BTreeMap::new();
    for group in groups {
        let state = match thorium.groups.get(group).await {
            Ok(_) => GroupState::Exists,
            Err(err) if err.status() == Some(StatusCode::NOT_FOUND) => GroupState::Missing,
            Err(err) if err.status() == Some(StatusCode::UNAUTHORIZED) => GroupState::Forbidden,
            Err(err) => return Err(err),
        };
        found.insert(group.clone(), state);
    }
    Ok(found)
}

/// Replace the groups of every item of a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest to change
/// * `group` - The group everything goes in
pub(in crate::handlers::toolbox) fn override_groups(manifest: &mut ToolboxManifestV2, group: &str) {
    // every kind of item has its own groups, and a pipeline its config's
    let groups = vec![group.to_owned()];
    for tool in manifest.tools.values_mut() {
        tool.groups.clone_from(&groups);
    }
    for toolset in manifest.toolsets.values_mut() {
        toolset.groups.clone_from(&groups);
    }
    for agent in manifest.agents.values_mut() {
        agent.groups.clone_from(&groups);
    }
    for workflow in manifest.workflows.values_mut() {
        workflow.groups.clone_from(&groups);
    }
    for version in manifest
        .pipelines
        .values_mut()
        .flat_map(|pipeline| pipeline.versions.values_mut())
    {
        if let Some(config) = &mut version.config {
            group.clone_into(&mut config.group);
        }
    }
    manifest.groups.retain(|name, _| name == group);
}

/// Read a toolbox manifest and fill in the specs and configs it points at
///
/// # Arguments
///
/// * `command` - The name of the command, for the progress bar
/// * `location` - Where the manifest is
/// * `group_override` - Put every item in this group instead of its own
pub(in crate::handlers::toolbox) async fn load_manifest_for(
    command: &str,
    location: &ManifestLocation,
    group_override: Option<&str>,
) -> Result<ToolboxManifestV2, Error> {
    // read the manifest without drawing a progress bar, since the report goes to stdout
    let bar = Bar::new_or_quiet(command, "Reading manifest...", BarKind::UnboundIO, true);
    let mut loaded = load_any(location, &bar).await?;
    // a spec or config that can't be read can't be used
    let failures = resolve_sources(&mut loaded.manifest, loaded.base_dir.as_deref()).await;
    if !failures.is_empty() {
        let listed: Vec<String> = failures
            .iter()
            .map(|failure| format!("{} {}: {}", failure.item, failure.field, failure.message))
            .collect();
        return Err(Error::new(format!(
            "Failed to read the toolbox:\n{}",
            listed.join("\n")
        )));
    }
    let mut manifest = loaded.manifest;
    // use the group the toolbox was imported into, if it was overridden
    if let Some(group) = group_override {
        override_groups(&mut manifest, group);
    }
    Ok(manifest)
}
