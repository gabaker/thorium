//! Tests for the updates an import builds for items that exist, and for tool root changes

use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thorium::models::{
    EntityKinds, EntityMetadata, EntityMetadataUpdate, HarnessEnvVar, Pipeline, PipelineRequest,
    ToolKind, WorkflowCatalogEntity,
};
use uuid::Uuid;

use super::{live_tool, manifest, now};
use crate::handlers::agents::fixtures::{agent, entity};
use crate::handlers::toolbox::import::classify::{
    live_workflow_view, parse_harness_env, root_change, workflow_view,
};
use crate::handlers::toolbox::import::journal::RootUndo;
use crate::handlers::toolbox::import::rollback::{pipeline_update, request_of, workflow_update};
use crate::handlers::toolbox::v2::types::{ToolManifest, WorkflowManifest};

/// Build a pipeline that exists
///
/// # Arguments
///
/// * `stage_tools` - The tool behind each stage
fn pipeline(stage_tools: &[(&str, Uuid)]) -> Pipeline {
    // one stage per tool, in the order given, with a one day SLA
    Pipeline {
        group: "lab".to_owned(),
        name: "scan".to_owned(),
        creator: "dev".to_owned(),
        order: stage_tools
            .iter()
            .map(|(stage, _)| vec![(*stage).to_owned()])
            .collect(),
        sla: 86_400,
        triggers: HashMap::new(),
        description: None,
        bans: HashMap::new(),
        stage_tools: stage_tools
            .iter()
            .map(|(stage, id)| ((*stage).to_owned(), *id))
            .collect(),
    }
}

/// A pipeline that already matches its request needs no update
#[test]
fn a_matching_pipeline_needs_no_update() {
    // the request that would create the pipeline as it is
    let tool = Uuid::new_v4();
    let live = pipeline(&[("a", tool)]);
    assert!(pipeline_update(&live, &request_of(&live)).is_none());
}

/// Each difference between a pipeline and its request becomes one field of the update
#[test]
fn pipeline_differences_become_update_fields() {
    // a request with another order, SLA, description, trigger, and stage tools
    let (old, new, extra) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let live = pipeline(&[("a", old), ("b", old)]);
    let mut wanted = request_of(&live);
    wanted.order = json!([["a"], ["c"]]);
    wanted.sla = Some(5);
    wanted.description = Some("scans".to_owned());
    wanted.stage_tools = BTreeMap::from([("a".to_owned(), new), ("c".to_owned(), extra)]);
    let update = pipeline_update(&live, &wanted).expect("an update");
    assert_eq!(update.order, Some(json!([["a"], ["c"]])));
    assert_eq!(update.sla, Some(5));
    assert_eq!(update.description.as_deref(), Some("scans"));
    // the changed and new stage tools are set, and the stage that is gone loses its tool
    assert_eq!(
        update.add_stage_tools,
        BTreeMap::from([("a".to_owned(), new), ("c".to_owned(), extra)])
    );
    assert_eq!(update.remove_stage_tools, ["b"]);
    // clearing the description is its own field
    let mut cleared = live.clone();
    cleared.description = Some("old".to_owned());
    let update = pipeline_update(&cleared, &request_of(&live)).expect("an update");
    assert!(update.clear_description && update.description.is_none());
}

/// A request built from a pipeline carries its SLA, so an unset SLA never reads as a change
#[test]
fn a_request_built_from_a_pipeline_carries_its_sla() {
    // the request has the live SLA, and an unset SLA in a request means "leave it"
    let live = pipeline(&[("a", Uuid::new_v4())]);
    let request = request_of(&live);
    assert_eq!(request.sla, Some(86_400));
    let unset = PipelineRequest {
        sla: None,
        ..request
    };
    assert!(pipeline_update(&live, &unset).is_none());
}

/// Build a workflow that exists with agents and an entry agent
///
/// # Arguments
///
/// * `names` - The names of its agents
/// * `entry` - The name of its entry agent
fn live_workflow(names: &[&str], entry: Option<&str>) -> (thorium::models::Entity, Vec<Uuid>) {
    // an agent entity per name, in the group `lab`
    let agents: Vec<thorium::models::Entity> =
        names.iter().map(|name| agent(name, |_| {})).collect();
    let ids = agents.iter().map(|agent| agent.id).collect();
    let entry_agent = entry.and_then(|entry| {
        agents
            .iter()
            .find(|agent| agent.name == entry)
            .map(|agent| agent.id)
    });
    let meta = WorkflowCatalogEntity {
        entry_agent,
        auto_instance: true,
        agents,
        modified: now(),
        ..WorkflowCatalogEntity::default()
    };
    (
        entity(
            "flow",
            EntityKinds::WorkflowCatalog,
            EntityMetadata::WorkflowCatalog(meta),
            &["lab"],
        ),
        ids,
    )
}

/// Build a workflow of a manifest
///
/// # Arguments
///
/// * `document` - The workflow's JSON
fn workflow_of(document: &serde_json::Value) -> WorkflowManifest {
    // parse it with the manifest's types
    serde_json::from_value(document.clone()).expect("a workflow")
}

/// A workflow that already matches its manifest needs no update
#[test]
fn a_matching_workflow_needs_no_update() {
    // a workflow of two agents with an entry agent, and the same in the manifest
    let (live, ids) = live_workflow(&["a", "b"], Some("a"));
    let wanted = workflow_view(
        "flow",
        &workflow_of(&json!({"groups": ["lab"], "agents": ["b", "a"], "entry_agent": "a"})),
    );
    let known = BTreeMap::from([("a".to_owned(), ids[0]), ("b".to_owned(), ids[1])]);
    assert_eq!(live_workflow_view(&live), wanted);
    assert!(
        workflow_update(&live, &wanted, &known)
            .expect("update")
            .is_none()
    );
}

/// Agents, the entry agent, settings, and groups that differ become one update
#[test]
fn workflow_differences_become_one_update() {
    // the workflow runs a and b from a; the manifest runs b and c from c, in another group
    let (live, ids) = live_workflow(&["a", "b"], Some("a"));
    let c = Uuid::new_v4();
    let wanted = workflow_view(
        "flow",
        &workflow_of(&json!({
            "groups": ["lab", "extra"], "description": "runs things", "agents": ["b", "c"],
            "entry_agent": "c", "limits": {"max_alive_sub_agents": 3}, "auto_instance": false,
            "model_auth": ["Group"]
        })),
    );
    let known = BTreeMap::from([
        ("a".to_owned(), ids[0]),
        ("b".to_owned(), ids[1]),
        ("c".to_owned(), c),
    ]);
    let update = workflow_update(&live, &wanted, &known)
        .expect("a valid update")
        .expect("an update");
    assert_eq!(update.add_groups, ["extra"]);
    assert_eq!(update.description.as_deref(), Some("runs things"));
    let Some(EntityMetadataUpdate::WorkflowCatalog {
        entry_agent,
        auto_instance,
        limits,
        model_auth,
        add_agents,
        remove_agents,
        ..
    }) = update.metadata
    else {
        panic!("expected a workflow update");
    };
    assert_eq!(entry_agent, Some(c));
    assert_eq!(auto_instance, Some(false));
    assert_eq!(limits.expect("limits").max_alive_sub_agents, Some(3));
    assert_eq!(model_auth.expect("model auth").levels.len(), 1);
    assert_eq!(add_agents, [c]);
    assert_eq!(remove_agents, [ids[0]]);
    // a workflow with no entry agent in the manifest clears the entry agent
    let wanted = workflow_view(
        "flow",
        &workflow_of(&json!({"groups": ["lab"], "agents": ["a", "b"]})),
    );
    let update = workflow_update(&live, &wanted, &known)
        .expect("a valid update")
        .expect("an update");
    let Some(EntityMetadataUpdate::WorkflowCatalog {
        clear_entry_agent, ..
    }) = update.metadata
    else {
        panic!("expected a workflow update");
    };
    assert_eq!(clear_entry_agent, Some(true));
    // an agent nothing knows is an error naming it
    let wanted = workflow_view(
        "flow",
        &workflow_of(&json!({"groups": ["lab"], "agents": ["a", "ghost"]})),
    );
    let err = workflow_update(&live, &wanted, &known).expect_err("an unknown agent");
    assert!(err.contains("ghost"), "{err}");
}

/// Build a tool of a manifest from JSON
///
/// # Arguments
///
/// * `document` - The tool's JSON
fn tool_of(document: &serde_json::Value) -> ToolManifest {
    // parse it with the manifest's types
    manifest(&json!({"name": "t", "tools": {"x": document}})).tools["x"].clone()
}

/// A tool whose description, groups, and tags match has no root change
#[test]
fn a_matching_tool_root_has_no_change() {
    // the manifest sets nothing the live tool lacks
    let mut live = live_tool("x", &["lab", "other"], &[("1", "h")]);
    live.description = Some("a tool".to_owned());
    live.tags = BTreeMap::from([("kind".to_owned(), BTreeSet::from(["scan".to_owned()]))]);
    let tool = tool_of(&json!({
        "groups": ["lab"], "description": "a tool", "tags": {"kind": ["scan"]}, "versions": {}
    }));
    assert!(root_change(&tool, &live).expect("a change").is_empty());
}

/// Groups, tags, and the description that are missing are added, and others are left alone
#[test]
fn a_tool_root_gains_what_the_manifest_adds() {
    // the manifest adds a group, a tag value, and a description
    let live = live_tool("x", &["lab"], &[("1", "h")]);
    let tool = tool_of(&json!({
        "groups": ["lab", "more"], "description": "new", "tags": {"kind": ["scan", "static"]},
        "versions": {}
    }));
    let change = root_change(&tool, &live).expect("a change");
    assert_eq!(change.description.as_deref(), Some("new"));
    assert_eq!(change.add_groups, ["more"]);
    assert_eq!(
        change.add_tags["kind"],
        BTreeSet::from(["scan".to_owned(), "static".to_owned()])
    );
    assert!(!change.lines().is_empty());
    // what undoes the change restores the old description and removes what was added
    let undo = RootUndo::from_change(&live, &change);
    assert_eq!(undo.description, Some(None));
    assert_eq!(undo.remove_groups, ["more"]);
    assert_eq!(undo.remove_tags, change.add_tags);
}

/// A harness's env and disabled models are the manifest's when it sets them
#[test]
fn a_harness_root_follows_the_manifest() {
    // a harness that disables one model and exports nothing
    let mut live = live_tool("h", &["lab"], &[("1", "h")]);
    live.kind = ToolKind::Harness;
    live.disabled_models = BTreeSet::from(["old".to_owned()]);
    let tool = tool_of(&json!({
        "groups": ["lab"], "harness_env": ["OpenAiApiKey=LITELLM_API_KEY"],
        "disabled_models": ["new"], "versions": {}
    }));
    let change = root_change(&tool, &live).expect("a change");
    let env = change.harness_env.expect("an env");
    assert!(env.export.contains(&HarnessEnvVar::OpenAiApiKey));
    assert_eq!(change.disable_models, BTreeSet::from(["new".to_owned()]));
    assert_eq!(change.enable_models, BTreeSet::from(["old".to_owned()]));
    // an image tool has no harness settings to compare
    let image = live_tool("i", &["lab"], &[("1", "h")]);
    assert!(root_change(&tool, &image).expect("a change").is_empty());
    // a value that isn't a harness env value is an error
    let bad = tool_of(&json!({"groups": ["lab"], "harness_env": ["Nope"], "versions": {}}));
    assert!(root_change(&bad, &live).is_err());
    assert!(parse_harness_env(&["Nope".to_owned()]).is_err());
}
