//! Tests for planning an import: what exists decides what is created, published, or updated

use serde_json::json;
use thorium::models::{EntityKinds, EntityMetadata, ToolSetEntity};

use super::{
    hash_of, image_spec, image_spec_depending_on, live_tool, manifest, tool_json, world_with,
};
use crate::handlers::agents::fixtures::{entity, tool as tool_entity, toolset_with};
use crate::handlers::toolbox::import::plan::{ImportPlan, Options, build_plan};
use crate::handlers::toolbox::import::preflight;
use crate::handlers::toolbox::v2::plan_types::{
    ConflictKind, ConflictPolicy, GroupState, ItemKind, ItemPlan, ItemRef, ItemState, PolicyState,
    VersionClass,
};

/// Options of an import that creates what is missing and settles conflicts by a policy
///
/// # Arguments
///
/// * `policy` - How the import settles an item that differs from the toolbox
fn options(policy: ConflictPolicy) -> Options {
    // creating, not creating groups, never touching network policies
    Options {
        create: true,
        create_groups: false,
        policy,
        update_network_policy: false,
    }
}

/// Find the plan of a tool
///
/// # Arguments
///
/// * `plan` - The plan
/// * `name` - The tool's name
fn tool_plan<'a>(plan: &'a ImportPlan, name: &str) -> &'a ItemPlan {
    // the item of that kind and name
    plan.plan
        .items
        .iter()
        .find(|item| item.item == ItemRef::new(ItemKind::Tool, name))
        .unwrap_or_else(|| panic!("no plan for tool {name}"))
}

/// A tool that doesn't exist is created, with every version new
#[test]
fn a_missing_tool_is_created() {
    // a manifest with one tool of two versions, in a group that exists
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("1.0", image_spec("a:1")), ("1.1", image_spec("a:2"))])}
    }));
    let world = world_with(&["lab"]);
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    // the tool is created, and its versions are new with their hashes
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let tool = tool_plan(&plan, "clamav");
    assert_eq!(tool.state, ItemState::Create);
    assert_eq!(tool.versions.len(), 2);
    assert!(tool.versions.iter().all(|v| v.class == VersionClass::New));
    assert_eq!(
        tool.versions[0].spec_hash.as_deref(),
        Some(hash_of(&image_spec("a:1")).as_str())
    );
    assert!(plan.plan.has_changes());
}

/// A tool whose versions are all published is unchanged and the plan changes nothing
#[test]
fn a_published_tool_is_unchanged() {
    // the world already has both versions, under their hashes
    let spec_a = image_spec("a:1");
    let spec_b = image_spec("a:2");
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("1.0", spec_a.clone()), ("1.1", spec_b.clone())])}
    }));
    let mut world = world_with(&["lab"]);
    world.tools.push(live_tool(
        "clamav",
        &["lab"],
        &[("1.0", &hash_of(&spec_a)), ("1.1", &hash_of(&spec_b))],
    ));
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    // nothing is written
    let tool = tool_plan(&plan, "clamav");
    assert_eq!(tool.state, ItemState::Unchanged);
    assert!(
        tool.versions
            .iter()
            .all(|v| v.class == VersionClass::Unchanged)
    );
    assert!(tool.conflict.is_none());
    assert!(!plan.has_changes());
}

/// A version whose spec exists under another label is unchanged
#[test]
fn a_spec_under_another_label_is_unchanged() {
    // the world has the spec as `old`, the manifest calls it `new`
    let spec = image_spec("a:1");
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("new", spec.clone())])}
    }));
    let mut world = world_with(&["lab"]);
    world
        .tools
        .push(live_tool("clamav", &["lab"], &[("old", &hash_of(&spec))]));
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    // the hash decides, not the label
    assert_eq!(
        tool_plan(&plan, "clamav").versions[0].class,
        VersionClass::Unchanged
    );
    assert!(!plan.has_changes());
}

/// A new label with a new spec is a new version, an update of the tool
#[test]
fn a_new_label_is_a_new_version() {
    // the world has 1.0; the manifest adds 1.1
    let spec_a = image_spec("a:1");
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("1.0", spec_a.clone()), ("1.1", image_spec("a:2"))])}
    }));
    let mut world = world_with(&["lab"]);
    world
        .tools
        .push(live_tool("clamav", &["lab"], &[("1.0", &hash_of(&spec_a))]));
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    // only the new version is published
    let tool = tool_plan(&plan, "clamav");
    assert_eq!(tool.state, ItemState::Update);
    assert_eq!(tool.versions[0].class, VersionClass::Unchanged);
    assert_eq!(tool.versions[1].class, VersionClass::NewVersion);
    let conflict = tool.conflict.as_ref().expect("a conflict");
    assert_eq!(conflict.kind, ConflictKind::InPlaceDiffers);
}

/// A label taken by another spec is a label conflict, published under the label and a hash prefix
#[test]
fn a_taken_label_is_a_label_conflict() {
    // the world has 1.0 with another spec
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("1.0", image_spec("a:2"))])}
    }));
    let mut world = world_with(&["lab"]);
    world
        .tools
        .push(live_tool("clamav", &["lab"], &[("1.0", "v1:other")]));
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    // the conflict names the label that will be published
    let tool = tool_plan(&plan, "clamav");
    assert_eq!(tool.versions[0].class, VersionClass::LabelConflict);
    let hash = hash_of(&image_spec("a:2"));
    let prefix: String = hash.chars().take(8).collect();
    assert_eq!(
        tool.versions[0].publish_label,
        Some(format!("1.0+{prefix}"))
    );
    let conflict = tool.conflict.as_ref().expect("a conflict");
    assert_eq!(conflict.kind, ConflictKind::LabelConflict);
    assert_eq!(conflict.label.as_deref(), Some("1.0"));
}

/// Skipping conflicts creates what is missing and leaves everything that differs alone
#[test]
fn skip_conflicts_leaves_existing_items_alone() {
    // one tool differs, one is missing
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {
            "differs": tool_json(&["lab"], &[("1.0", image_spec("a:2"))]),
            "missing": tool_json(&["lab"], &[("1.0", image_spec("b:1"))])
        }
    }));
    let mut world = world_with(&["lab"]);
    world
        .tools
        .push(live_tool("differs", &["lab"], &[("1.0", "v1:other")]));
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::SkipConflicts));
    // the differing tool is skipped; the missing one is created
    assert_eq!(tool_plan(&plan, "differs").state, ItemState::Skipped);
    assert_eq!(tool_plan(&plan, "missing").state, ItemState::Create);
    let summary = plan.plan.summary();
    assert_eq!((summary.create, summary.skipped, summary.update), (1, 1, 0));
    // asking leaves the item a conflict for a person to decide
    let asked = build_plan(&manifest, &world, &options(ConflictPolicy::Ask));
    assert_eq!(tool_plan(&asked, "differs").state, ItemState::Update);
    assert_eq!(asked.plan.conflicts().len(), 1);
}

/// An update creates nothing: missing items are not imported and missing groups are left out
#[test]
fn an_update_creates_nothing() {
    // a missing tool, a tool with a new version, and a tool in a group that doesn't exist
    let spec_a = image_spec("a:1");
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {
            "missing": tool_json(&["lab"], &[("1.0", image_spec("m:1"))]),
            "grows": tool_json(&["lab"], &[("1.0", spec_a.clone()), ("2.0", image_spec("a:2"))]),
            "elsewhere": tool_json(&["gone"], &[("1.0", image_spec("e:1"))])
        },
        "network_policies": [{"name": "p", "groups": ["lab"]}]
    }));
    let mut world = world_with(&["lab"]);
    world
        .tools
        .push(live_tool("grows", &["lab"], &[("1.0", &hash_of(&spec_a))]));
    let mut opts = options(ConflictPolicy::Overwrite);
    opts.create = false;
    let plan = build_plan(&manifest, &world, &opts);
    // nothing is created, and the group that doesn't exist is not planned
    assert_eq!(tool_plan(&plan, "missing").state, ItemState::NotImported);
    assert_eq!(tool_plan(&plan, "elsewhere").state, ItemState::NotImported);
    assert_eq!(tool_plan(&plan, "grows").state, ItemState::Update);
    assert!(
        plan.plan
            .groups
            .iter()
            .all(|group| group.state != GroupState::Missing)
    );
    assert!(plan.new_policies.is_empty());
    assert!(
        plan.plan
            .notes
            .iter()
            .any(|note| note.contains("not created by an update"))
    );
}

/// Versions are planned in publish order: semver ascending, custom labels, then the latest label
#[test]
fn versions_are_planned_in_publish_order() {
    // labels in a jumbled order, with `latest` named as the manifest's latest
    let mut tool = tool_json(
        &["lab"],
        &[
            ("latest", image_spec("a:5")),
            ("zeta", image_spec("a:4")),
            ("1.10.0", image_spec("a:3")),
            ("1.9.0", image_spec("a:2")),
            ("alpha", image_spec("a:1")),
        ],
    );
    tool["latest"] = json!("latest");
    let manifest = manifest(&json!({"name": "t", "tools": {"clamav": tool}}));
    let plan = build_plan(
        &manifest,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    // 1.9.0 before 1.10.0, then the custom labels, then latest last
    let labels: Vec<&str> = tool_plan(&plan, "clamav")
        .versions
        .iter()
        .map(|version| version.label.as_str())
        .collect();
    assert_eq!(labels, ["1.9.0", "1.10.0", "alpha", "zeta", "latest"]);
}

/// A group that doesn't exist is missing, and the import is refused unless groups are created
#[test]
fn a_missing_group_needs_create_groups() {
    // a tool in a group that doesn't exist
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["new"], &[("1.0", image_spec("a:1"))])}
    }));
    let world = world_with(&["lab"]);
    let mut opts = options(ConflictPolicy::Overwrite);
    let plan = build_plan(&manifest, &world, &opts);
    assert_eq!(plan.plan.groups[0].name, "new");
    assert_eq!(plan.plan.groups[0].state, GroupState::Missing);
    // refused without --create-groups, with the reason named
    let problems = preflight::check(&manifest, &world, &opts, &plan);
    assert!(
        problems.iter().any(|p| p.contains("--create-groups")),
        "{problems:?}"
    );
    // allowed with it
    opts.create_groups = true;
    let plan = build_plan(&manifest, &world, &opts);
    assert!(preflight::check(&manifest, &world, &opts, &plan).is_empty());
}

/// A group the caller can't write to is an error only when something would be written there
#[test]
fn a_forbidden_group_is_an_error_only_when_written() {
    // the group exists but can't be written to
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("1.0", image_spec("a:1"))])}
    }));
    let mut world = world_with(&["lab"]);
    world.groups.get_mut("lab").expect("group").writable = false;
    let opts = options(ConflictPolicy::Overwrite);
    let plan = build_plan(&manifest, &world, &opts);
    assert_eq!(plan.plan.groups[0].state, GroupState::Forbidden);
    let problems = preflight::check(&manifest, &world, &opts, &plan);
    assert!(
        problems.iter().any(|p| p.contains("group lab")),
        "{problems:?}"
    );
    // with the tool already published there is nothing to write, so nothing to refuse
    world.tools.push(live_tool(
        "clamav",
        &["lab"],
        &[("1.0", &hash_of(&image_spec("a:1")))],
    ));
    let plan = build_plan(&manifest, &world, &opts);
    assert!(preflight::check(&manifest, &world, &opts, &plan).is_empty());
}

/// An admin-only security context is refused for a caller who isn't an admin
#[test]
fn an_admin_only_spec_is_refused_for_others() {
    // a spec with a non-default security context
    let mut spec = image_spec("a:1");
    spec["Image"]["environment"]["K8s"]["security_context"] =
        json!({"user": 0, "group": 0, "allow_privilege_escalation": true});
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"clamav": tool_json(&["lab"], &[("1.0", spec)])}
    }));
    let mut world = world_with(&["lab"]);
    let opts = options(ConflictPolicy::Overwrite);
    let plan = build_plan(&manifest, &world, &opts);
    let problems = preflight::check(&manifest, &world, &opts, &plan);
    assert!(
        problems.iter().any(|p| p.contains("only an admin")),
        "{problems:?}"
    );
    // an admin may publish it
    world.caller.as_mut().expect("caller").is_admin = true;
    assert!(preflight::check(&manifest, &world, &opts, &plan).is_empty());
}

/// A tool that depends on another tool of the manifest is applied in a later wave
#[test]
fn dependencies_order_the_waves() {
    // pipeline -> tool b -> tool a, plus an unrelated tool
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:1"))]),
            "b": tool_json(&["lab"], &[("1", image_spec_depending_on("b:1", &["ref:tool/a"]))]),
            "c": tool_json(&["lab"], &[("1", image_spec("c:1"))])
        },
        "pipelines": {"p": {"versions": {"latest": {
            "config": {"group": "lab", "name": "p", "order": [["b"]]},
            "stage_tools": {"b": {"tool": "b", "version": "1"}}
        }}}}
    }));
    let plan = build_plan(
        &manifest,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    // a and c first, then b, then the pipeline
    let names: Vec<Vec<String>> = plan
        .waves
        .iter()
        .map(|wave| wave.iter().map(ToString::to_string).collect())
        .collect();
    assert_eq!(
        names,
        [
            vec!["tool a".to_owned(), "tool c".to_owned()],
            vec!["tool b".to_owned()],
            vec!["pipeline p".to_owned()]
        ]
    );
    // the plan lists the items in the same order
    let order: Vec<String> = plan.plan.items.iter().map(|i| i.item.to_string()).collect();
    assert_eq!(order, ["tool a", "tool c", "tool b", "pipeline p"]);
}

/// A dependency on a tool that is created by the import has no hash yet; one on an existing tool does
#[test]
fn references_to_existing_tools_are_hashed() {
    // b depends on tool `base`, which is in Thorium and not in the manifest
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"b": tool_json(&["lab"], &[("1", image_spec_depending_on("b:1", &["ref:tool/base"]))])}
    }));
    let mut world = world_with(&["lab"]);
    world
        .tools
        .push(live_tool("base", &["lab"], &[("1", "v1:base")]));
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(tool_plan(&plan, "b").versions[0].spec_hash.is_some());
    // with nothing named `base` anywhere the import can't go ahead
    let plan = build_plan(
        &manifest,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    assert!(
        plan.errors.iter().any(|e| e.contains("base")),
        "{:?}",
        plan.errors
    );
    // a dependency on a tool of the manifest that doesn't exist yet leaves the hash unknown
    let both = super::manifest(&json!({
        "name": "t",
        "tools": {
            "base": tool_json(&["lab"], &[("1", image_spec("base:1"))]),
            "b": tool_json(&["lab"], &[("1", image_spec_depending_on("b:1", &["ref:tool/base"]))])
        }
    }));
    let plan = build_plan(
        &both,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(tool_plan(&plan, "b").versions[0].spec_hash.is_none());
}

/// Agents that delegate to each other in a cycle are still planned
#[test]
fn agent_cycles_are_planned() {
    // a delegates to b and b delegates to a
    let manifest = manifest(&json!({
        "name": "t",
        "agents": {
            "a": {"groups": ["lab"], "sub_agents": ["b"]},
            "b": {"groups": ["lab"], "sub_agents": ["a"]}
        },
        "workflows": {"w": {"groups": ["lab"], "agents": ["a", "b"], "entry_agent": "a"}}
    }));
    let plan = build_plan(
        &manifest,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    // both agents come before the workflow
    let order: Vec<String> = plan.plan.items.iter().map(|i| i.item.to_string()).collect();
    assert_eq!(order, ["agent a", "agent b", "workflow w"]);
}

/// A toolset that matches is unchanged; one that differs is an update with a diff
#[test]
fn toolsets_compare_by_view() {
    // a toolset in Thorium holding the tool `strings`
    let manifest_json = |tools: serde_json::Value| {
        manifest(&json!({
            "name": "t",
            "tools": {"strings": tool_json(&["lab"], &[("1", image_spec("s:1"))])},
            "toolsets": {"kit": {"groups": ["lab"], "tools": tools}}
        }))
    };
    let live = toolset_with("kit", |meta: &mut ToolSetEntity| {
        meta.tools = vec![tool_entity("strings")];
    });
    let mut world = world_with(&["lab"]);
    world.toolsets.push(live);
    world.tools.push(live_tool(
        "strings",
        &["lab"],
        &[("1", &hash_of(&image_spec("s:1")))],
    ));
    // the same tools: unchanged
    let same = build_plan(
        &manifest_json(json!([{"tool": "strings"}])),
        &world,
        &options(ConflictPolicy::Overwrite),
    );
    let item = same
        .plan
        .items
        .iter()
        .find(|item| item.item.kind == ItemKind::Toolset)
        .expect("a toolset");
    assert_eq!(item.state, ItemState::Unchanged);
    // a pinned tool: differs, with a diff naming it
    let differs = build_plan(
        &manifest_json(json!([{"tool": "strings", "version": "1"}])),
        &world,
        &options(ConflictPolicy::Overwrite),
    );
    let item = differs
        .plan
        .items
        .iter()
        .find(|item| item.item.kind == ItemKind::Toolset)
        .expect("a toolset");
    assert_eq!(item.state, ItemState::Update);
    assert!(
        item.diff
            .as_deref()
            .is_some_and(|diff| diff.contains("version"))
    );
    assert_eq!(
        item.conflict.as_ref().expect("conflict").kind,
        ConflictKind::InPlaceDiffers
    );
}

/// A pipeline is created when missing, unchanged when it matches, and updated when a stage tool differs
#[test]
fn pipelines_compare_with_their_stage_tools() {
    // the tool the stage runs, and the pipeline that exists with it
    let spec = image_spec("s:1");
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"strings": tool_json(&["lab"], &[("1", spec.clone())])},
        "pipelines": {"scan": {"versions": {"latest": {
            "config": {"group": "lab", "name": "scan", "order": [["strings"]], "sla": 100},
            "stage_tools": {"strings": {"tool": "strings"}}
        }}}}
    }));
    let tool = live_tool("strings", &["lab"], &[("1", &hash_of(&spec))]);
    let mut world = world_with(&["lab"]);
    world.tools.push(tool.clone());
    // missing: created
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    let pipeline = plan
        .plan
        .items
        .iter()
        .find(|item| item.item.kind == ItemKind::Pipeline)
        .expect("a pipeline");
    assert_eq!(pipeline.state, ItemState::Create);
    // existing and identical: unchanged
    let mut live = thorium::models::Pipeline {
        group: "lab".to_owned(),
        name: "scan".to_owned(),
        creator: "dev".to_owned(),
        order: vec![vec!["strings".to_owned()]],
        sla: 100,
        triggers: std::collections::HashMap::new(),
        description: None,
        bans: std::collections::HashMap::new(),
        stage_tools: std::collections::BTreeMap::from([("strings".to_owned(), tool.id)]),
    };
    world.pipelines.push(live.clone());
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    let pipeline = plan
        .plan
        .items
        .iter()
        .find(|item| item.item.kind == ItemKind::Pipeline)
        .expect("a pipeline");
    assert_eq!(pipeline.state, ItemState::Unchanged);
    // existing with another SLA: updated
    live.sla = 5;
    world.pipelines = vec![live];
    let plan = build_plan(&manifest, &world, &options(ConflictPolicy::Overwrite));
    let pipeline = plan
        .plan
        .items
        .iter()
        .find(|item| item.item.kind == ItemKind::Pipeline)
        .expect("a pipeline");
    assert_eq!(pipeline.state, ItemState::Update);
    assert!(
        pipeline
            .conflict
            .as_ref()
            .expect("conflict")
            .detail
            .contains("sla")
    );
}

/// A pipeline stage naming a tool that is nowhere is an error naming the stage
#[test]
fn a_pipeline_stage_without_a_tool_is_an_error() {
    // the stage's tool is neither in the manifest nor in Thorium
    let manifest = manifest(&json!({
        "name": "t",
        "pipelines": {"scan": {"versions": {"latest": {
            "config": {"group": "lab", "name": "scan", "order": [["ghost"]]},
            "stage_tools": {"ghost": {"tool": "ghost"}}
        }}}}
    }));
    let plan = build_plan(
        &manifest,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    assert!(
        plan.errors
            .iter()
            .any(|e| e.contains("stage ghost") && e.contains("ghost")),
        "{:?}",
        plan.errors
    );
}

/// Network policies are created when missing and an update leaves them out
#[test]
fn network_policies_are_planned() {
    // one policy named by the manifest
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])},
        "network_policies": [{"name": "allow-all", "groups": ["lab"]}]
    }));
    let plan = build_plan(
        &manifest,
        &world_with(&["lab"]),
        &options(ConflictPolicy::Overwrite),
    );
    assert_eq!(plan.new_policies.len(), 1);
    assert_eq!(plan.plan.policies[0].state, PolicyState::Create);
    assert_eq!(plan.plan.policies[0].name, "allow-all");
}

/// A toolset in the manifest and a toolset entity of another kind don't mix up
#[test]
fn entity_lookup_is_by_kind_and_group() {
    // an agent named like the toolset, in the same group
    let mut world = world_with(&["lab"]);
    world.agents.push(entity(
        "kit",
        EntityKinds::Agent,
        EntityMetadata::Other,
        &["lab"],
    ));
    let groups = vec!["lab".to_owned()];
    assert!(
        world
            .entity(EntityKinds::ToolSet, "kit", &groups)
            .expect("lookup")
            .is_none()
    );
    assert!(
        world
            .entity(EntityKinds::Agent, "kit", &groups)
            .expect("lookup")
            .is_some()
    );
    assert!(
        world
            .entity(EntityKinds::Agent, "kit", &["other".to_owned()])
            .expect("lookup")
            .is_none()
    );
}

/// A pipeline name longer than the API allows is refused before anything is written
#[test]
fn a_long_pipeline_name_is_refused() {
    // a pipeline with a 26 character name
    let name = "p".repeat(26);
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])},
        "pipelines": {name.clone(): {"versions": {"latest": {
            "config": {"group": "lab", "name": name.clone(), "order": [["a"]]},
            "stage_tools": {"a": {"tool": "a"}}
        }}}}
    }));
    let world = world_with(&["lab"]);
    let opts = options(ConflictPolicy::Overwrite);
    let plan = build_plan(&manifest, &world, &opts);
    let problems = preflight::check(&manifest, &world, &opts, &plan);
    assert!(
        problems.iter().any(|p| p.contains("longer than 25")),
        "{problems:?}"
    );
}
