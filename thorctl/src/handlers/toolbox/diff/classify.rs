//! Compares a toolbox manifest with what Thorium holds and says what an import would do
//!
//! A tool version is compared by its spec hash, the same content hash the API gives a tool
//! instance, so a version is unchanged exactly when some instance of the tool already has its
//! spec. The other kinds are compared field by field after both sides are put in the same
//! normal form.

use serde_json::Value;
use similar::TextDiff;
use std::collections::BTreeMap;
use thorium::models::{HarnessToolAccess, NetworkPolicyRequest, ThorDigest, ToolSpec};

use crate::handlers::toolbox::export::live::LiveView;
use crate::handlers::toolbox::export::snapshot::{
    normalize_agent, normalize_toolset, normalize_workflow,
};
use crate::handlers::toolbox::v2::graph::waves;
use crate::handlers::toolbox::v2::plan_types::{
    Conflict, ConflictKind, GroupPlan, GroupState, ItemKind, ItemPlan, ItemRef, ItemState, Plan,
    PolicyPlan, PolicyState, VersionClass, VersionPlan,
};
use crate::handlers::toolbox::v2::refs::resolve_slots;
use crate::handlers::toolbox::v2::types::{
    PipelineManifestV2, PipelineVersionV2, ToolManifest, ToolRef, ToolboxManifestV2,
};

/// Compute the spec hash Thorium would give a manifest version
///
/// References are resolved against the tools Thorium holds, so the hash can only be computed
/// when everything the spec refers to exists there.
///
/// # Arguments
///
/// * `spec` - The version's spec as JSON, with references in its id slots
/// * `provides` - The native tools the version provides (harness versions only)
/// * `live` - What Thorium holds
pub(in crate::handlers::toolbox) fn version_hash(
    spec: &Value,
    provides: &[ToolRef],
    live: &LiveView,
) -> Result<String, String> {
    // swap each reference for the id of the live tool or instance it names
    let mut resolved = spec.clone();
    let problems = resolve_slots(&mut resolved, |reference, kind| {
        live.resolve(
            reference,
            kind == crate::handlers::toolbox::v2::refs::SlotKind::ToolInstanceId,
        )
    });
    if let Some(problem) = problems.first() {
        return Err(format!("{}: {}", problem.path, problem.message));
    }
    // parse the typed spec the way the API does
    let mut typed = ToolSpec::parse_request(&resolved.to_string())?;
    // a default tool access block is stored unset, so it hashes like one left out
    if let ToolSpec::Harness(harness) = &mut typed
        && harness.interface.tool_access == Some(HarnessToolAccess::default())
    {
        harness.interface.tool_access = None;
    }
    // a container backing contributes the hash of the instance it pins
    let pinned = match typed.pinned_instance() {
        Some(id) => Some(
            live.instance_hash(id)
                .ok_or_else(|| format!("the pinned tool instance {id} is not in Thorium"))?
                .to_owned(),
        ),
        None => None,
    };
    // so do the native tools a harness provides
    let mut hashes = Vec::new();
    for tool in provides {
        let hash = live.ref_hash(tool).ok_or_else(|| {
            format!(
                "the provided tool {}{} is not in Thorium",
                tool.tool,
                tool.version
                    .as_ref()
                    .map_or_else(String::new, |label| format!("@{label}"))
            )
        })?;
        hashes.push(hash);
    }
    ThorDigest::provided_tool_spec(&typed, pinned.as_deref(), &hashes)
        .map_err(|err| format!("failed to hash the spec: {err}"))
}

/// Classify the versions of a tool that exists in Thorium
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `tool` - The tool in the manifest
/// * `live` - What Thorium holds
fn classify_versions(name: &str, tool: &ToolManifest, live: &LiveView) -> Vec<VersionPlan> {
    let existing = live.tool(name, &tool.groups);
    let order = tool.publish_order();
    let mut versions = Vec::new();
    for label in order {
        let version = &tool.versions[&label];
        // the hash Thorium would give this version, when everything it refers to exists
        let hash = version
            .spec
            .as_ref()
            .and_then(|spec| version_hash(spec, &version.provides, live).ok());
        // a tool that doesn't exist has only new versions
        let Some(existing) = existing else {
            versions.push(VersionPlan {
                label,
                class: VersionClass::New,
                spec_hash: hash,
                publish_label: None,
            });
            continue;
        };
        // some instance with the hash is the version; else the label may be taken
        let same_hash = hash
            .as_ref()
            .is_some_and(|hash| existing.instances.iter().any(|i| i.spec_hash == *hash));
        let label_taken = existing.instances.iter().any(|i| i.label == label);
        let class = if same_hash {
            VersionClass::Unchanged
        } else if label_taken {
            VersionClass::LabelConflict
        } else {
            VersionClass::NewVersion
        };
        let publish_label = match (&class, &hash) {
            (VersionClass::LabelConflict, Some(hash)) => Some(
                crate::handlers::toolbox::v2::plan_types::label_with_hash(&label, hash),
            ),
            _ => None,
        };
        versions.push(VersionPlan {
            label,
            class,
            spec_hash: hash,
            publish_label,
        });
    }
    versions
}

/// Plan what an import does with a tool
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `tool` - The tool in the manifest
/// * `live` - What Thorium holds
fn plan_tool(name: &str, tool: &ToolManifest, live: &LiveView) -> ItemPlan {
    let versions = classify_versions(name, tool, live);
    let exists = live.tool(name, &tool.groups).is_some();
    // a conflict is the first version whose label is taken by another spec
    let conflict = versions
        .iter()
        .find(|version| version.class == VersionClass::LabelConflict)
        .map(|version| Conflict {
            item: ItemRef::new(ItemKind::Tool, name),
            group: tool.groups.first().cloned().unwrap_or_default(),
            label: Some(version.label.clone()),
            kind: ConflictKind::LabelConflict,
            detail: format!("version {} exists with another spec", version.label),
        });
    let changes = versions
        .iter()
        .any(|version| version.class != VersionClass::Unchanged);
    let state = match (exists, changes) {
        (false, _) => ItemState::Create,
        (true, true) => ItemState::Update,
        (true, false) => ItemState::Unchanged,
    };
    ItemPlan {
        item: ItemRef::new(ItemKind::Tool, name),
        group: tool.groups.first().cloned().unwrap_or_default(),
        state,
        versions,
        diff: None,
        conflict,
    }
}

/// Convert an item to JSON so two items compare by value
///
/// # Arguments
///
/// * `item` - The item to convert
fn as_value<T: serde::Serialize>(item: Option<&T>) -> Option<Value> {
    // an item that can't be converted compares as missing
    item.and_then(|item| serde_json::to_value(item).ok())
}

/// Render an item as YAML for a diff
///
/// # Arguments
///
/// * `item` - The item to render
fn yaml_of(item: &Value) -> String {
    // the YAML of the normalized item
    serde_norway::to_string(item).unwrap_or_default()
}

/// Build the unified diff of two renderings of an item
///
/// # Arguments
///
/// * `item` - The item the diff is about
/// * `live` - The item as Thorium holds it
/// * `manifest` - The item as the manifest has it
fn unified(item: &ItemRef, live: &str, manifest: &str) -> String {
    // line by line, with three lines of context
    TextDiff::from_lines(live, manifest)
        .unified_diff()
        .context_radius(3)
        .header(
            &format!("live/{} {}", item.kind, item.name),
            &format!("toolbox/{} {}", item.kind, item.name),
        )
        .to_string()
}

/// Plan what an import does with an item that is updated in place
///
/// # Arguments
///
/// * `item` - The item
/// * `group` - The group the item is in
/// * `live_view` - The item as Thorium holds it, if it exists
/// * `manifest_view` - The item as the manifest has it
fn plan_in_place(
    item: &ItemRef,
    group: &str,
    live_view: Option<&Value>,
    manifest_view: &Value,
) -> ItemPlan {
    let make = |state, diff: Option<String>, conflict: Option<Conflict>| ItemPlan {
        item: item.clone(),
        group: group.to_owned(),
        state,
        versions: Vec::new(),
        diff,
        conflict,
    };
    // an item Thorium doesn't have is created
    let Some(live_view) = live_view else {
        return make(ItemState::Create, None, None);
    };
    // an identical one is left alone
    if live_view == manifest_view {
        return make(ItemState::Unchanged, None, None);
    }
    // otherwise it is updated, with the difference shown
    let diff = unified(item, &yaml_of(live_view), &yaml_of(manifest_view));
    let conflict = Conflict {
        item: item.clone(),
        group: group.to_owned(),
        label: None,
        kind: ConflictKind::InPlaceDiffers,
        detail: "differs from the manifest".to_owned(),
    };
    make(ItemState::Update, Some(diff), Some(conflict))
}

/// Get the normal form of a pipeline version for comparing
///
/// A manifest that leaves the SLA unset takes the live pipeline's.
///
/// # Arguments
///
/// * `version` - The pipeline version
/// * `sla` - The SLA to use when the version sets none
fn normalized_pipeline(version: &PipelineVersionV2, sla: Option<u64>) -> Value {
    // the config's settings and the stage tools, with descriptions trimmed
    let config = version.config.as_ref();
    serde_json::json!({
        "group": config.map(|config| config.group.clone()),
        "order": config.map(|config| config.order.clone()),
        "sla": config.and_then(|config| config.sla).or(sla),
        "triggers": config.map(|config| config.triggers.clone()),
        "description": config
            .and_then(|config| config.description.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty()),
        "stage_tools": version.stage_tools,
    })
}

/// Plan what an import does with a pipeline
///
/// # Arguments
///
/// * `name` - The pipeline's name
/// * `pipeline` - The pipeline in the manifest
/// * `live` - What Thorium holds
fn plan_pipeline(name: &str, pipeline: &PipelineManifestV2, live: &LiveView) -> ItemPlan {
    // the manifest's version, compared with the pipeline of the same name in its group
    let version = pipeline.versions.values().next();
    let group = version
        .and_then(|version| version.config.as_ref())
        .map_or_else(String::new, |config| config.group.clone());
    let existing: Option<&PipelineManifestV2> = live.live.pipelines.get(name).filter(|found| {
        found
            .versions
            .values()
            .filter_map(|version| version.config.as_ref())
            .any(|config| config.group == group)
    });
    let live_version = existing.and_then(|found| found.versions.values().next());
    let live_sla = live_version
        .and_then(|version| version.config.as_ref())
        .and_then(|config| config.sla);
    let manifest_value = version.map(|version| normalized_pipeline(version, live_sla));
    let live_value = live_version.map(|version| normalized_pipeline(version, live_sla));
    let item = ItemRef::new(ItemKind::Pipeline, name);
    match manifest_value {
        Some(manifest_value) => plan_in_place(&item, &group, live_value.as_ref(), &manifest_value),
        None => plan_in_place(&item, &group, live_value.as_ref(), &Value::Null),
    }
}

/// Plan what an import does with each network policy the manifest defines
///
/// # Arguments
///
/// * `manifest` - The manifest
/// * `live` - What Thorium holds
fn plan_policies(manifest: &ToolboxManifestV2, live: &LiveView) -> Vec<PolicyPlan> {
    // the policies at the top level and the ones bundled with a version, by name
    let mut defined: BTreeMap<String, &NetworkPolicyRequest> = BTreeMap::new();
    for policy in manifest.network_policies.iter().chain(
        manifest
            .tools
            .values()
            .flat_map(|tool| tool.versions.values())
            .flat_map(|version| &version.network_policies),
    ) {
        defined.entry(policy.name.clone()).or_insert(policy);
    }
    // a policy is compared without its groups, which an override changes
    let comparable = |policy: &NetworkPolicyRequest| {
        let mut value = serde_json::to_value(policy).unwrap_or(Value::Null);
        if let Some(object) = value.as_object_mut() {
            object.remove("groups");
        }
        value
    };
    defined
        .into_iter()
        .map(|(name, policy)| {
            let state = match live.policies.get(&name) {
                Some(Some(found)) if comparable(found) == comparable(policy) => PolicyState::Reuse,
                Some(Some(_)) => PolicyState::Differs,
                _ => PolicyState::Create,
            };
            PolicyPlan { name, state }
        })
        .collect()
}

/// Plan what importing a manifest into Thorium would do
///
/// # Arguments
///
/// * `manifest` - The manifest, with its specs in place
/// * `live` - What Thorium holds
pub(in crate::handlers::toolbox) fn plan(manifest: &ToolboxManifestV2, live: &LiveView) -> Plan {
    // the groups the manifest names
    let groups = manifest
        .named_groups()
        .into_iter()
        .map(|name| GroupPlan {
            state: live
                .groups
                .get(&name)
                .copied()
                .unwrap_or(GroupState::Exists),
            description: manifest
                .groups
                .get(&name)
                .and_then(|group| group.description.clone()),
            name,
        })
        .collect();
    // the items in the order an import creates them, falling back to name order around a cycle
    let order: Vec<ItemRef> = match waves(manifest) {
        Ok(waves) => waves.into_iter().flatten().collect(),
        Err(_) => crate::handlers::toolbox::export::items_of(manifest),
    };
    let mut items = Vec::new();
    for item in order {
        let plan = match item.kind {
            ItemKind::Tool => plan_tool(&item.name, &manifest.tools[&item.name], live),
            ItemKind::Toolset => {
                let wanted = normalize_toolset(&manifest.toolsets[&item.name]);
                let group = wanted.groups.first().cloned().unwrap_or_default();
                let found = live.live.toolsets.get(&item.name).map(normalize_toolset);
                plan_in_place(
                    &item,
                    &group,
                    as_value(found.as_ref()).as_ref(),
                    &as_value(Some(&wanted)).unwrap_or_default(),
                )
            }
            ItemKind::Agent => {
                let wanted = normalize_agent(&manifest.agents[&item.name]);
                let group = wanted.groups.first().cloned().unwrap_or_default();
                let found = live.live.agents.get(&item.name).map(normalize_agent);
                plan_in_place(
                    &item,
                    &group,
                    as_value(found.as_ref()).as_ref(),
                    &as_value(Some(&wanted)).unwrap_or_default(),
                )
            }
            ItemKind::Workflow => {
                let wanted = normalize_workflow(&manifest.workflows[&item.name]);
                let group = wanted.groups.first().cloned().unwrap_or_default();
                let found = live.live.workflows.get(&item.name).map(normalize_workflow);
                plan_in_place(
                    &item,
                    &group,
                    as_value(found.as_ref()).as_ref(),
                    &as_value(Some(&wanted)).unwrap_or_default(),
                )
            }
            ItemKind::Pipeline => plan_pipeline(&item.name, &manifest.pipelines[&item.name], live),
        };
        items.push(plan);
    }
    // a tool whose latest is another version than the manifest's is worth a note
    let mut notes = Vec::new();
    for (name, tool) in &manifest.tools {
        let (Some(latest), Some(found)) = (&tool.latest, live.tool(name, &tool.groups)) else {
            continue;
        };
        if found
            .latest_label()
            .is_some_and(|live_latest| live_latest != latest)
        {
            notes.push(format!(
                "tool {name}: the manifest's latest is {latest} but Thorium's is {}",
                found.latest_label().unwrap_or_default()
            ));
        }
    }
    Plan {
        groups,
        policies: plan_policies(manifest, live),
        items,
        notes,
    }
}
