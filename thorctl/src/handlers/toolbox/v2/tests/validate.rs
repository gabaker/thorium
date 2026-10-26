//! Tests for validating manifests

use serde_json::{Value, json};
use uuid::Uuid;

use super::super::graph;
use super::super::load::resolve_sources;
use super::super::plan_types::{ItemKind, ItemRef};
use super::super::types::ToolboxManifestV2;
use super::super::validate::{Issue, Severity, ValidationReport, validate_manifest};
use super::{
    BROKEN_V2, CLEAN_V2, REAL_TOOLBOX, SMALL_LEGACY, Scratch, assert_golden, codes, image_spec,
    image_spec_depending_on, issues_of, load, load_text, tool_of, with_tools,
};
use crate::utils::output::{OutputFormat, render};

/// A tool version with neither `spec` nor `spec_from` is a named structural error
#[tokio::test]
async fn version_without_a_spec_is_a_named_error() {
    // a version with nothing to build its spec from
    let loaded = load(&with_tools(
        &json!({"a": {"groups": ["g"], "versions": {"1": {}}}}),
    ))
    .await
    .expect("parse");
    let issues = issues_of(&loaded);
    // the error names the version and has the field in its code
    let issue = issues
        .iter()
        .find(|issue| issue.code == "missing_field:spec")
        .expect("issue");
    assert_eq!(issue.severity, Severity::Error);
    assert_eq!(issue.item, "tool a@1");
}

/// A harness that provides a tool or version the manifest lacks is refused
#[tokio::test]
async fn harness_provides_of_a_missing_tool_or_version_is_refused() {
    // a harness providing a tool the manifest doesn't define, and a version it doesn't have
    let harness = json!({
        "kind": "Harness", "groups": ["g"],
        "versions": {"1": {
            "spec": {"Harness": {"workload": image_spec("x:1")["Image"].clone()}},
            "provides": [{"tool": "ghost"}, {"tool": "real", "version": "9"}]
        }}
    });
    let real = json!({"kind": "Agentic", "groups": ["g"], "versions": {"1": {"spec": {"Agentic": {
        "function_name": "f", "model_description": "d", "input_schema": {"type": "object"},
        "backing": {"HarnessNative": {"tool": "t"}}
    }}}}});
    let loaded = load(&with_tools(&json!({"h": harness, "real": real})))
        .await
        .expect("parse");
    let issues = issues_of(&loaded);
    // both are errors with the same code, each naming what is missing
    let missing: Vec<&Issue> = issues
        .iter()
        .filter(|issue| issue.code == "missing_ref:provides")
        .collect();
    assert_eq!(missing.len(), 2, "{issues:?}");
    assert!(missing.iter().any(|issue| issue.message.contains("ghost")));
    assert!(missing.iter().any(|issue| issue.message.contains("real@9")));
}

/// A provided tool must be in every group of the harness that provides it
#[tokio::test]
async fn provided_tool_groups_must_cover_the_harness() {
    // the harness is in groups g and h but the provided tool only in g
    let harness = json!({
        "kind": "Harness", "groups": ["g", "h"],
        "versions": {"1": {
            "spec": {"Harness": {"workload": image_spec("x:1")["Image"].clone()}},
            "provides": [{"tool": "native"}]
        }}
    });
    let native = json!({"kind": "Agentic", "groups": ["g"], "versions": {"1": {"spec": {"Agentic": {
        "function_name": "f", "model_description": "d", "input_schema": {"type": "object"},
        "backing": {"HarnessNative": {"tool": "t"}}
    }}}}});
    let loaded = load(&with_tools(&json!({"h": harness, "native": native})))
        .await
        .expect("parse");
    let issues = issues_of(&loaded);
    // the group the provided tool lacks is named
    let issue = issues
        .iter()
        .find(|issue| issue.code == "invalid_value:provides.groups")
        .expect("issue");
    assert!(issue.message.contains("'h'"), "{}", issue.message);
    // providing a tool that isn't agentic, or from a non harness tool, is refused too
    let plain = json!({"groups": ["g"], "versions": {"1": {"spec": image_spec("x:1"), "provides": [{"tool": "native"}]}}});
    let loaded = load(&with_tools(
        &json!({"plain": plain, "native": tool_of(&image_spec("y:1"))}),
    ))
    .await
    .expect("parse");
    assert!(codes(&issues_of(&loaded)).contains("invalid_value:provides"));
}

/// Tools whose specs refer to each other in a loop are refused
#[tokio::test]
async fn dependency_cycle_is_refused() {
    // alpha needs beta's results and beta needs alpha's
    let loaded = load(&with_tools(&json!({
        "alpha": tool_of(&image_spec_depending_on(&["ref:tool/beta"])),
        "beta": tool_of(&image_spec_depending_on(&["ref:tool/alpha"])),
        "solo": tool_of(&image_spec_depending_on(&["ref:tool/solo"])),
    })))
    .await
    .expect("parse");
    let issues = issues_of(&loaded);
    let cycles: Vec<&Issue> = issues
        .iter()
        .filter(|issue| issue.code == "cycle:tool")
        .collect();
    // the loop and the self reference are each reported once, naming the path
    assert_eq!(cycles.len(), 2, "{issues:?}");
    assert!(
        cycles
            .iter()
            .any(|issue| issue.message.contains("alpha -> beta -> alpha"))
    );
    assert!(
        cycles
            .iter()
            .any(|issue| issue.message.contains("solo -> solo"))
    );
    assert!(cycles.iter().all(|issue| issue.severity == Severity::Error));
    // ordering the manifest into waves fails and names the stuck items
    let stuck = graph::waves(&loaded.manifest).expect_err("cycle");
    assert!(stuck.contains(&ItemRef::new(ItemKind::Tool, "alpha")));
}

/// Agents that delegate to each other in a loop are a warning, not an error
#[tokio::test]
async fn agent_cycle_is_a_warning() {
    // two agents each listing the other as a sub agent
    let document = json!({
        "name": "t",
        "agents": {
            "a": {"groups": ["g"], "sub_agents": ["b"]},
            "b": {"groups": ["g"], "sub_agents": ["a"]},
        }
    });
    let issues = issues_of(&load(&document).await.expect("parse"));
    let issue = issues
        .iter()
        .find(|issue| issue.code == "cycle:agent")
        .expect("cycle");
    assert_eq!(issue.severity, Severity::Warning);
}

/// Items are ordered into waves where each item follows what it depends on
#[tokio::test]
async fn waves_follow_dependencies() {
    // the clean fixture's tools, toolset, agent, workflow, and pipeline depend on one another
    let loaded = load_text(CLEAN_V2).await.expect("parse");
    let waves = graph::waves(&loaded.manifest).expect("acyclic");
    // find the wave of an item
    let wave_of = |kind: ItemKind, name: &str| {
        waves
            .iter()
            .position(|wave| wave.contains(&ItemRef::new(kind, name)))
            .expect("item in a wave")
    };
    // the image tool comes before the agentic tool that is backed by it, which comes before the
    // harness providing it, then the agent, then the workflow
    assert!(wave_of(ItemKind::Tool, "strings") < wave_of(ItemKind::Tool, "strings-agent"));
    assert!(wave_of(ItemKind::Tool, "strings-agent") < wave_of(ItemKind::Tool, "pal-harness"));
    assert!(wave_of(ItemKind::Tool, "pal-harness") < wave_of(ItemKind::Agent, "triage"));
    assert!(wave_of(ItemKind::Toolset, "kit") < wave_of(ItemKind::Agent, "triage"));
    assert!(wave_of(ItemKind::Agent, "triage") < wave_of(ItemKind::Workflow, "triage-flow"));
    assert!(wave_of(ItemKind::Tool, "strings") < wave_of(ItemKind::Pipeline, "strings-only"));
}

/// Every stage of a pipeline's order needs a stage tool
#[tokio::test]
async fn pipeline_stage_without_a_stage_tool_is_refused() {
    // a stage ordered but backed by nothing, and a stage tool no stage uses
    let document = json!({
        "name": "t",
        "tools": {"x": tool_of(&image_spec("x:1"))},
        "pipelines": {"p": {"versions": {"latest": {
            "config": {"group": "g", "name": "p", "order": [["one", "two"]]},
            "stage_tools": {"one": {"tool": "x"}, "extra": {"tool": "x"}}
        }}}}
    });
    let issues = issues_of(&load(&document).await.expect("parse"));
    // the missing stage is an error naming it; the unused one is a warning
    let missing = issues
        .iter()
        .find(|issue| issue.code == "missing_field:stage_tools.*")
        .expect("missing");
    assert_eq!(missing.severity, Severity::Error);
    assert!(missing.message.contains("'two'"));
    let extra = issues
        .iter()
        .find(|issue| issue.code == "invalid_value:stage_tools.*")
        .expect("extra");
    assert_eq!(extra.severity, Severity::Warning);
    // a malformed order is refused
    let document = json!({
        "name": "t",
        "pipelines": {"p": {"versions": {"latest": {"config": {"group": "g", "name": "p", "order": "one"}}}}}
    });
    assert!(
        codes(&issues_of(&load(&document).await.expect("parse"))).contains("invalid_value:order")
    );
}

/// References are resolved inside the manifest, flagged when they leave it, and refused when
/// they name a version that isn't there
#[tokio::test]
async fn references_resolve_against_the_manifest() {
    // one ref that lands, one to a tool outside the manifest, and one to a missing version
    let backing = |tool_ref: &str| {
        json!({"Agentic": {
            "function_name": "f", "model_description": "d", "input_schema": {"type": "object"},
            "backing": {"Container": {"tool_instance": tool_ref}}
        }})
    };
    let loaded = load(&with_tools(&json!({
        "base": tool_of(&image_spec("x:1")),
        "ok": tool_of(&backing("ref:tool/base@1")),
        "outside": tool_of(&backing("ref:tool/elsewhere@1")),
        "stale": tool_of(&backing("ref:tool/base@2")),
    })))
    .await
    .expect("parse");
    let issues = issues_of(&loaded);
    // the good ref raises nothing
    assert!(
        issues.iter().all(|issue| issue.item != "tool ok@1"),
        "{issues:?}"
    );
    // the outside ref is a warning and the stale one an error
    let outside = issues
        .iter()
        .find(|issue| issue.item == "tool outside@1")
        .expect("outside");
    assert_eq!(
        (outside.code.as_str(), outside.severity),
        ("unresolved_ref:tool", Severity::Warning)
    );
    let stale = issues
        .iter()
        .find(|issue| issue.item == "tool stale@1")
        .expect("stale");
    assert_eq!(
        (stale.code.as_str(), stale.severity),
        ("missing_ref:tool", Severity::Error)
    );
}

/// Every issue code can be raised
#[tokio::test]
async fn every_issue_code_is_reachable() {
    // a manifest holding at least one instance of each problem validation looks for
    let secret_spec = {
        let mut spec = image_spec("x:1");
        spec["Image"]["environment"]["K8s"]["env"] = json!({"DB_PASSWORD": "x"});
        spec["Image"]["data"]["dependencies"]["results"]["tools"] =
            json!([Uuid::new_v4().to_string(), "bogus"]);
        spec
    };
    let document = json!({
        "name": "sink",
        "groups": {"declared": {}},
        "tools": {
            "bad name": tool_of(&image_spec("x:1")),
            "nogroup": {"versions": {"1": {"spec": image_spec("x:1")}}},
            "nospec": {"groups": ["g"], "versions": {"1": {}}},
            "empty": {"groups": ["g"], "versions": {}},
            "labels": {"groups": ["g"], "latest": "nope", "kind": "Agentic", "harness_env": ["A=B"], "versions": {
                "bad label": {"spec": image_spec("x:1"), "provides": [{"tool": "empty"}]},
                "2": {"spec": image_spec("x:1"), "spec_from": "s.yaml", "build": {"build_image": true}},
            }},
            "badspec": {"groups": ["declared"], "versions": {"1": {"spec": {"Image": {"nonsense": 1}}}}},
            "secret": {"groups": ["g"], "versions": {"1": {"spec": secret_spec}}},
        },
        "toolsets": {"ts": {"groups": ["g"], "selector": "nokey", "tools": [{"tool": "empty", "version": "9"}]}},
        "agents": {"ag": {"groups": ["g"], "harness": {"tool": "secret"}, "toolsets": [{"toolset": "ghost"}], "sub_agents": ["ghost"]}},
        "workflows": {"wf": {"groups": ["g"], "agents": [], "entry_agent": "x"}},
        "pipelines": {"pl": {"versions": {
            "a": {"incomplete": true, "config": {"group": "g", "name": "pl", "order": "s"}},
            "b": {},
            "c": {"config": {"group": "g", "name": "pl", "order": [["s"]]}, "stage_tools": {"t": {"tool": "nospec"}}},
        }}},
        "builds": {"bb": {"bad label": {"build_image": true}}},
    });
    let loaded = load(&document).await.expect("parse");
    let found = codes(&issues_of(&loaded));
    // the codes from the manifest's own contents
    for expected in [
        "invalid_value:name",
        "invalid_value:version",
        "invalid_value:latest",
        "invalid_value:kind",
        "invalid_value:harness_env",
        "invalid_value:spec_from",
        "invalid_value:spec",
        "invalid_value:spec_ref",
        "invalid_value:selector",
        "invalid_value:harness",
        "invalid_value:entry_agent",
        "invalid_value:order",
        "invalid_value:provides",
        "invalid_value:stages.*",
        "missing_field:groups",
        "missing_field:versions",
        "missing_field:spec",
        "missing_field:config",
        "missing_field:agents",
        "missing_field:build.build_path",
        "missing_field:build.image_tags",
        "missing_ref:tool",
        "missing_field:stage_tools.*",
        "invalid_value:stage_tools.*",
        "unresolved_ref:toolset",
        "unresolved_ref:agent",
        "warning:uuid-literal",
        "warning:secret-env",
        "note:undeclared-group",
    ] {
        assert!(
            found.contains(expected),
            "{expected} not raised; got {found:?}"
        );
    }
    // the codes that need other inputs: a cycle, provides checks, stage checks, and the legacy skips
    let cyclic = load(&with_tools(&json!({
        "a": tool_of(&image_spec_depending_on(&["ref:tool/a"]))
    })))
    .await
    .expect("parse");
    assert!(codes(&issues_of(&cyclic)).contains("cycle:tool"));
    let legacy = load_text(SMALL_LEGACY).await.expect("parse");
    let legacy_codes = codes(&issues_of(&legacy));
    for expected in ["missing_field:image", "invalid_value:stages.*"] {
        assert!(legacy_codes.contains(expected), "{expected}");
    }
    // a format version other than 2 on a built manifest
    let mut manifest = ToolboxManifestV2::new("t");
    manifest.format_version = 7;
    let issues = validate_manifest(
        &manifest,
        &super::super::load::ConvertReport::default(),
        &[],
    );
    assert!(codes(&issues).contains("invalid_value:format_version"));
}

/// The validate output matches its golden files in each format
#[tokio::test]
async fn validate_output_matches_golden_files() {
    // validate a clean, a broken, and a legacy manifest
    for (name, text, strict) in [
        ("clean", CLEAN_V2, false),
        ("broken", BROKEN_V2, false),
        ("legacy", SMALL_LEGACY, true),
    ] {
        let loaded = load_text(text).await.expect("parse");
        let report = ValidationReport::new(&loaded, &[], strict);
        // the human report, and the JSON document for the broken one
        assert_golden(
            &format!("validate.{name}.human.golden"),
            &render(&report, OutputFormat::Human, false).expect("render"),
        );
        if name == "broken" {
            assert_golden(
                "validate.broken.json.golden",
                &render(&report, OutputFormat::Json, false).expect("render"),
            );
            assert_golden(
                "validate.broken.table.golden",
                &render(&report, OutputFormat::Table, false).expect("render"),
            );
        }
    }
    // the clean manifest is valid even strictly, and the broken one is not
    assert!(ValidationReport::new(&load_text(CLEAN_V2).await.expect("parse"), &[], true).valid);
    assert!(!ValidationReport::new(&load_text(BROKEN_V2).await.expect("parse"), &[], false).valid);
    // warnings only fail a strict run
    let legacy = load_text(SMALL_LEGACY).await.expect("parse");
    assert!(ValidationReport::new(&legacy, &[], false).valid);
    assert!(!ValidationReport::new(&legacy, &[], true).valid);
}

/// The commands that only read a manifest refuse a bad one without a config or a server
#[tokio::test]
async fn list_and_validate_refuse_bad_manifests_without_a_server() {
    use crate::args::{Args, SubCommands};
    use clap::Parser;
    // a manifest with an unknown key, in a scratch directory
    let scratch = Scratch::new();
    let bad = scratch.0.join("bad.json");
    std::fs::write(&bad, "{\"name\": \"t\", \"toolz\": {}}").expect("write");
    let bad = bad.to_string_lossy().into_owned();
    // neither command is given a config, so any attempt to connect would fail differently
    for command in ["list", "validate"] {
        let args = Args::try_parse_from([
            "thorctl",
            "--config",
            "/nonexistent/config.yml",
            "toolbox",
            command,
            &bad,
        ])
        .expect("parse");
        let SubCommands::Toolbox(toolbox) = &args.cmd else {
            panic!("not a toolbox command");
        };
        // the refusal names the unknown key
        let err = if command == "list" {
            super::super::super::handle(&args, toolbox)
                .await
                .expect_err("refused")
        } else {
            match toolbox {
                crate::args::toolbox::Toolbox::Validate(cmd) => {
                    super::super::validate::validate(cmd)
                        .await
                        .expect_err("refused")
                }
                _ => panic!("not validate"),
            }
        };
        assert!(err.to_string().contains("toolz"), "{command}: {err}");
    }
}

/// Issues loading a legacy manifest raised are reported, the skips as warnings
#[tokio::test]
async fn legacy_skips_are_warnings() {
    // load the fixture holding an image without a container image
    let loaded = load_text(SMALL_LEGACY).await.expect("parse");
    // validation turns the skips into warnings
    let issues = issues_of(&loaded);
    assert!(issues.iter().any(|issue| issue.code == "missing_field:image" && issue.severity == Severity::Warning));
    assert!(codes(&issues).contains("invalid_value:stages.*"));
}

/// A name defined in both sections of a merged manifest is an error
#[tokio::test]
async fn a_name_in_both_sections_is_an_error() {
    // the same name in `images` and `tools`
    let legacy: Value = serde_json::from_str(SMALL_LEGACY).expect("fixture");
    let document = json!({
        "name": "merged",
        "tools": {"clamav": tool_of(&image_spec("x:1"))},
        "images": {"clamav": legacy["images"]["clamav"].clone()},
    });
    let issues = issues_of(&load(&document).await.expect("parse"));
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "collision:clamav" && issue.severity == Severity::Error)
    );
}

/// A tool, toolset, workflow, or pipeline named like an entity kind is an error with the API's
/// line, and names that only contain the word are not
#[tokio::test]
async fn protected_names_are_errors() {
    // one protected and one allowed name in each section that names an entity
    let spec = image_spec("x:1");
    let pipeline = |name: &str| {
        json!({"versions": {"1": {"config": {"group": "g", "name": name, "order": [["s"]]},
            "stage_tools": {"s": {"tool": "tool-runner"}}}}})
    };
    let document = json!({
        "name": "protected",
        "tools": {"Tool": tool_of(&spec), "tool-runner": tool_of(&spec)},
        "toolsets": {"tool_sets": {"groups": ["g"]}, "my-toolset": {"groups": ["g"]}},
        "workflows": {"workflow": {"groups": ["g"], "agents": []},
            "workflow-one": {"groups": ["g"], "agents": []}},
        "pipelines": {"pipelines": pipeline("pipelines"), "tool-pipeline": pipeline("tool-pipeline")},
    });
    let issues = issues_of(&load(&document).await.expect("parse"));
    // each protected name is an error that carries the API's refusal for that name
    for (item, name) in [
        ("tool Tool", "Tool"),
        ("toolset tool_sets", "tool_sets"),
        ("workflow workflow", "workflow"),
        ("pipeline pipelines", "pipelines"),
    ] {
        let found = issues
            .iter()
            .find(|issue| issue.item == item && issue.code == "invalid_value:name")
            .unwrap_or_else(|| panic!("no name error for {item}: {issues:?}"));
        assert_eq!(found.severity, Severity::Error);
        assert_eq!(
            found.message,
            thorium::utils::protected_names::refusal(name)
        );
    }
    // the allowed neighbors raise no name error
    for item in [
        "tool tool-runner",
        "toolset my-toolset",
        "workflow workflow-one",
        "pipeline tool-pipeline",
    ] {
        assert!(
            issues
                .iter()
                .all(|issue| !(issue.item == item && issue.code == "invalid_value:name")),
            "{item}: {issues:?}"
        );
    }
}

/// The real toolbox validates without errors
#[tokio::test]
async fn real_toolbox_validates_without_errors() {
    // load the toolbox this repository ships and check it
    let issues = issues_of(&load_text(REAL_TOOLBOX).await.expect("parse"));
    assert!(
        issues.iter().all(|issue| issue.severity != Severity::Error),
        "{issues:?}"
    );
}

/// Sources that can't be read are errors naming the field
#[tokio::test]
async fn unreadable_sources_are_errors() {
    // a manifest pointing at a spec file that isn't there
    let document = with_tools(&json!({
        "gone": {"groups": ["g"], "versions": {"1": {"spec_from": "missing.yaml"}}},
    }));
    let mut loaded = load(&document).await.expect("parse");
    let failures = resolve_sources(&mut loaded.manifest, None).await;
    // validation reports the failure as an error
    let issues = validate_manifest(&loaded.manifest, &loaded.report, &failures);
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "invalid_value:spec_from"
                && issue.severity == Severity::Error)
    );
}
