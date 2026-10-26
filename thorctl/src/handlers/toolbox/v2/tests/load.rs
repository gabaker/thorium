//! Tests for loading manifests of either format

use serde_json::{Value, json};
use std::path::PathBuf;

use super::super::load::{SourceFormat, resolve_sources};
use super::{
    CLEAN_V2, REAL_TOOLBOX, SMALL_LEGACY, Scratch, image_spec, load, load_text, quiet, tool_of,
    with_tools,
};

/// A manifest with only a tools section parses, keeps its order, and counts its items
#[tokio::test]
async fn manifest_with_only_tools_parses() {
    // load a manifest that holds nothing but a tools section
    let loaded = load(&with_tools(&json!({
        "b": tool_of(&image_spec("x:1")),
        "a": tool_of(&image_spec("y:1")),
    })))
    .await
    .expect("parse");
    // it is a v2 manifest with no legacy report
    assert_eq!(loaded.source, SourceFormat::V2);
    assert!(!loaded.report.has_issues());
    // the tools are counted and kept in name order
    let counts = loaded.manifest.counts();
    assert_eq!((counts.tools, counts.tool_versions), (2, 2));
    let names: Vec<&String> = loaded.manifest.tools.keys().collect();
    assert_eq!(names, ["a", "b"]);
    // an omitted format version reads as the current one
    assert_eq!(loaded.manifest.format_version, 2);
}

/// A v2 manifest serializes the same bytes after being parsed and written again
#[tokio::test]
async fn v2_manifest_round_trips_to_the_same_bytes() {
    // load the clean fixture and write it out
    let first = load_text(CLEAN_V2).await.expect("parse");
    let written = serde_json::to_string_pretty(&first.manifest).expect("serialize");
    // loading what was written and writing it again changes nothing
    let second = load_text(&written).await.expect("parse again");
    let rewritten = serde_json::to_string_pretty(&second.manifest).expect("serialize again");
    assert_eq!(written, rewritten);
    // the document keeps every section of the fixture
    let counts = second.manifest.counts();
    assert_eq!(
        (
            counts.tools,
            counts.toolsets,
            counts.agents,
            counts.workflows,
            counts.pipelines
        ),
        (3, 1, 1, 1, 1)
    );
    assert_eq!(counts.builds, 1);
}

/// A manifest with an unknown top level key is refused
#[tokio::test]
async fn unknown_top_level_key_is_refused() {
    // a misspelled section must not be silently ignored
    let err = load(&json!({"name": "t", "toolz": {}}))
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("toolz"), "{err}");
    // an unknown key inside a tool is refused too
    let err = load(&with_tools(
        &json!({"a": {"groups": ["g"], "versions": {}, "colour": 1}}),
    ))
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("colour"), "{err}");
}

/// A format version other than 2 is refused with a message naming it
#[tokio::test]
async fn unsupported_format_version_is_refused() {
    // a future format must not be read as this one
    let err = load(&json!({"format_version": 3, "name": "t"}))
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("format_version"), "{err}");
}

/// A legacy manifest loads as v2 and keeps what each entry builds from
#[tokio::test]
async fn legacy_manifest_loads_as_v2() {
    // load the small legacy fixture
    let loaded = load_text(SMALL_LEGACY).await.expect("parse");
    assert_eq!(loaded.source, SourceFormat::Legacy);
    let manifest = &loaded.manifest;
    // the image with a config became a tool in its config's group, remembering its config
    let clamav = &manifest.tools["clamav"];
    assert_eq!(clamav.groups, ["static"]);
    let version = &clamav.versions["latest"];
    assert!(
        version
            .legacy
            .as_ref()
            .is_some_and(|legacy| legacy.config.is_some())
    );
    assert_eq!(version.dir, "images/clamav");
    let build = version.build.as_ref().expect("build");
    assert!(build.build_image);
    assert_eq!(build.image_tags, ["ghcr.io/example/clamav:latest"]);
    // the entry with only build information became a build-only context
    assert_eq!(manifest.builds["base"]["1"].build_path, "images/base");
    assert!(!manifest.tools.contains_key("base"));
    // the bundled network policies are deduplicated and the registry carried over
    assert_eq!(manifest.registry.as_deref(), Some("ghcr.io/example"));
    // the pipeline's stage became a pinned stage tool
    let stage = &manifest.pipelines["antivirus"].versions["latest"].stage_tools["clamav"];
    assert_eq!(
        (stage.tool.as_str(), stage.version.as_deref()),
        ("clamav", Some("latest"))
    );
}

/// A legacy entry that can't convert is skipped with its code, and its pipeline is dropped
#[tokio::test]
async fn legacy_skips_use_the_migration_codes() {
    // load the fixture holding an image without a container image
    let loaded = load_text(SMALL_LEGACY).await.expect("parse");
    let skipped: Vec<(String, String)> = loaded
        .report
        .skipped
        .iter()
        .map(|item| (item.reason.code(), item.item.clone()))
        .collect();
    // the image is skipped, and the pipeline that stages it is dropped
    assert!(skipped.contains(&(
        "missing_field:image".to_owned(),
        "image noimage@latest".to_owned()
    )));
    assert!(skipped.contains(&(
        "invalid_value:stages.*".to_owned(),
        "pipeline scan-all@latest".to_owned()
    )));
    assert!(!loaded.manifest.tools.contains_key("noimage"));
    assert!(!loaded.manifest.pipelines.contains_key("scan-all"));
    // what converted is counted
    assert_eq!(loaded.report.converted_tools, 1);
    assert_eq!(loaded.report.converted_builds, 1);
    assert_eq!(loaded.report.converted_pipelines, 1);
    assert_eq!(loaded.report.mappings.len(), 1);
}

/// The legacy types still read and write a legacy manifest the way they always did
#[test]
fn legacy_types_round_trip_unchanged() {
    use crate::handlers::toolbox::manifest::ToolboxManifest;
    // a small legacy document with only fields the legacy types know
    let original = json!({
        "name": "legacy",
        "registry": "r",
        "pipelines": {"p": {"latest": {
            "dir": "d", "description": "", "images": {"i": {"version": "v"}}, "config": null
        }}},
        "images": {"i": {"v": {
            "dir": "d", "build_path": "b", "config": null
        }}},
        "bundled_images": false
    });
    // parsing and writing it again must give the same document
    let parsed: ToolboxManifest = serde_json::from_value(original.clone()).expect("parse");
    assert_eq!(serde_json::to_value(&parsed).expect("serialize"), original);
}

/// A pipeline that stages an image dropped for a group collision is reported, not a panic
#[tokio::test]
async fn pipeline_staging_a_collided_image_is_dropped_with_an_issue() {
    // one image whose versions sit in two groups, staged by a pipeline that pins one of them
    let legacy: Value = serde_json::from_str(SMALL_LEGACY).expect("fixture");
    let first = legacy["images"]["clamav"]["latest"].clone();
    let mut second = first.clone();
    second["config"]["group"] = json!("other");
    let mut pipeline = legacy["pipelines"]["antivirus"].clone();
    pipeline["latest"]["images"] = json!({"clamav": {"version": "latest"}});
    let document = json!({
        "name": "collided",
        "images": {"clamav": {"latest": first, "2": second}},
        "pipelines": {"antivirus": pipeline},
    });
    // loading must not panic
    let loaded = load(&document).await.expect("parse");
    // the collision is reported and the pipeline is dropped with the stage code
    assert_eq!(loaded.report.collisions, ["clamav"]);
    assert!(!loaded.manifest.tools.contains_key("clamav"));
    assert!(!loaded.manifest.pipelines.contains_key("antivirus"));
    assert!(loaded.report.skipped.iter().any(|item| {
        item.reason.code() == "invalid_value:stages.*" && item.item == "pipeline antivirus@latest"
    }));
}

/// A manifest with both legacy and v2 sections merges them
#[tokio::test]
async fn both_sections_merge() {
    // a document with a v2 tool and a legacy image
    let legacy: Value = serde_json::from_str(SMALL_LEGACY).expect("fixture");
    let document = json!({
        "name": "merged",
        "tools": {"extra": tool_of(&image_spec("x:1"))},
        "images": {"clamav": legacy["images"]["clamav"].clone()},
    });
    let loaded = load(&document).await.expect("parse");
    // both parts are present
    assert_eq!(loaded.source, SourceFormat::Merged);
    assert!(loaded.manifest.tools.contains_key("extra"));
    assert!(loaded.manifest.tools.contains_key("clamav"));
    assert_eq!(loaded.report.collisions.len(), 0);
}

/// A name defined as both a legacy image and a v2 tool is a collision
#[tokio::test]
async fn a_name_in_both_sections_is_a_collision() {
    // the same name in `images` and `tools`
    let legacy: Value = serde_json::from_str(SMALL_LEGACY).expect("fixture");
    let document = json!({
        "name": "merged",
        "tools": {"clamav": tool_of(&image_spec("x:1"))},
        "images": {"clamav": legacy["images"]["clamav"].clone()},
    });
    let loaded = load(&document).await.expect("parse");
    // the collision is reported and the v2 definition is the one kept
    assert_eq!(loaded.report.collisions, ["clamav"]);
    assert!(loaded.manifest.tools["clamav"].versions.contains_key("1"));
}

/// The real toolbox loads with the counts the repository's toolbox has
#[tokio::test]
async fn real_toolbox_loads_with_its_known_counts() {
    // load the toolbox this repository ships
    let loaded = load_text(REAL_TOOLBOX).await.expect("parse");
    assert_eq!(loaded.source, SourceFormat::Legacy);
    let manifest = &loaded.manifest;
    let counts = manifest.counts();
    // 59 images with a config became tools, and the 4 ghidra-base entries became builds
    assert_eq!((counts.tools, counts.tool_versions), (59, 59));
    assert_eq!((counts.builds, counts.build_versions), (1, 4));
    assert_eq!(counts.pipelines, 32);
    assert_eq!(counts.network_policies, 1);
    assert_eq!(manifest.network_policies[0].name, "allow-all");
    assert_eq!(manifest.named_groups(), ["static"]);
    // nothing was skipped, and the report counts match
    assert!(!loaded.report.has_issues(), "{:?}", loaded.report);
    assert_eq!(loaded.report.converted_tools, 59);
    assert_eq!(loaded.report.converted_builds, 4);
    assert_eq!(loaded.report.converted_pipelines, 32);
    assert_eq!(loaded.report.mappings.len(), 55);
    // four entries opt out of building and three refuse a base image override
    let builds: Vec<_> = manifest
        .tools
        .values()
        .flat_map(|tool| tool.versions.values())
        .filter_map(|version| version.build.as_ref())
        .collect();
    assert_eq!(builds.len(), 59);
    assert_eq!(builds.iter().filter(|build| !build.build_image).count(), 4);
    assert_eq!(
        builds
            .iter()
            .filter(|build| build
                .base_image
                .as_ref()
                .is_some_and(|base| base.allow_override == Some(false)))
            .count(),
        3
    );
    // every pipeline stage is pinned to the label the legacy pipeline named
    for pipeline in manifest.pipelines.values() {
        for version in pipeline.versions.values() {
            assert!(!version.stage_tools.is_empty());
            assert!(
                version
                    .stage_tools
                    .values()
                    .all(|stage| stage.version.as_deref() == Some("latest"))
            );
        }
    }
}

/// A directory, a config.toml, and a file path all load the same manifest, and a directory with
/// no manifest says how to make one
#[tokio::test]
async fn directories_load_their_manifest() {
    use super::super::load::load_any;
    use crate::args::toolbox::ManifestLocation;
    // a directory holding a toolbox.json and a config.toml
    let scratch = Scratch::new();
    std::fs::write(scratch.0.join("toolbox.json"), CLEAN_V2).expect("write");
    std::fs::write(scratch.0.join("config.toml"), "name = \"x\"\n").expect("write");
    for path in [
        scratch.0.clone(),
        scratch.0.join("config.toml"),
        scratch.0.join("toolbox.json"),
    ] {
        let loaded = load_any(&ManifestLocation::Path(path), &quiet())
            .await
            .expect("load");
        assert_eq!(loaded.manifest.name, "Clean v2 toolbox");
        assert_eq!(loaded.base_dir.as_deref(), Some(scratch.0.as_path()));
    }
    // a v2 file next to it wins over the legacy name
    std::fs::write(
        scratch.0.join("toolbox.v2.json"),
        "{\"name\": \"preferred\"}",
    )
    .expect("write");
    let loaded = load_any(&ManifestLocation::Path(scratch.0.clone()), &quiet())
        .await
        .expect("load");
    assert_eq!(loaded.manifest.name, "preferred");
    // an empty directory names `toolbox build`
    let empty = Scratch::new();
    let err = load_any(&ManifestLocation::Path(empty.0.clone()), &quiet())
        .await
        .expect_err("no manifest");
    assert!(err.to_string().contains("toolbox build"), "{err}");
}

/// Specs, configs, and policies a manifest points at are read, and the ones that can't be are
/// reported with the field that names them
#[tokio::test]
async fn sources_are_resolved_or_reported() {
    // a manifest pointing at a YAML spec, a missing spec, and a policy file
    let scratch = Scratch::new();
    let spec = serde_norway::to_string(&image_spec("x:1")).expect("yaml");
    std::fs::write(scratch.0.join("good.spec.yaml"), spec).expect("write");
    std::fs::write(
        scratch.0.join("policy.json"),
        json!({"name": "allow", "groups": ["g"], "ingress": null, "egress": null}).to_string(),
    )
    .expect("write");
    let document = with_tools(&json!({
        "good": {"groups": ["g"], "versions": {"1": {"spec_from": "good.spec.yaml", "network_policies_from": ["policy.json"]}}},
        "gone": {"groups": ["g"], "versions": {"1": {"spec_from": "missing.yaml"}}},
    }));
    let mut loaded = load(&document).await.expect("parse");
    let failures = resolve_sources(&mut loaded.manifest, Some(&scratch.0)).await;
    // the readable spec replaced its pointer and the policy was bundled
    let good = &loaded.manifest.tools["good"].versions["1"];
    assert!(good.spec.is_some() && good.spec_from.is_none());
    assert_eq!(good.network_policies[0].name, "allow");
    assert_eq!(good.network_policies_from.len(), 0);
    // the unreadable one is reported and keeps its pointer
    assert_eq!(failures.len(), 1);
    assert_eq!(
        (failures[0].item.as_str(), failures[0].field.as_str()),
        ("tool gone@1", "spec_from")
    );
    assert!(
        loaded.manifest.tools["gone"].versions["1"]
            .spec_from
            .is_some()
    );
}

/// A missing file is reported as an error rather than an empty manifest
#[tokio::test]
async fn missing_manifest_file_is_an_error() {
    use super::super::load::load_any;
    use crate::args::toolbox::ManifestLocation;
    // reading a file that isn't there fails with its path
    let err = load_any(
        &ManifestLocation::Path(PathBuf::from("/nonexistent/toolbox.json")),
        &quiet(),
    )
    .await
    .expect_err("missing");
    assert!(
        err.to_string().contains("/nonexistent/toolbox.json"),
        "{err}"
    );
}

/// A pipeline whose config is read from a source takes the description its version carries
/// (the `description.md` or manifest description) when the config has none, and keeps a
/// description of its own
#[tokio::test]
async fn read_pipeline_config_takes_the_carried_description_when_it_has_none() {
    // three pipeline configs: a null description, a missing one, and one of its own
    let scratch = Scratch::new();
    for (file, description) in [
        ("null.json", ", \"description\": null"),
        ("missing.json", ""),
        ("own.json", ", \"description\": \"its own\""),
    ] {
        let name = file.trim_end_matches(".json");
        std::fs::write(
            scratch.0.join(file),
            format!(r#"{{"group": "g", "name": "{name}", "order": ["a"]{description}}}"#),
        )
        .expect("write");
    }
    // one version of each carries a description, and one carries none
    let version = |file: &str, description: &str| {
        json!({"versions": {"1": {
            "description": description, "config_from": file,
            "stage_tools": {"a": {"tool": "a"}}
        }}})
    };
    let document = json!({
        "name": "test",
        "tools": {"a": tool_of(&image_spec("x:1"))},
        "pipelines": {
            "null": version("null.json", "from md"),
            "missing": version("missing.json", "from md"),
            "own": version("own.json", "from md"),
            "bare": version("null.json", ""),
        },
    });
    let mut loaded = load(&document).await.expect("parse");
    let failures = resolve_sources(&mut loaded.manifest, Some(&scratch.0)).await;
    assert!(failures.is_empty(), "{failures:?}");
    // read each pipeline's description in Thorium
    let described = |name: &str| {
        loaded.manifest.pipelines[name].versions["1"]
            .config
            .as_ref()
            .and_then(|config| config.description.clone())
    };
    assert_eq!(described("null").as_deref(), Some("from md"));
    assert_eq!(described("missing").as_deref(), Some("from md"));
    assert_eq!(described("own").as_deref(), Some("its own"));
    assert_eq!(described("bare"), None);
}
