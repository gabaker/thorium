//! Planning a toolbox import: reading what exists and deciding what an import would do
//!
//! Planning is pure. It takes a loaded manifest and a [`World`] snapshot of the target groups and
//! produces a [`Plan`] (the types `toolbox diff` shares), the order to apply it in, and every
//! reason the import can't go ahead. Nothing is written.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thorium::models::{EntityKinds, NetworkPolicyRequest, PipelineRequest};

use super::classify::{
    RootChange, agent_view, classify_version, live_agent_view, live_toolset_view,
    live_workflow_view, pipeline_differences, root_change, toolset_view, view_diff, workflow_view,
};
use super::resolve::{Ids, Resolve, prepare_spec};
use super::world::World;
use crate::handlers::toolbox::policies::{
    PolicyClassification, PolicyUpdatePlan, build_updates, classify_policy, normalize,
};
use crate::handlers::toolbox::v2::graph::{dependencies, waves_of};
use crate::handlers::toolbox::v2::plan_types::{
    Conflict, ConflictKind, ConflictPolicy, GroupPlan, GroupState, ItemKind, ItemPlan, ItemRef,
    ItemState, Plan, PolicyPlan, PolicyState, VersionClass,
};
use crate::handlers::toolbox::v2::types::{
    PipelineConfig, PipelineManifestV2, PipelineVersionV2, ToolboxManifestV2,
};

/// What an import is asked to do
#[derive(Debug, Clone)]
pub struct Options {
    /// Whether items that don't exist are created (an update creates nothing)
    pub create: bool,
    /// Whether groups the manifest names that don't exist are created
    pub create_groups: bool,
    /// How an existing item that differs from the manifest is decided when no one is asked
    pub policy: ConflictPolicy,
    /// Whether existing network policies are brought in line with the manifest
    pub update_network_policy: bool,
}

/// An import's plan, with everything needed to apply it
pub struct ImportPlan {
    /// The read-only description of what the import does, which `--dry-run` prints
    pub plan: Plan,
    /// The items to write in the order and the waves they are applied in
    pub waves: Vec<Vec<ItemRef>>,
    /// Every reason the import can't go ahead
    pub errors: Vec<String>,
    /// The network policies to create
    pub new_policies: Vec<NetworkPolicyRequest>,
    /// The existing network policies to bring in line with the manifest
    pub policy_updates: Vec<PolicyUpdatePlan>,
}

/// The plan and the problems found, as the document `--dry-run -o json` prints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDocument {
    /// What the import would do
    pub plan: Plan,
    /// What stops the import from going ahead
    pub errors: Vec<String>,
}

impl ImportPlan {
    /// Whether applying this plan writes anything
    #[must_use]
    pub fn has_changes(&self) -> bool {
        // the plan counts groups, policies, creates, and updates
        self.plan.has_changes() || !self.policy_updates.is_empty()
    }
}

/// Get the groups of an item of a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest holding the item
/// * `item` - The item
#[must_use]
pub fn item_groups(manifest: &ToolboxManifestV2, item: &ItemRef) -> Vec<String> {
    // each kind keeps its groups in its own place
    match item.kind {
        ItemKind::Tool => manifest
            .tools
            .get(&item.name)
            .map(|tool| tool.groups.clone()),
        ItemKind::Toolset => manifest
            .toolsets
            .get(&item.name)
            .map(|toolset| toolset.groups.clone()),
        ItemKind::Agent => manifest
            .agents
            .get(&item.name)
            .map(|agent| agent.groups.clone()),
        ItemKind::Workflow => manifest
            .workflows
            .get(&item.name)
            .map(|workflow| workflow.groups.clone()),
        ItemKind::Pipeline => manifest
            .pipelines
            .get(&item.name)
            .and_then(|pipeline| choose_pipeline_version(pipeline))
            .and_then(|(_, version)| version.config.as_ref())
            .map(|config| vec![config.group.clone()]),
    }
    .unwrap_or_default()
}

/// Choose the version of a pipeline an import uses: `latest` if there is one, else the last label
///
/// # Arguments
///
/// * `pipeline` - The pipeline in the manifest
#[must_use]
pub fn choose_pipeline_version(
    pipeline: &PipelineManifestV2,
) -> Option<(&String, &PipelineVersionV2)> {
    // a pipeline has one definition in Thorium, so one version of the manifest's is used
    pipeline
        .versions
        .get_key_value("latest")
        .or_else(|| pipeline.versions.iter().next_back())
}

/// Order the items of a manifest into waves a write can be applied in
///
/// Agents can delegate to each other in a cycle, so an edge between agents is wired after every
/// agent exists and doesn't order the waves.
///
/// # Arguments
///
/// * `manifest` - The manifest to order
pub fn apply_waves(manifest: &ToolboxManifestV2) -> Result<Vec<Vec<ItemRef>>, BTreeSet<ItemRef>> {
    // drop the edges from an agent to its sub-agents
    let mut graph = dependencies(manifest);
    for (item, edges) in &mut graph {
        if item.kind == ItemKind::Agent {
            edges.retain(|edge| edge.kind != ItemKind::Agent);
        }
    }
    waves_of(&graph)
}

/// Build the plan to import a manifest into a world
///
/// # Arguments
///
/// * `manifest` - The manifest to import, with its group override and selection applied
/// * `world` - What exists in the target groups
/// * `opts` - What the import is asked to do
pub fn build_plan(manifest: &ToolboxManifestV2, world: &World, opts: &Options) -> ImportPlan {
    let mut errors: Vec<String> = Vec::new();
    let mut plan = Plan::default();
    // the groups the items name
    for name in manifest.named_groups() {
        let (state, description) = group_state(manifest, world, &name);
        // an update creates no group, so a missing one is left out of its plan
        if state == GroupState::Missing && !opts.create {
            plan.notes.push(format!(
                "group {name} doesn't exist, so nothing in it is updated"
            ));
            continue;
        }
        plan.groups.push(GroupPlan {
            name,
            state,
            description,
        });
    }
    // the network policies the tools name
    let (new_policies, policy_updates) = plan_policies(manifest, world, opts, &mut plan);
    // the order items are applied in
    let waves = match apply_waves(manifest) {
        Ok(waves) => waves,
        Err(stuck) => {
            errors.push(format!(
                "the items depend on each other in a cycle: {}",
                stuck
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<String>>()
                    .join(", ")
            ));
            Vec::new()
        }
    };
    // plan each item in dependency order
    let ids = Ids::from_world(manifest, world);
    for item in waves.iter().flatten() {
        let planned = match item.kind {
            ItemKind::Tool => plan_tool(
                manifest,
                world,
                &ids,
                opts,
                &item.name,
                &mut plan,
                &mut errors,
            ),
            ItemKind::Toolset | ItemKind::Agent | ItemKind::Workflow => {
                plan_entity(manifest, world, opts, item)
            }
            ItemKind::Pipeline => {
                plan_pipeline(manifest, world, &ids, opts, &item.name, &mut errors)
            }
        };
        match planned {
            Ok(Some(planned)) => plan.items.push(planned),
            Ok(None) => {}
            Err(message) => errors.push(message),
        }
    }
    // decide the items that differ when no one is asked
    apply_policy(&mut plan, opts.policy);
    ImportPlan {
        plan,
        waves,
        errors,
        new_policies,
        policy_updates,
    }
}

/// Decide the items that differ from the manifest according to the conflict policy
///
/// With skip-conflicts every existing item that differs is left alone. Overwriting applies them
/// all, and asking leaves the decision to a person (or to the caller, when there is none).
///
/// # Arguments
///
/// * `plan` - The plan to decide
/// * `policy` - How conflicts are decided
pub fn apply_policy(plan: &mut Plan, policy: ConflictPolicy) {
    // only skipping changes the plan
    if policy != ConflictPolicy::SkipConflicts {
        return;
    }
    // an existing item that would change is left as it is
    for item in &mut plan.items {
        if item.state == ItemState::Update {
            item.state = ItemState::Skipped;
        }
    }
}

/// Get the state of a group the manifest names
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `world` - What exists
/// * `name` - The group's name
fn group_state(
    manifest: &ToolboxManifestV2,
    world: &World,
    name: &str,
) -> (GroupState, Option<String>) {
    // the description the group is created with, when the manifest gives one
    let description = manifest
        .groups
        .get(name)
        .and_then(|group| group.description.clone());
    // a group that exists is writable or forbidden; one that doesn't is missing
    let state = match world.groups.get(name) {
        Some(info) if info.writable => GroupState::Exists,
        Some(_) => GroupState::Forbidden,
        None => GroupState::Missing,
    };
    (state, description)
}

/// Plan the network policies of a manifest
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `world` - What exists
/// * `opts` - What the import is asked to do
/// * `plan` - The plan to add each policy to
fn plan_policies(
    manifest: &ToolboxManifestV2,
    world: &World,
    opts: &Options,
    plan: &mut Plan,
) -> (Vec<NetworkPolicyRequest>, Vec<PolicyUpdatePlan>) {
    let mut new = Vec::new();
    let mut updates = Vec::new();
    // the policies of each version are bundled with the manifest's own, deduplicated by name
    let mut seen = BTreeSet::new();
    let policies = manifest.network_policies.iter().chain(
        manifest
            .tools
            .values()
            .flat_map(|tool| tool.versions.values())
            .flat_map(|version| version.network_policies.iter()),
    );
    for policy in policies {
        if !seen.insert(policy.name.clone()) {
            continue;
        }
        // hand-written addresses are put in the form Thorium stores before they are compared
        let policy = &normalize(policy.clone());
        match classify_policy(policy, &world.policies) {
            PolicyClassification::New => {
                // an update never creates a policy
                if opts.create {
                    plan.policies.push(PolicyPlan {
                        name: policy.name.clone(),
                        state: PolicyState::Create,
                    });
                    new.push(policy.clone());
                } else {
                    plan.notes.push(format!(
                        "network policy {} doesn't exist and is not created by an update",
                        policy.name
                    ));
                }
            }
            PolicyClassification::Unchanged => plan.policies.push(PolicyPlan {
                name: policy.name.clone(),
                state: PolicyState::Reuse,
            }),
            PolicyClassification::Mismatched {
                missing_groups,
                existing,
            } => {
                plan.policies.push(PolicyPlan {
                    name: policy.name.clone(),
                    state: PolicyState::Differs,
                });
                // only with the flag is the existing policy changed
                if opts.update_network_policy {
                    let mismatch = crate::handlers::toolbox::policies::PolicyMismatch {
                        name: policy.name.clone(),
                        missing_groups,
                        chosen: policy.clone(),
                        existing,
                    };
                    updates.extend(build_updates(&mismatch));
                } else {
                    plan.notes.push(format!(
                        "network policy {} differs from the toolbox and is left as it is \
                         (--update-network-policy changes it)",
                        policy.name
                    ));
                }
            }
        }
    }
    (new, updates)
}

/// Plan one tool
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `world` - What exists
/// * `ids` - The ids known before anything is created
/// * `opts` - What the import is asked to do
/// * `name` - The tool's name
/// * `plan` - The plan, which gets any notes
/// * `errors` - Where reasons the import can't go ahead are added
fn plan_tool(
    manifest: &ToolboxManifestV2,
    world: &World,
    ids: &Ids,
    opts: &Options,
    name: &str,
    plan: &mut Plan,
    errors: &mut Vec<String>,
) -> Result<Option<ItemPlan>, String> {
    let tool = &manifest.tools[name];
    let item = ItemRef::new(ItemKind::Tool, name);
    let group = tool.groups.first().cloned().unwrap_or_default();
    // find the tool in Thorium
    let existing = world.tool(name, &tool.groups)?;
    // compare each version, in the order they are published
    let mut versions = Vec::new();
    for label in tool.publish_order() {
        let version = &tool.versions[&label];
        let prepared = match prepare_spec(manifest, world, ids, version, &tool.groups) {
            Ok(prepared) => prepared,
            Err(message) => {
                errors.push(format!("tool {name}@{label}: {message}"));
                continue;
            }
        };
        versions.push(classify_version(&label, &prepared, existing));
    }
    // the differences a tool that exists has beside its versions
    let root = match existing {
        Some(live) => {
            root_change(tool, live).map_err(|message| format!("tool {name}: {message}"))?
        }
        None => RootChange::default(),
    };
    // decide what happens to the tool
    let Some(live) = existing else {
        let state = if opts.create {
            ItemState::Create
        } else {
            ItemState::NotImported
        };
        return Ok(Some(ItemPlan {
            item,
            group,
            state,
            versions,
            diff: None,
            conflict: None,
        }));
    };
    let changed: Vec<&_> = versions
        .iter()
        .filter(|version| {
            matches!(
                version.class,
                VersionClass::NewVersion | VersionClass::LabelConflict
            )
        })
        .collect();
    if changed.is_empty() && root.is_empty() {
        // everything matches, but the latest version can differ from the manifest's
        note_latest(name, live, &versions, plan);
        return Ok(Some(ItemPlan {
            item,
            group,
            state: ItemState::Unchanged,
            versions,
            diff: None,
            conflict: None,
        }));
    }
    // describe what differs
    let mut detail: Vec<String> = changed
        .iter()
        .map(|version| match (&version.class, &version.publish_label) {
            (VersionClass::LabelConflict, Some(label)) => {
                format!(
                    "version {} differs and is published as {label}",
                    version.label
                )
            }
            (VersionClass::LabelConflict, None) => {
                format!(
                    "version {} differs and is published under a new label",
                    version.label
                )
            }
            _ => format!("new version {}", version.label),
        })
        .collect();
    detail.extend(root.lines());
    let conflict_label = changed
        .iter()
        .find(|version| version.class == VersionClass::LabelConflict)
        .map(|version| version.label.clone());
    let kind = if conflict_label.is_some() {
        ConflictKind::LabelConflict
    } else {
        ConflictKind::InPlaceDiffers
    };
    Ok(Some(ItemPlan {
        item: item.clone(),
        group: group.clone(),
        state: ItemState::Update,
        versions,
        diff: None,
        conflict: Some(Conflict {
            item,
            group,
            label: conflict_label,
            kind,
            detail: detail.join("; "),
        }),
    }))
}

/// Add a note when the latest version of a tool is not the manifest's latest and every version
/// is unchanged
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `live` - The tool in Thorium
/// * `versions` - The tool's planned versions, in publish order
/// * `plan` - The plan to add the note to
fn note_latest(
    name: &str,
    live: &super::world::LiveTool,
    versions: &[crate::handlers::toolbox::v2::plan_types::VersionPlan],
    plan: &mut Plan,
) {
    // the manifest's latest is the version published last
    let Some(last) = versions.last() else {
        return;
    };
    let Some(hash) = &last.spec_hash else {
        return;
    };
    // compare with the instance Thorium follows as latest
    let live_hash = live
        .latest_label
        .as_deref()
        .and_then(|label| live.instance(label))
        .map(|instance| &instance.spec_hash);
    if live_hash.is_some_and(|live_hash| live_hash != hash) {
        plan.notes.push(format!(
            "tool {name}: its latest version is {}, not the toolbox's {}; published versions \
             can't be moved, so it is left as it is",
            live.latest_label.as_deref().unwrap_or("-"),
            last.label
        ));
    }
}

/// Plan one toolset, agent, or workflow
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `world` - What exists
/// * `opts` - What the import is asked to do
/// * `item` - The item to plan
fn plan_entity(
    manifest: &ToolboxManifestV2,
    world: &World,
    opts: &Options,
    item: &ItemRef,
) -> Result<Option<ItemPlan>, String> {
    let groups = item_groups(manifest, item);
    let group = groups.first().cloned().unwrap_or_default();
    let name = &item.name;
    // find the entity in Thorium and compare its view with the manifest's
    let (existing, diff) = match item.kind {
        ItemKind::Toolset => {
            let live = world.entity(EntityKinds::ToolSet, name, &groups)?;
            let wanted = toolset_view(name, &manifest.toolsets[name]);
            (
                live,
                live.and_then(|live| view_diff(&live_toolset_view(live), &wanted)),
            )
        }
        ItemKind::Agent => {
            let live = world.entity(EntityKinds::Agent, name, &groups)?;
            let wanted = agent_view(name, &manifest.agents[name]);
            (
                live,
                live.and_then(|live| view_diff(&live_agent_view(live), &wanted)),
            )
        }
        _ => {
            let live = world.entity(EntityKinds::WorkflowCatalog, name, &groups)?;
            let wanted = workflow_view(name, &manifest.workflows[name]);
            (
                live,
                live.and_then(|live| view_diff(&live_workflow_view(live), &wanted)),
            )
        }
    };
    Ok(Some(entity_item(
        opts,
        item,
        group,
        existing.is_some(),
        diff,
    )))
}

/// Build the plan of an item that is created or compared as a whole
///
/// # Arguments
///
/// * `opts` - What the import is asked to do
/// * `item` - The item
/// * `group` - The item's first group
/// * `exists` - Whether the item exists in Thorium
/// * `diff` - How the item differs from the manifest, when it does
fn entity_item(
    opts: &Options,
    item: &ItemRef,
    group: String,
    exists: bool,
    diff: Option<String>,
) -> ItemPlan {
    // an item that doesn't exist is created, or left out by an update
    if !exists {
        return ItemPlan {
            item: item.clone(),
            group,
            state: if opts.create {
                ItemState::Create
            } else {
                ItemState::NotImported
            },
            versions: Vec::new(),
            diff: None,
            conflict: None,
        };
    }
    // an item that matches is left alone
    let Some(diff) = diff else {
        return ItemPlan {
            item: item.clone(),
            group,
            state: ItemState::Unchanged,
            versions: Vec::new(),
            diff: None,
            conflict: None,
        };
    };
    // one that differs is a conflict
    ItemPlan {
        item: item.clone(),
        group: group.clone(),
        state: ItemState::Update,
        versions: Vec::new(),
        conflict: Some(Conflict {
            item: item.clone(),
            group,
            label: None,
            kind: ConflictKind::InPlaceDiffers,
            detail: format!("the {} differs from the toolbox", item.kind),
        }),
        diff: Some(diff),
    }
}

/// Resolve the stage tools of a pipeline version to ids
///
/// Returns `None` when a tool is created by the import itself, so its id isn't known yet.
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `world` - What exists
/// * `ids` - The ids known
/// * `config` - The pipeline's config
/// * `version` - The pipeline version
pub fn pipeline_request(
    manifest: &ToolboxManifestV2,
    world: &World,
    ids: &Ids,
    config: &PipelineConfig,
    version: &PipelineVersionV2,
) -> Result<Option<PipelineRequest>, String> {
    let groups = vec![config.group.clone()];
    let mut stage_tools = std::collections::BTreeMap::new();
    let mut pending = false;
    // resolve the tool behind each stage
    for (stage, stage_tool) in &version.stage_tools {
        match ids.resolve(
            manifest,
            world,
            &stage_tool.tool,
            stage_tool.version.as_deref(),
            &groups,
        ) {
            Resolve::Id(id) => {
                stage_tools.insert(stage.clone(), id);
            }
            Resolve::Pending(_) => pending = true,
            Resolve::Missing(message) => {
                return Err(format!("stage {stage}: {message}"));
            }
        }
    }
    if pending {
        return Ok(None);
    }
    Ok(Some(config.to_request(stage_tools)))
}

/// Plan one pipeline
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `world` - What exists
/// * `ids` - The ids known before anything is created
/// * `opts` - What the import is asked to do
/// * `name` - The pipeline's name
/// * `errors` - Where reasons the import can't go ahead are added
fn plan_pipeline(
    manifest: &ToolboxManifestV2,
    world: &World,
    ids: &Ids,
    opts: &Options,
    name: &str,
    errors: &mut Vec<String>,
) -> Result<Option<ItemPlan>, String> {
    let item = ItemRef::new(ItemKind::Pipeline, name);
    let Some((_, version)) = choose_pipeline_version(&manifest.pipelines[name]) else {
        return Ok(None);
    };
    let Some(config) = &version.config else {
        errors.push(format!("pipeline {name}: has no config"));
        return Ok(None);
    };
    let group = config.group.clone();
    let existing = world.pipeline(&group, &config.name);
    // the stage tools may be created by this import, which leaves their ids unknown
    let request = pipeline_request(manifest, world, ids, config, version)
        .map_err(|message| format!("pipeline {name}: {message}"))?;
    let Some(live) = existing else {
        return Ok(Some(entity_item(opts, &item, group, false, None)));
    };
    // compare a pipeline that exists
    let differences = match &request {
        Some(request) => pipeline_differences(live, request),
        None => vec!["a stage's tool is created by this import".to_owned()],
    };
    let diff = (!differences.is_empty()).then(|| differences.join("\n"));
    let mut planned = entity_item(opts, &item, group, true, diff);
    // say what differs, rather than that something does
    if let Some(conflict) = &mut planned.conflict {
        conflict.detail = differences.join("; ");
    }
    Ok(Some(planned))
}
