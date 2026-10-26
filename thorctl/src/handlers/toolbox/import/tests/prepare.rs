//! Tests for shaping a manifest before planning: override, selection, and what to read

use clap::Parser;
use serde_json::json;
use std::collections::BTreeSet;

use super::{image_spec, image_spec_depending_on, manifest, tool_json};
use crate::args::toolbox::ImportToolbox;
use crate::handlers::toolbox::import::prepare::{
    override_group, referenced_tools, rewrite_images, select, wanted,
};

/// Parse the import settings of a command line
///
/// # Arguments
///
/// * `args` - The arguments after `import toolbox.json`
fn opts(args: &[&str]) -> crate::args::toolbox::ImportOpts {
    // the manifest is a placeholder; selection never reads it
    ImportToolbox::try_parse_from(
        ["import", "toolbox.json"]
            .into_iter()
            .chain(args.iter().copied()),
    )
    .expect("parse")
    .opts
}

/// A manifest with two tools, a toolset, an agent, a workflow, and two pipelines
fn big() -> crate::handlers::toolbox::v2::types::ToolboxManifestV2 {
    // b depends on a; the pipeline `pb` runs b and `pc` runs c
    manifest(&json!({
        "name": "t",
        "groups": {"old": {"description": "the old group"}},
        "tools": {
            "a": tool_json(&["old"], &[("1", image_spec("a:1"))]),
            "b": tool_json(&["old"], &[("1", image_spec_depending_on("b:1", &["ref:tool/a"]))]),
            "c": tool_json(&["old"], &[("1", image_spec("c:1"))])
        },
        "toolsets": {"kit": {"groups": ["old"], "tools": [{"tool": "b"}]}},
        "agents": {"ag": {"groups": ["old"], "toolsets": [{"toolset": "kit"}]}},
        "workflows": {"wf": {"groups": ["old"], "agents": ["ag"], "entry_agent": "ag"}},
        "pipelines": {
            "pb": {"versions": {"latest": {
                "config": {"group": "old", "name": "pb", "order": [["b"]]},
                "stage_tools": {"b": {"tool": "b"}}
            }}},
            "pc": {"versions": {"latest": {
                "config": {"group": "old", "name": "pc", "order": [["c"]]},
                "stage_tools": {"c": {"tool": "c"}}
            }}}
        },
        "network_policies": [{"name": "unused", "groups": ["old"]}]
    }))
}

/// The group override puts every kind of item in one group and keeps one group entry
#[test]
fn override_group_moves_everything() {
    // override the group of everything
    let mut manifest = big();
    override_group(&mut manifest, "new");
    assert_eq!(manifest.tools["a"].groups, ["new"]);
    assert_eq!(manifest.toolsets["kit"].groups, ["new"]);
    assert_eq!(manifest.agents["ag"].groups, ["new"]);
    assert_eq!(manifest.workflows["wf"].groups, ["new"]);
    assert_eq!(manifest.network_policies[0].groups, ["new"]);
    // pipelines are in the group their config names
    let config = manifest.pipelines["pb"].versions["latest"]
        .config
        .as_ref()
        .expect("config");
    assert_eq!(config.group, "new");
    // the group map holds only the new group, keeping a description
    assert_eq!(manifest.groups.keys().collect::<Vec<_>>(), ["new"]);
    assert_eq!(
        manifest.groups["new"].description.as_deref(),
        Some("the old group")
    );
    assert_eq!(manifest.named_groups(), ["new"]);
}

/// Selecting a pipeline keeps its tools and what they depend on, and nothing else
#[test]
fn selecting_a_pipeline_keeps_what_it_needs() {
    // select pb: it needs b, which needs a
    let mut manifest = big();
    select(&mut manifest, &opts(&["-p", "pb"])).expect("select");
    let tools: Vec<&String> = manifest.tools.keys().collect();
    assert_eq!(tools, ["a", "b"]);
    assert_eq!(manifest.pipelines.keys().collect::<Vec<_>>(), ["pb"]);
    // the toolset, agent, and workflow weren't asked for
    assert!(
        manifest.toolsets.is_empty() && manifest.agents.is_empty() && manifest.workflows.is_empty()
    );
    // the policy no kept tool names is dropped
    assert!(manifest.network_policies.is_empty());
}

/// --only takes a kind and a name, and selects a workflow with everything under it
#[test]
fn only_selects_any_kind() {
    // the workflow needs its agent, which needs the toolset, which needs b and a
    let mut manifest = big();
    select(&mut manifest, &opts(&["--only", "workflow/wf"])).expect("select");
    assert_eq!(manifest.tools.keys().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(
        manifest.toolsets.len() + manifest.agents.len() + manifest.workflows.len(),
        3
    );
    assert!(manifest.pipelines.is_empty());
}

/// Naming an item the toolbox doesn't have is an error naming it
#[test]
fn selecting_a_missing_item_is_an_error() {
    // a tool and an agent that aren't there
    let mut manifest = big();
    let err = select(
        &mut manifest,
        &opts(&["-t", "ghost", "--only", "agent/nobody"]),
    )
    .expect_err("missing items");
    assert!(
        err.contains("tool ghost") && err.contains("agent nobody"),
        "{err}"
    );
}

/// --no-pipelines drops pipelines, and selecting one of them then fails
#[test]
fn no_pipelines_drops_pipelines() {
    // nothing selected: everything but pipelines stays
    let mut manifest = big();
    select(&mut manifest, &opts(&["--no-pipelines"])).expect("select");
    assert!(manifest.pipelines.is_empty());
    assert_eq!(manifest.tools.len(), 3);
    // a pipeline can't be selected once they are dropped
    let mut manifest = big();
    assert!(select(&mut manifest, &opts(&["--no-pipelines", "-p", "pb"])).is_err());
}

/// With no selection every item stays
#[test]
fn no_selection_keeps_everything() {
    // nothing is cut
    let mut manifest = big();
    select(&mut manifest, &opts(&[])).expect("select");
    assert_eq!(manifest.tools.len(), 3);
    assert_eq!(manifest.pipelines.len(), 2);
    assert_eq!(manifest.network_policies.len(), 1);
}

/// What is read from Thorium covers the manifest's groups and every tool it refers to
#[test]
fn wanted_covers_groups_and_references() {
    // a toolset names a tool the manifest doesn't define
    let manifest = manifest(&json!({
        "name": "t",
        "tools": {"a": tool_json(&["g1"], &[("1", image_spec_depending_on("a:1", &["ref:tool/outside"]))])},
        "toolsets": {"kit": {"groups": ["g2"], "tools": [{"tool": "elsewhere"}]}},
        "network_policies": [{"name": "p", "groups": ["g3"]}]
    }));
    let wanted = wanted(&manifest);
    let groups: Vec<&str> = wanted.groups.iter().map(String::as_str).collect();
    assert_eq!(groups, ["g1", "g2", "g3"]);
    let tools: BTreeSet<String> = ["a", "elsewhere", "outside"].map(str::to_owned).into();
    assert_eq!(wanted.tools, tools);
    assert_eq!(referenced_tools(&manifest), tools);
    assert!(wanted.entities && wanted.policies && !wanted.pipelines);
}

/// Bundled images are pointed at the registry path with their tag kept
#[test]
fn bundled_images_are_rewritten() {
    // images with a tag, with a registry port, and with none
    let mut manifest = manifest(&json!({
        "name": "t",
        "tools": {
            "a": tool_json(&["g"], &[("1", image_spec("hub.io/x/a:v1"))]),
            "b": tool_json(&["g"], &[("1", image_spec("localhost:5000/b"))])
        }
    }));
    let count = rewrite_images(&mut manifest, "reg.local/base/");
    assert_eq!(count, 2);
    let image = |name: &str| {
        manifest.tools[name].versions["1"]
            .spec
            .as_ref()
            .expect("spec")["Image"]["environment"]["K8s"]["image"]
            .as_str()
            .expect("image")
            .to_owned()
    };
    assert_eq!(image("a"), "reg.local/base/g/a:v1");
    assert_eq!(image("b"), "reg.local/base/g/b:latest");
}
