//! Unit tests for the pipeline edit and update commands

use std::collections::{BTreeMap, HashMap};

use clap::Parser;
use serde_json::Value;
use thorium::models::{EventTrigger, Pipeline};
use uuid::Uuid;

use super::{PipelineData, changed_tools, flag_patches, merge_view, parse_trigger};
use crate::args::pipelines::UpdatePipeline;
use crate::handlers::pipelines::view::PipelineView;
use crate::utils::patch::merge_patch;
use crate::utils::pipelines::stage_tools::StageToolRef;

/// Build a tool ref
///
/// # Arguments
///
/// * `tool` - The tool's name
/// * `version` - The pinned version, if any
fn tool(tool: &str, version: Option<&str>) -> StageToolRef {
    StageToolRef {
        tool: tool.to_owned(),
        version: version.map(ToOwned::to_owned),
    }
}

/// Build a pipeline of two stages whose tools are known by name and id
///
/// # Arguments
///
/// * `a` - The id of stage `a`'s tool
/// * `b` - The id of stage `b`'s tool
fn data(a: Uuid, b: Uuid) -> PipelineData {
    PipelineData {
        pipeline: Pipeline {
            group: "g".to_owned(),
            name: "p".to_owned(),
            creator: "tester".to_owned(),
            order: vec![vec!["a".to_owned()], vec!["b".to_owned()]],
            sla: 3600,
            triggers: HashMap::new(),
            description: Some("before".to_owned()),
            bans: HashMap::new(),
            stage_tools: BTreeMap::from([("a".to_owned(), a), ("b".to_owned(), b)]),
        },
        tools: BTreeMap::from([
            ("a".to_owned(), tool("scan", None)),
            ("b".to_owned(), tool("sort", Some("1.0.0"))),
        ]),
    }
}

/// Parse an update command line
///
/// # Arguments
///
/// * `flags` - The arguments after `update orbit`
fn update(flags: &[&str]) -> UpdatePipeline {
    let mut argv = vec!["update", "orbit"];
    argv.extend_from_slice(flags);
    UpdatePipeline::try_parse_from(argv).unwrap()
}

/// Apply the typed flags of an update command to the view of a pipeline
///
/// # Arguments
///
/// * `data` - The pipeline to start from
/// * `flags` - The arguments after `update orbit`
fn apply(data: &PipelineData, flags: &[&str]) -> PipelineView {
    // patch the document the way the shared flow does
    let mut document =
        serde_json::to_value(PipelineView::new(&data.pipeline, &data.tools)).unwrap();
    for patch in flag_patches(&update(flags)).unwrap() {
        merge_patch(
            &mut document,
            &serde_json::from_str::<Value>(&patch).unwrap(),
        );
    }
    serde_json::from_value(document).unwrap()
}

/// The view of a pipeline names each stage's tool and carries its settings
#[test]
fn view_names_tools() {
    let data = data(Uuid::new_v4(), Uuid::new_v4());
    let view = PipelineView::new(&data.pipeline, &data.tools);
    assert_eq!(view.sla, Some(3600));
    assert_eq!(view.stage_tools["b"], tool("sort", Some("1.0.0")));
    assert_eq!(view.order.len(), 2);
    view.check().unwrap();
}

/// `--stage-tool` replaces a stage's tool, and a tool without a version follows the latest
#[test]
fn stage_tool_flag_replaces_the_tool() {
    let data = data(Uuid::new_v4(), Uuid::new_v4());
    // a pinned stage set without a version no longer holds its old pin
    let view = apply(&data, &["--stage-tool", "b=sort"]);
    assert_eq!(view.stage_tools["b"], tool("sort", None));
    // a version pins it
    let view = apply(&data, &["--stage-tool", "a=scan@2.0.0"]);
    assert_eq!(view.stage_tools["a"], tool("scan", Some("2.0.0")));
    // other stages are untouched
    assert_eq!(view.stage_tools["b"], tool("sort", Some("1.0.0")));
}

/// A new stage needs `--order` and `--stage-tool` together
#[test]
fn new_stages_come_with_their_tools() {
    let data = data(Uuid::new_v4(), Uuid::new_v4());
    // both flags make a complete view
    let view = apply(
        &data,
        &["--order", "a,b;c", "--stage-tool", "c=extra@3.0.0"],
    );
    view.check().unwrap();
    assert_eq!(view.order[0], ["a", "b"]);
    assert_eq!(view.stage_tools["c"], tool("extra", Some("3.0.0")));
    // the order alone leaves the new stage without a tool, which check refuses
    let view = apply(&data, &["--order", "a;b;c"]);
    let err = view.check().unwrap_err();
    assert!(err.to_string().contains("no tool is set for: c"), "{err}");
}

/// Dropping a stage means dropping it from the order and its tool together
#[test]
fn removed_stages_drop_their_tools() {
    let data = data(Uuid::new_v4(), Uuid::new_v4());
    let view = apply(&data, &["--order", "a", "--remove-stage-tool", "b"]);
    view.check().unwrap();
    assert!(!view.stage_tools.contains_key("b"));
    // removing only the tool leaves a stage without one
    let view = apply(&data, &["--remove-stage-tool", "b"]);
    assert!(view.check().is_err());
}

/// The scalar flags, triggers and description clearing change the view
#[test]
fn scalar_flags_change_the_view() {
    let data = data(Uuid::new_v4(), Uuid::new_v4());
    let view = apply(
        &data,
        &["--sla", "42", "-d", "123", "--add-trigger", "t=NewSample"],
    );
    assert_eq!(view.sla, Some(42));
    // a numeric description stays text
    assert_eq!(view.description.as_deref(), Some("123"));
    assert_eq!(view.triggers["t"], EventTrigger::NewSample);
    // a trigger can be removed again, and the description cleared
    let mut data = data;
    data.pipeline
        .triggers
        .insert("old".to_owned(), EventTrigger::NewSample);
    let view = apply(&data, &["--remove-trigger", "old", "--clear-description"]);
    assert!(view.triggers.is_empty());
    assert_eq!(view.description, None);
}

/// Only stages whose named tool differs are looked up
#[test]
fn only_changed_tools_are_resolved() {
    let data = data(Uuid::new_v4(), Uuid::new_v4());
    let mut view = PipelineView::new(&data.pipeline, &data.tools);
    assert!(changed_tools(&data, &view).is_empty());
    // re-pinning one stage changes only that stage
    view.stage_tools
        .insert("b".to_owned(), tool("sort", Some("2.0.0")));
    let changed = changed_tools(&data, &view);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed["b"], tool("sort", Some("2.0.0")));
}

/// A view becomes a merge view that keeps the stored id of every unchanged stage
#[test]
fn merge_view_keeps_stored_ids() {
    let (a, b, new) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let data = data(a, b);
    let mut view = PipelineView::new(&data.pipeline, &data.tools);
    view.stage_tools
        .insert("b".to_owned(), tool("sort", Some("2.0.0")));
    // the SLA left out keeps the stored one
    view.sla = None;
    let merged = merge_view(&data, view, &BTreeMap::from([("b".to_owned(), new)]));
    assert_eq!(merged.stage_tools["a"], a);
    assert_eq!(merged.stage_tools["b"], new);
    assert_eq!(merged.sla, 3600);
}

/// A trigger argument is `NAME=JSON` and must be a trigger the API knows
#[test]
fn triggers_are_parsed_and_checked() {
    let (name, trigger) = parse_trigger("t=NewSample").unwrap();
    assert_eq!(name, "t");
    assert_eq!(trigger, EventTrigger::NewSample);
    // a JSON object works too
    assert!(
        parse_trigger(r#"t={"Tag": {"tag_types": ["Files"], "required": {}, "not": {}}}"#).is_ok()
    );
    // a missing name, a missing =, and an unknown trigger are refused
    assert!(parse_trigger("=NewSample").is_err());
    assert!(parse_trigger("NewSample").is_err());
    let err = parse_trigger("t=Bogus").unwrap_err();
    assert!(
        err.to_string().contains("Trigger 't' is not valid"),
        "{err}"
    );
}
