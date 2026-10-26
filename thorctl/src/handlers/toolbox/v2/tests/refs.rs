//! Tests for symbolic references and the spec id slot table

use serde_json::json;
use std::collections::BTreeSet;
use thorium::models::{
    AgenticTool, AgenticToolBacking, ContainerToolBacking, HarnessSpec,
    ToolChildrenDependencySettings, ToolFileSystemDependencySettings, ToolSpec, WorkloadSpec,
};
use uuid::Uuid;

use super::super::refs::{
    Ref, SLOTS, SlotKind, SlotValue, find_slots, resolve_slots, unresolve_slots, uuid_paths,
};
use super::image_spec_depending_on;

/// Reference text parses, prints the way it was written, and is checked against its slot
#[test]
fn references_parse_and_print() {
    // every form reads back as it was written
    for text in [
        "ref:tool/clamav",
        "ref:tool/clamav@1.2.3",
        "ref:toolset/kit",
        "ref:agent/triage",
    ] {
        let reference: Ref = text.parse().expect("parse");
        assert_eq!(reference.to_string(), text);
    }
    // malformed references are refused with a reason
    for text in [
        "tool/x",
        "ref:tool",
        "ref:tool/",
        "ref:other/x",
        "ref:toolset/x@1",
        "ref:tool/a b",
    ] {
        assert!(text.parse::<Ref>().is_err(), "{text}");
    }
    // a tool slot refuses a pinned reference and an instance slot refuses an unpinned one
    let spec = image_spec_depending_on(&["ref:tool/x@1", "ref:tool/y", "not a ref"]);
    let (found, problems) = find_slots(&spec);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "Image.data.dependencies.results.tools[1]");
    assert_eq!(found[0].kind, SlotKind::ToolId);
    assert_eq!(problems.len(), 2);
    assert!(problems[0].message.contains("drop the @LABEL"));
    let literal = Uuid::new_v4().to_string();
    let (found, _) = find_slots(&image_spec_depending_on(&[&literal]));
    assert!(matches!(found[0].value, SlotValue::Uuid(_)));
}

/// Resolving references swaps them for ids, and unresolving swaps them back
#[test]
fn references_resolve_and_unresolve() {
    // resolve each reference to a fresh id
    let mut spec = image_spec_depending_on(&["ref:tool/a", "ref:tool/b"]);
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    let problems = resolve_slots(&mut spec, |reference, _| match reference {
        Ref::Tool { name, .. } if name == "a" => Ok(ids[0]),
        Ref::Tool { name, .. } if name == "b" => Ok(ids[1]),
        other => Err(format!("unknown {other}")),
    });
    assert_eq!(problems.len(), 0);
    // the spec now parses as the API's own type
    ToolSpec::parse_request(&spec.to_string()).expect("a resolved spec parses");
    // unresolving restores the references
    unresolve_slots(&mut spec, |id, _| {
        let name = if id == ids[0] { "a" } else { "b" };
        Some(Ref::Tool {
            name: name.to_owned(),
            label: None,
        })
    });
    assert_eq!(
        spec["Image"]["data"]["dependencies"]["results"]["tools"],
        json!(["ref:tool/a", "ref:tool/b"])
    );
    // a reference the resolver can't resolve is reported at its path
    let mut spec = image_spec_depending_on(&["ref:tool/gone"]);
    let problems = resolve_slots(&mut spec, |_, _| Err("no such tool".to_owned()));
    assert_eq!(problems[0].path, "Image.data.dependencies.results.tools[0]");
}

/// Build a spec with every id slot the model has filled with an id
///
/// # Arguments
///
/// * `seed` - The id to put in every slot
fn specs_with_every_id(seed: Uuid) -> Vec<ToolSpec> {
    // dependencies that name the seed tool in each list
    let mut workload = WorkloadSpec::default();
    workload.data.dependencies.results.tools = vec![seed];
    workload.data.dependencies.children = ToolChildrenDependencySettings {
        tools: vec![seed],
        ..ToolChildrenDependencySettings::default()
    };
    workload.data.dependencies.filesystems = ToolFileSystemDependencySettings {
        tools: vec![seed],
        ..ToolFileSystemDependencySettings::default()
    };
    // an agentic tool backed by a container instance
    let agentic = AgenticTool {
        backing: AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: seed,
            ..ContainerToolBacking::default()
        }),
        ..AgenticTool::default()
    };
    // a harness running the same workload
    let harness = HarnessSpec {
        workload: workload.clone(),
        ..HarnessSpec::default()
    };
    vec![
        ToolSpec::Image(workload),
        ToolSpec::Agentic(agentic),
        ToolSpec::Harness(harness),
    ]
}

/// The slot table lists every place a tool spec holds an id, and every row is real
#[test]
fn slot_table_covers_every_id_in_a_spec() {
    // find every id in specs whose id fields are all filled
    let mut found: BTreeSet<String> = BTreeSet::new();
    for spec in specs_with_every_id(Uuid::new_v4()) {
        found.extend(uuid_paths(&serde_json::to_value(spec).expect("serialize")));
    }
    // default image and harness specs hold no id at all
    for spec in [
        ToolSpec::Image(WorkloadSpec::default()),
        ToolSpec::Harness(HarnessSpec::default()),
    ] {
        assert_eq!(
            uuid_paths(&serde_json::to_value(spec).expect("serialize")).len(),
            0
        );
    }
    // the ids found are exactly the table's slots: a new id field makes this fail until it is added
    let table: BTreeSet<String> = SLOTS.iter().map(|slot| slot.path.to_owned()).collect();
    assert_eq!(found, table);
}
