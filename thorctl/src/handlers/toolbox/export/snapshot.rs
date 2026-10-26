//! Turns the tools, toolsets, agents, workflows, and pipelines read from Thorium into a toolbox
//! v2 manifest
//!
//! Everything here is pure: the live reads happen in [`super::live`], and the same conversion
//! builds the "live side" of `toolbox diff` and `toolbox remove`, so what an export writes and
//! what a diff compares against can't drift apart.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thorium::Error;
use thorium::models::{
    Entity, EntityMetadata, ModelAuthLevel, NetworkPolicyRequest, Pipeline, ToolKind,
};
use uuid::Uuid;

use crate::handlers::toolbox::v2::refs::{Ref, SlotKind, SlotValue, find_slots, unresolve_slots};
use crate::handlers::toolbox::v2::types::{
    AgentManifest, GroupManifest, PipelineConfig, PipelineManifestV2, PipelineVersionV2, StageTool,
    ToolManifest, ToolRef, ToolSetManifest, ToolSetRef, ToolVersionManifest, ToolboxManifestV2,
    WorkflowManifest, check_name,
};

/// The label an exported pipeline's single version has
pub(in crate::handlers::toolbox) const PIPELINE_LABEL: &str = "latest";

/// One version of a tool as read from Thorium
#[derive(Debug, Clone)]
pub(in crate::handlers::toolbox) struct SnapInstance {
    /// The id of the tool instance
    pub id: Uuid,
    /// The version label
    pub label: String,
    /// The content hash of the instance's spec
    pub spec_hash: String,
    /// Whether the instance is deprecated
    pub deprecated: bool,
    /// The spec as JSON, once it has been read (a summary alone leaves it unset)
    pub spec: Option<Value>,
    /// The ids of the native tool instances a harness instance provides
    pub provides: Vec<Uuid>,
}

/// A tool as read from Thorium
#[derive(Debug, Clone)]
pub(in crate::handlers::toolbox) struct SnapTool {
    /// The id of the tool
    pub id: Uuid,
    /// The name of the tool
    pub name: String,
    /// The groups the tool is in
    pub groups: Vec<String>,
    /// The description of the tool
    pub description: Option<String>,
    /// The user tags on the tool root, by key
    pub tags: BTreeMap<String, BTreeSet<String>>,
    /// The kind of tool
    pub kind: ToolKind,
    /// The id of the tool's latest instance
    pub latest: Option<Uuid>,
    /// The harness env values the tool exports, in the text `tools harness-env --set` takes
    pub harness_env: Vec<String>,
    /// The model ids no run of this harness may resolve to
    pub disabled_models: Vec<String>,
    /// The tool's instances (newest first, as the API lists them)
    pub instances: Vec<SnapInstance>,
}

impl SnapTool {
    /// Build a tool from its entity and its instances
    ///
    /// Returns `None` when the entity isn't a tool.
    ///
    /// # Arguments
    ///
    /// * `entity` - The tool's entity
    /// * `instances` - The tool's instances
    pub(in crate::handlers::toolbox) fn from_entity(
        entity: &Entity,
        instances: Vec<SnapInstance>,
    ) -> Option<Self> {
        // only tool entities carry a tool root
        let EntityMetadata::Tool(meta) = &entity.metadata else {
            return None;
        };
        // the harness env is written as the values to export, renamed where the env renames them
        let harness_env = meta.harness_env.as_ref().map_or_else(Vec::new, |env| {
            env.export
                .iter()
                .filter_map(|var| {
                    let name = serde_json::to_value(var).ok()?.as_str()?.to_owned();
                    Some(match env.names.get(var) {
                        Some(rename) => format!("{name}={rename}"),
                        None => name,
                    })
                })
                .collect()
        });
        Some(SnapTool {
            id: entity.id,
            name: entity.name.clone(),
            groups: entity.groups.clone(),
            description: entity.description.clone(),
            tags: flatten_tags(entity),
            kind: meta.kind,
            latest: meta.latest,
            harness_env,
            disabled_models: meta.disabled_models.iter().cloned().collect(),
            instances,
        })
    }

    /// Get the label of this tool's latest instance
    pub(in crate::handlers::toolbox) fn latest_label(&self) -> Option<&str> {
        // find the latest instance among the instances
        let latest = self.latest?;
        self.instances
            .iter()
            .find(|instance| instance.id == latest)
            .map(|instance| instance.label.as_str())
    }
}

/// Flatten an entity's tags into the values under each key (the groups they are visible to are
/// not carried)
///
/// # Arguments
///
/// * `entity` - The entity to read the tags of
fn flatten_tags(entity: &Entity) -> BTreeMap<String, BTreeSet<String>> {
    // keep each key's values, sorted
    entity
        .tags
        .iter()
        .map(|(key, values)| (key.clone(), values.keys().cloned().collect()))
        .collect()
}

/// Everything read from Thorium that a manifest is built from
#[derive(Debug, Clone, Default)]
pub(in crate::handlers::toolbox) struct Snapshot {
    /// The tools and their instances
    pub tools: Vec<SnapTool>,
    /// The toolset entities
    pub toolsets: Vec<Entity>,
    /// The agent entities
    pub agents: Vec<Entity>,
    /// The workflow entities
    pub workflows: Vec<Entity>,
    /// The pipelines
    pub pipelines: Vec<Pipeline>,
    /// The network policies the tool specs name
    pub policies: Vec<NetworkPolicyRequest>,
    /// The groups the items are in, with their descriptions
    pub groups: BTreeMap<String, GroupManifest>,
    /// What was left out and why, for the report
    pub notes: Vec<String>,
    /// Ids that can't be read or can't be exported, so they are not asked for again
    pub ignored: BTreeSet<Uuid>,
    /// Tools and tool instances named without reading the whole tool, by id
    pub refs: HashMap<Uuid, ToolRef>,
}

/// The ids a snapshot refers to that it doesn't hold yet
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::handlers::toolbox) struct Needed {
    /// Ids of unknown kind (a pipeline stage's tool, or an id found in a spec)
    pub unknown: BTreeSet<Uuid>,
    /// Ids of tools to read
    pub tools: BTreeSet<Uuid>,
    /// Ids of tool instances whose specs are to be read
    pub instances: BTreeSet<Uuid>,
    /// Ids of toolsets to read
    pub toolsets: BTreeSet<Uuid>,
    /// Ids of agents to read
    pub agents: BTreeSet<Uuid>,
}

impl Needed {
    /// Whether nothing more is needed
    pub(in crate::handlers::toolbox) fn is_empty(&self) -> bool {
        // every set must be empty
        self.unknown.is_empty()
            && self.tools.is_empty()
            && self.instances.is_empty()
            && self.toolsets.is_empty()
            && self.agents.is_empty()
    }
}

/// How a manifest is built from a snapshot
#[derive(Debug, Clone, Default)]
pub(in crate::handlers::toolbox) struct BuildOptions {
    /// Write every item into this group instead of its own
    pub group_override: Option<String>,
    /// Clear the container image url of image tools
    pub strip_registry: bool,
}

/// A manifest built from a snapshot
#[derive(Debug, Clone)]
pub(in crate::handlers::toolbox) struct Built {
    /// The manifest
    pub manifest: ToolboxManifestV2,
    /// Things worth telling the user
    pub warnings: Vec<String>,
}

/// Lookups from the ids Thorium uses to the names a manifest uses
struct Index {
    /// Tool names by tool id
    tools: HashMap<Uuid, String>,
    /// The tool name and version label of each tool instance, by instance id
    instances: HashMap<Uuid, (String, String)>,
    /// Toolset names by toolset id
    toolsets: HashMap<Uuid, String>,
    /// Agent names by agent id
    agents: HashMap<Uuid, String>,
    /// Tools and instances named without reading the whole tool, by id
    refs: HashMap<Uuid, ToolRef>,
}

impl Index {
    /// Index what a snapshot holds
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The snapshot to index
    fn new(snapshot: &Snapshot) -> Self {
        // index tools and their instances
        let mut tools = HashMap::new();
        let mut instances = HashMap::new();
        for tool in &snapshot.tools {
            tools.insert(tool.id, tool.name.clone());
            for instance in &tool.instances {
                instances.insert(instance.id, (tool.name.clone(), instance.label.clone()));
            }
        }
        // index toolsets and agents by id
        let toolsets = snapshot
            .toolsets
            .iter()
            .map(|entity| (entity.id, entity.name.clone()))
            .collect();
        let agents = snapshot
            .agents
            .iter()
            .map(|entity| (entity.id, entity.name.clone()))
            .collect();
        Index {
            tools,
            instances,
            toolsets,
            agents,
            refs: snapshot.refs.clone(),
        }
    }

    /// Get the reference a tool or tool instance id stands for
    ///
    /// # Arguments
    ///
    /// * `id` - The id of a tool or a tool instance
    fn tool_ref(&self, id: Uuid) -> Option<ToolRef> {
        // a tool id follows the tool's latest, an instance id pins its version
        if let Some(name) = self.tools.get(&id) {
            return Some(ToolRef {
                tool: name.clone(),
                version: None,
            });
        }
        if let Some((tool, label)) = self.instances.get(&id) {
            return Some(ToolRef {
                tool: tool.clone(),
                version: Some(label.clone()),
            });
        }
        // otherwise it may be one named without reading its tool
        self.refs.get(&id).cloned()
    }
}

/// Get the reference a tool or tool instance entity (an edge target) stands for
///
/// An instance entity is named `TOOL@LABEL`, and tool names hold no `@`.
///
/// # Arguments
///
/// * `entity` - The edge's target entity
fn entity_tool_ref(entity: &Entity) -> Option<ToolRef> {
    // a tool follows its latest and an instance pins the label in its name
    match &entity.metadata {
        EntityMetadata::Tool(_) => Some(ToolRef {
            tool: entity.name.clone(),
            version: None,
        }),
        EntityMetadata::ToolInstance(instance) => {
            let (tool, _) = entity.name.split_once('@')?;
            Some(ToolRef {
                tool: tool.to_owned(),
                version: Some(instance.version.to_string()),
            })
        }
        _ => None,
    }
}

/// Collect the tool and tool instance an edge target entity needs read
///
/// # Arguments
///
/// * `entity` - The edge's target entity
/// * `needed` - The ids needed so far
fn need_edge_tool(entity: &Entity, needed: &mut Needed) {
    // a tool is needed whole, an instance needs its spec and its tool
    match &entity.metadata {
        EntityMetadata::Tool(_) => {
            needed.tools.insert(entity.id);
        }
        EntityMetadata::ToolInstance(instance) => {
            needed.instances.insert(entity.id);
            needed.tools.insert(instance.tool);
        }
        _ => {}
    }
}

impl Snapshot {
    /// Find the groups the snapshot's items are in
    ///
    /// The groups scope the lookups of what the items use, such as their network policies, and
    /// are the groups whose descriptions an export records.
    pub(in crate::handlers::toolbox) fn item_groups(&self) -> Vec<String> {
        // collect the groups of every kind of item once, in name order
        let mut groups = BTreeSet::new();
        for tool in &self.tools {
            groups.extend(tool.groups.iter().cloned());
        }
        for entity in self
            .toolsets
            .iter()
            .chain(&self.agents)
            .chain(&self.workflows)
        {
            groups.extend(entity.groups.iter().cloned());
        }
        groups.extend(self.pipelines.iter().map(|pipeline| pipeline.group.clone()));
        groups.into_iter().collect()
    }

    /// Find the ids this snapshot's items refer to that it doesn't hold yet
    ///
    /// Read what this returns, add it to the snapshot, and ask again until it returns nothing.
    pub(in crate::handlers::toolbox) fn needed(&self) -> Needed {
        let index = Index::new(self);
        let mut needed = Needed::default();
        // pipelines name a tool or a tool instance for every stage
        for pipeline in &self.pipelines {
            for id in pipeline.stage_tools.values() {
                if index.tool_ref(*id).is_none() {
                    needed.unknown.insert(*id);
                }
            }
        }
        // toolsets name tools
        for toolset in &self.toolsets {
            if let EntityMetadata::ToolSet(meta) = &toolset.metadata {
                for tool in &meta.tools {
                    need_edge_tool(tool, &mut needed);
                }
            }
        }
        // agents name a harness, tools, toolsets, and sub-agents
        for agent in &self.agents {
            if let EntityMetadata::Agent(meta) = &agent.metadata {
                for tool in meta
                    .harness
                    .iter()
                    .map(AsRef::as_ref)
                    .chain(&meta.latest_tools)
                    .chain(&meta.pinned_tools)
                {
                    need_edge_tool(tool, &mut needed);
                }
                for toolset in &meta.tool_sets {
                    match &toolset.metadata {
                        EntityMetadata::ToolSetInstance(instance) => {
                            needed.toolsets.insert(instance.tool_set);
                        }
                        _ => {
                            needed.toolsets.insert(toolset.id);
                        }
                    }
                }
                needed
                    .agents
                    .extend(meta.sub_agents.iter().map(|agent| agent.id));
            }
        }
        // workflows name agents
        for workflow in &self.workflows {
            if let EntityMetadata::WorkflowCatalog(meta) = &workflow.metadata {
                needed
                    .agents
                    .extend(meta.agents.iter().map(|agent| agent.id));
            }
        }
        // tool instances name tools and instances in their specs and provides
        for tool in &self.tools {
            for instance in &tool.instances {
                for id in &instance.provides {
                    needed.instances.insert(*id);
                }
                let Some(spec) = &instance.spec else {
                    continue;
                };
                for slot in find_slots(spec).0 {
                    if let SlotValue::Uuid(id) = slot.value {
                        match slot.kind {
                            SlotKind::ToolId => needed.tools.insert(id),
                            SlotKind::ToolInstanceId => needed.instances.insert(id),
                        };
                    }
                }
            }
        }
        // what is already held or can't be read is not needed again
        needed.tools.retain(|id| !index.tools.contains_key(id));
        needed
            .toolsets
            .retain(|id| !index.toolsets.contains_key(id));
        needed.agents.retain(|id| !index.agents.contains_key(id));
        // an instance is needed when its spec isn't read; one the snapshot doesn't know is unknown
        let instances = std::mem::take(&mut needed.instances);
        for id in instances {
            let known = self
                .tools
                .iter()
                .flat_map(|tool| &tool.instances)
                .find(|instance| instance.id == id);
            match known {
                Some(instance) if instance.spec.is_none() => {
                    needed.instances.insert(id);
                }
                Some(_) => {}
                None => {
                    needed.unknown.insert(id);
                }
            }
        }
        needed.unknown.retain(|id| !self.ignored.contains(id));
        needed.instances.retain(|id| !self.ignored.contains(id));
        needed
    }
}

/// Sort and deduplicate a list of tool references
///
/// # Arguments
///
/// * `tools` - The references to order
fn sorted_refs(mut tools: Vec<ToolRef>) -> Vec<ToolRef> {
    // order by tool then version so equal sets compare equal
    tools.sort_by(|a, b| (&a.tool, &a.version).cmp(&(&b.tool, &b.version)));
    tools.dedup();
    tools
}

/// Get the model auth levels as a sorted list, where no levels means every level
///
/// # Arguments
///
/// * `levels` - The levels the entity allows
fn auth_levels<'a>(levels: impl Iterator<Item = &'a ModelAuthLevel>) -> Vec<ModelAuthLevel> {
    // a set keeps them in precedence order
    levels
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Rewrite the ids in a tool spec as references and strip the registry when asked
///
/// # Arguments
///
/// * `spec` - The spec as read from Thorium
/// * `index` - The names of everything the manifest holds
/// * `strip_registry` - Whether to clear image urls
fn export_spec(spec: &Value, index: &Index, strip_registry: bool) -> Value {
    // turn tool ids into `ref:tool/NAME` and instance ids into `ref:tool/NAME@LABEL`
    let mut spec = spec.clone();
    unresolve_slots(&mut spec, |id, kind| match kind {
        SlotKind::ToolId => index.tools.get(&id).map(|name| Ref::Tool {
            name: name.clone(),
            label: None,
        }),
        SlotKind::ToolInstanceId => index.instances.get(&id).map(|(name, label)| Ref::Tool {
            name: name.clone(),
            label: Some(label.clone()),
        }),
    });
    // an image tool can be written with no image url so it is built from the toolbox's registry
    if strip_registry {
        for pointer in [
            "/Image/environment/K8s/image",
            "/Harness/workload/environment/K8s/image",
        ] {
            if let Some(image) = spec.pointer_mut(pointer) {
                *image = Value::String(String::new());
            }
        }
    }
    spec
}

/// Find the network policy names a tool spec's environments name
///
/// # Arguments
///
/// * `spec` - The spec as JSON
pub(in crate::handlers::toolbox) fn spec_policies(spec: &Value) -> BTreeSet<String> {
    // the policies are a list of names in each kubernetes environment
    let mut names = BTreeSet::new();
    for pointer in [
        "/Image/environment/K8s/network_policies",
        "/Harness/workload/environment/K8s/network_policies",
    ] {
        if let Some(Value::Array(list)) = spec.pointer(pointer) {
            names.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    names
}

/// Build a tool and its versions
///
/// # Arguments
///
/// * `tool` - The tool as read from Thorium
/// * `index` - The names of everything the manifest holds
/// * `groups` - The groups the exported tool is in
/// * `options` - How the manifest is built
/// * `warnings` - Where to add what is worth telling the user
fn build_tool(
    tool: &SnapTool,
    index: &Index,
    groups: Vec<String>,
    options: &BuildOptions,
    warnings: &mut Vec<String>,
) -> ToolManifest {
    // each loaded instance is one version
    let mut versions = BTreeMap::new();
    for instance in &tool.instances {
        let Some(spec) = &instance.spec else {
            continue;
        };
        // the native tools a harness provides become references
        let mut provides = Vec::new();
        for id in &instance.provides {
            match index.instances.get(id) {
                Some((name, label)) => provides.push(ToolRef {
                    tool: name.clone(),
                    version: Some(label.clone()),
                }),
                None => warnings.push(format!(
                    "tool {}@{}: provides the tool instance {id}, which was not exported",
                    tool.name, instance.label
                )),
            }
        }
        // ids in the spec that couldn't be named are left in place, which ties the manifest to
        // this Thorium
        let exported = export_spec(spec, index, options.strip_registry);
        for slot in find_slots(&exported).0 {
            if let SlotValue::Uuid(id) = slot.value {
                warnings.push(format!(
                    "tool {}@{}: {} holds the id {id}, which was not exported",
                    tool.name, instance.label, slot.path
                ));
            }
        }
        versions.insert(
            instance.label.clone(),
            ToolVersionManifest {
                spec: Some(exported),
                provides: sorted_refs(provides),
                ..ToolVersionManifest::default()
            },
        );
    }
    // the tool's latest label is kept when that version was exported
    let latest = tool
        .latest_label()
        .filter(|label| versions.contains_key(*label))
        .map(str::to_owned);
    ToolManifest {
        kind: Some(tool.kind),
        groups,
        description: tool.description.clone(),
        tags: tool
            .tags
            .iter()
            .map(|(key, values)| (key.clone(), values.iter().cloned().collect()))
            .collect(),
        latest,
        harness_env: tool.harness_env.clone(),
        disabled_models: tool.disabled_models.clone(),
        versions,
    }
}

/// Get the groups an exported item is in
///
/// # Arguments
///
/// * `groups` - The groups the item is in
/// * `options` - How the manifest is built
fn out_groups(groups: &[String], options: &BuildOptions) -> Vec<String> {
    // an override replaces every group, otherwise the groups are sorted
    match &options.group_override {
        Some(group) => vec![group.clone()],
        None => {
            let mut groups = groups.to_vec();
            groups.sort();
            groups.dedup();
            groups
        }
    }
}

/// Build a toolset
///
/// # Arguments
///
/// * `entity` - The toolset's entity
/// * `groups` - The groups the exported toolset is in
fn build_toolset(entity: &Entity, groups: Vec<String>) -> Option<ToolSetManifest> {
    // only toolset entities carry a toolset root
    let EntityMetadata::ToolSet(meta) = &entity.metadata else {
        return None;
    };
    Some(ToolSetManifest {
        groups,
        description: entity.description.clone(),
        read_only: Some(meta.read_only),
        selector: meta
            .selector
            .as_ref()
            .map(|selector| format!("{}={}", selector.key, selector.value)),
        auto_instance: Some(meta.auto_instance),
        tools: sorted_refs(meta.tools.iter().filter_map(entity_tool_ref).collect()),
    })
}

/// Build an agent
///
/// # Arguments
///
/// * `entity` - The agent's entity
/// * `groups` - The groups the exported agent is in
/// * `index` - The names of everything the manifest holds
fn build_agent(entity: &Entity, groups: Vec<String>, index: &Index) -> Option<AgentManifest> {
    // only agent entities carry an agent root
    let EntityMetadata::Agent(meta) = &entity.metadata else {
        return None;
    };
    // a pinned toolset instance is exported as its toolset
    let mut toolsets: Vec<ToolSetRef> = meta
        .tool_sets
        .iter()
        .filter_map(|toolset| match &toolset.metadata {
            EntityMetadata::ToolSetInstance(instance) => index.toolsets.get(&instance.tool_set),
            _ => Some(&toolset.name),
        })
        .map(|name| ToolSetRef {
            toolset: name.clone(),
        })
        .collect();
    toolsets.sort_by(|a, b| a.toolset.cmp(&b.toolset));
    toolsets.dedup();
    let mut sub_agents: Vec<String> = meta
        .sub_agents
        .iter()
        .map(|agent| agent.name.clone())
        .collect();
    sub_agents.sort();
    Some(AgentManifest {
        groups,
        description: entity.description.clone(),
        config: meta.config.clone(),
        model_auth: auth_levels(meta.model_auth.levels.iter()),
        harness: meta.harness.as_deref().and_then(entity_tool_ref),
        tools: sorted_refs(
            meta.latest_tools
                .iter()
                .chain(&meta.pinned_tools)
                .filter_map(entity_tool_ref)
                .collect(),
        ),
        toolsets,
        sub_agents,
        extend_target_groups: false,
    })
}

/// Build a workflow
///
/// # Arguments
///
/// * `entity` - The workflow's entity
/// * `groups` - The groups the exported workflow is in
fn build_workflow(entity: &Entity, groups: Vec<String>) -> Option<WorkflowManifest> {
    // only workflow entities carry a workflow root
    let EntityMetadata::WorkflowCatalog(meta) = &entity.metadata else {
        return None;
    };
    let mut agents: Vec<String> = meta.agents.iter().map(|agent| agent.name.clone()).collect();
    agents.sort();
    Some(WorkflowManifest {
        groups,
        description: entity.description.clone(),
        agents,
        entry_agent: meta
            .entry_agent
            .and_then(|id| meta.agents.iter().find(|agent| agent.id == id))
            .map(|agent| agent.name.clone()),
        limits: meta.limits.clone(),
        default_models: meta.default_models.clone(),
        model_auth: auth_levels(meta.model_auth.levels.iter()),
        auto_instance: Some(meta.auto_instance),
    })
}

/// Build a pipeline
///
/// Returns the stages with no tool behind them when the pipeline can't be exported.
///
/// # Arguments
///
/// * `pipeline` - The pipeline as read from Thorium
/// * `index` - The names of everything the manifest holds
/// * `options` - How the manifest is built
fn build_pipeline(
    pipeline: &Pipeline,
    index: &Index,
    options: &BuildOptions,
) -> Result<PipelineManifestV2, Vec<String>> {
    // every stage needs a tool the manifest can name
    let mut stage_tools = BTreeMap::new();
    let mut missing = Vec::new();
    for stage in pipeline.order.iter().flatten() {
        match pipeline
            .stage_tools
            .get(stage)
            .and_then(|id| index.tool_ref(*id))
        {
            Some(tool) => {
                stage_tools.insert(
                    stage.clone(),
                    StageTool {
                        tool: tool.tool,
                        version: tool.version,
                    },
                );
            }
            None => missing.push(stage.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    // the config is the pipeline's own settings
    let config = PipelineConfig {
        group: options
            .group_override
            .clone()
            .unwrap_or_else(|| pipeline.group.clone()),
        name: pipeline.name.clone(),
        order: serde_json::to_value(&pipeline.order).unwrap_or(Value::Null),
        sla: Some(pipeline.sla),
        triggers: pipeline.triggers.clone().into_iter().collect(),
        description: pipeline.description.clone(),
    };
    let version = PipelineVersionV2 {
        config: Some(config),
        stage_tools,
        ..PipelineVersionV2::default()
    };
    Ok(PipelineManifestV2 {
        versions: BTreeMap::from([(PIPELINE_LABEL.to_owned(), version)]),
    })
}

/// Add an item to a map, refusing two different items with one name
///
/// # Arguments
///
/// * `map` - The items by name
/// * `kind` - The kind of item, for the error
/// * `name` - The item's name
/// * `id` - The identity of the item (its id or its group), to tell two items apart
/// * `owners` - The identity of each item already added, by name
/// * `item` - The item
fn insert_unique<T>(
    map: &mut BTreeMap<String, T>,
    owners: &mut HashMap<(&'static str, String), String>,
    kind: &'static str,
    name: &str,
    id: &str,
    item: T,
) -> Result<(), Error> {
    // two entities of one kind and name can't share a manifest key
    if let Some(other) = owners.insert((kind, name.to_owned()), id.to_owned()) {
        return Err(Error::new(format!(
            "Two {kind}s are named '{name}' ({other} and {id}), but a toolbox holds one per name; \
             export one of them by itself"
        )));
    }
    map.insert(name.to_owned(), item);
    Ok(())
}

/// Build a manifest from a snapshot
///
/// # Arguments
///
/// * `name` - The name of the toolbox
/// * `snapshot` - What was read from Thorium
/// * `options` - How the manifest is built
pub(in crate::handlers::toolbox) fn build_manifest(
    name: &str,
    snapshot: &Snapshot,
    options: &BuildOptions,
) -> Result<Built, Error> {
    let index = Index::new(snapshot);
    let mut manifest = ToolboxManifestV2::new(name);
    let mut warnings = snapshot.notes.clone();
    let mut owners = HashMap::new();
    // tools
    for tool in &snapshot.tools {
        if let Err(err) = check_name(&tool.name) {
            warnings.push(format!("tool '{}': {err}", tool.name));
        }
        let groups = out_groups(&tool.groups, options);
        let built = build_tool(tool, &index, groups, options, &mut warnings);
        insert_unique(
            &mut manifest.tools,
            &mut owners,
            "tool",
            &tool.name,
            &tool.id.to_string(),
            built,
        )?;
    }
    // toolsets, agents, and workflows
    for entity in &snapshot.toolsets {
        if let Some(built) = build_toolset(entity, out_groups(&entity.groups, options)) {
            insert_unique(
                &mut manifest.toolsets,
                &mut owners,
                "toolset",
                &entity.name,
                &entity.id.to_string(),
                built,
            )?;
        }
    }
    for entity in &snapshot.agents {
        if let Some(built) = build_agent(entity, out_groups(&entity.groups, options), &index) {
            insert_unique(
                &mut manifest.agents,
                &mut owners,
                "agent",
                &entity.name,
                &entity.id.to_string(),
                built,
            )?;
        }
    }
    for entity in &snapshot.workflows {
        if let Some(built) = build_workflow(entity, out_groups(&entity.groups, options)) {
            insert_unique(
                &mut manifest.workflows,
                &mut owners,
                "workflow",
                &entity.name,
                &entity.id.to_string(),
                built,
            )?;
        }
    }
    // pipelines that have a tool behind every stage
    for pipeline in &snapshot.pipelines {
        match build_pipeline(pipeline, &index, options) {
            Ok(built) => insert_unique(
                &mut manifest.pipelines,
                &mut owners,
                "pipeline",
                &pipeline.name,
                &pipeline.group,
                built,
            )?,
            Err(stages) => warnings.push(format!(
                "pipeline {} in {} was skipped: no tool backs its stage(s) {}",
                pipeline.name,
                pipeline.group,
                stages.join(", ")
            )),
        }
    }
    // the network policies the tools name, and the groups everything is in
    manifest.network_policies.clone_from(&snapshot.policies);
    // a policy is exported in the override group, as importing with an override puts it, or else
    // in the exported items' groups only, so importing the export attaches it to the groups the
    // manifest names and the export never names groups outside it
    let item_groups = snapshot.item_groups();
    for policy in &mut manifest.network_policies {
        match &options.group_override {
            Some(group) => policy.groups = vec![group.clone()],
            None => policy.groups.retain(|group| item_groups.contains(group)),
        }
    }
    manifest
        .network_policies
        .sort_by(|a, b| a.name.cmp(&b.name));
    for group in manifest.named_groups() {
        let description = snapshot
            .groups
            .get(&group)
            .and_then(|found| found.description.clone());
        manifest.groups.insert(group, GroupManifest { description });
    }
    Ok(Built { manifest, warnings })
}

/// Bring a toolset to the form two toolsets are compared in
///
/// # Arguments
///
/// * `toolset` - The toolset to normalize
pub(in crate::handlers::toolbox) fn normalize_toolset(
    toolset: &ToolSetManifest,
) -> ToolSetManifest {
    // defaults are written out, and lists and text are put in order
    let mut toolset = toolset.clone();
    toolset.groups.sort();
    toolset.description = normalize_text(toolset.description.take());
    toolset.read_only = Some(toolset.read_only.unwrap_or(false));
    toolset.auto_instance = Some(toolset.auto_instance.unwrap_or(true));
    toolset.tools = sorted_refs(toolset.tools);
    toolset
}

/// Bring an agent to the form two agents are compared in
///
/// # Arguments
///
/// * `agent` - The agent to normalize
pub(in crate::handlers::toolbox) fn normalize_agent(agent: &AgentManifest) -> AgentManifest {
    // lists are put in order and the flag the API doesn't store is cleared
    let mut agent = agent.clone();
    agent.groups.sort();
    agent.description = normalize_text(agent.description.take());
    agent.model_auth = auth_levels(agent.model_auth.iter());
    agent.tools = sorted_refs(agent.tools);
    agent.toolsets.sort_by(|a, b| a.toolset.cmp(&b.toolset));
    agent.toolsets.dedup();
    agent.sub_agents.sort();
    agent.sub_agents.dedup();
    agent.extend_target_groups = false;
    agent
}

/// Bring a workflow to the form two workflows are compared in
///
/// # Arguments
///
/// * `workflow` - The workflow to normalize
pub(in crate::handlers::toolbox) fn normalize_workflow(
    workflow: &WorkflowManifest,
) -> WorkflowManifest {
    // defaults are written out and lists are put in order
    let mut workflow = workflow.clone();
    workflow.groups.sort();
    workflow.description = normalize_text(workflow.description.take());
    workflow.model_auth = auth_levels(workflow.model_auth.iter());
    workflow.agents.sort();
    workflow.agents.dedup();
    workflow.auto_instance = Some(workflow.auto_instance.unwrap_or(true));
    workflow
}

/// Trim a description, treating blank text as none
///
/// # Arguments
///
/// * `text` - The description
fn normalize_text(text: Option<String>) -> Option<String> {
    // blank text and no text compare equal
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}
