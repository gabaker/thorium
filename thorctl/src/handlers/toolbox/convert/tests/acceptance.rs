//! Tests converting this repository's real toolbox

use serde_json::Value;
use std::collections::BTreeSet;
use thorium::client::migrations::{ImageContext, ToolIdMap, convert_image};
use thorium::models::{Image, ImageRequest, ToolKind, ToolSpec, entities::digests::ThorDigest};

use super::{REAL_TOOLBOX, REAL_TOOLBOX_V2, convert_default, quiet, stored};
use crate::handlers::toolbox::convert::{ConvertOptions, convert_document, render_manifest};
use crate::handlers::toolbox::v2::load::parse_any;
use crate::handlers::toolbox::v2::validate::{Severity, validate_manifest};

/// Read the real toolbox's legacy document
fn legacy_document() -> Value {
    // the file is the legacy toolbox kept from before the manifests became tools
    serde_json::from_str(REAL_TOOLBOX).expect("the real toolbox parses")
}

/// The real toolbox converts to 59 image tools, 4 builds, 32 pinned pipelines and 1 policy with
/// nothing reported
#[tokio::test]
async fn the_real_toolbox_converts_cleanly() {
    let converted = convert_default(&legacy_document()).await;
    let summary = &converted.summary;
    // nothing was skipped or raised
    assert!(summary.issues.is_empty(), "{:?}", summary.issues);
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
    assert_eq!(summary.collisions, Vec::<String>::new());
    // 59 tools with one version each, 4 ghidra-base builds, 32 pipelines, 1 policy
    assert_eq!((summary.tools, summary.tool_versions), (59, 59));
    assert_eq!(summary.builds, 4);
    assert_eq!(summary.pipelines, 32);
    assert_eq!(summary.network_policies, 1);
    let manifest = &converted.manifest;
    assert_eq!(manifest.builds.len(), 1);
    assert_eq!(
        manifest.builds["ghidra-base"].keys().collect::<Vec<_>>(),
        ["10.3.2", "11.2", "12.0", "12.1"]
    );
    // every tool is an Image tool in the static group with the latest label and a K8s spec
    for (name, tool) in &manifest.tools {
        assert_eq!(tool.kind, Some(ToolKind::Image), "{name}");
        assert_eq!(tool.groups, ["static"], "{name}");
        assert_eq!(
            tool.versions.keys().collect::<Vec<_>>(),
            ["latest"],
            "{name}"
        );
        let spec = tool.versions["latest"].spec.as_ref().expect("a spec");
        assert!(
            spec["Image"]["environment"]["K8s"]["image"]
                .as_str()
                .is_some_and(|image| !image.is_empty()),
            "{name}"
        );
    }
    // the four builds that are switched off stay off
    let off = manifest
        .tools
        .values()
        .flat_map(|tool| tool.versions.values())
        .filter(|version| {
            version
                .build
                .as_ref()
                .is_some_and(|build| !build.build_image)
        })
        .count();
    assert_eq!(off, 4);
    // every stage of every pipeline is pinned to the version its legacy pipeline named
    let legacy = legacy_document();
    let mut stages = 0;
    for (name, pipeline) in &manifest.pipelines {
        let version = &pipeline.versions["latest"];
        let named = &legacy["pipelines"][name]["latest"]["images"];
        assert_eq!(
            version.stage_tools.len(),
            named.as_object().unwrap().len(),
            "{name}"
        );
        for (stage, stage_tool) in &version.stage_tools {
            assert_eq!(stage_tool.tool, *stage, "{name}");
            assert_eq!(
                stage_tool.version.as_deref(),
                named[stage]["version"].as_str(),
                "{name}/{stage}"
            );
            stages += 1;
        }
    }
    assert_eq!(summary.mappings.len(), stages);
}

/// Every converted tool's spec hashes like the live migration's conversion of the same image
#[tokio::test]
async fn real_specs_hash_like_the_live_migration() {
    let converted = convert_default(&legacy_document()).await;
    // the stored form of every legacy image, as the API would hold it
    let legacy = legacy_document();
    let mut images: Vec<Image> = Vec::new();
    for (name, labels) in legacy["images"].as_object().unwrap() {
        for entry in labels.as_object().unwrap().values() {
            if entry.get("config").is_none() {
                continue;
            }
            let request: ImageRequest = serde_json::from_value(entry["config"].clone()).unwrap();
            let mut image = stored(&request);
            image.name = name.clone();
            images.push(image);
        }
    }
    assert_eq!(images.len(), 59);
    let map = ToolIdMap::from_images(&images);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    for image in &images {
        // the live conversion's hash
        let live = convert_image(image, &ctx).expect("the live conversion succeeds");
        let live_hash = ThorDigest::tool_spec(&live.tool.spec, None).unwrap();
        // the manifest's spec, typed, hashes the same
        let value = converted.manifest.tools[&image.name].versions["latest"]
            .spec
            .clone()
            .unwrap();
        let spec: ToolSpec = serde_json::from_value(value).expect("a typed spec");
        assert_eq!(
            ThorDigest::tool_spec(&spec, None).unwrap(),
            live_hash,
            "{}",
            image.name
        );
    }
}

/// The committed toolbox, built from the tool manifests, is exactly what converting the kept
/// legacy toolbox writes: rewriting the manifests to tools changed nothing a conversion would not
#[tokio::test]
async fn the_committed_v2_toolbox_is_reproducible() {
    let converted = convert_default(&legacy_document()).await;
    let text = render_manifest(&converted.manifest).unwrap();
    assert!(
        text == REAL_TOOLBOX_V2,
        "converting tools/toolbox.v1.json no longer gives tools/toolbox.json"
    );
    // converting the committed file changes nothing
    let again = convert_document(
        REAL_TOOLBOX_V2.as_bytes(),
        None,
        &ConvertOptions::default(),
        &quiet(),
    )
    .await
    .unwrap();
    assert_eq!(render_manifest(&again.manifest).unwrap(), REAL_TOOLBOX_V2);
}

/// Both forms of the real toolbox validate without errors and name the same tools and pipelines
#[tokio::test]
async fn both_real_toolboxes_validate() {
    let legacy = parse_any(REAL_TOOLBOX.as_bytes(), None, &quiet())
        .await
        .unwrap();
    let current = parse_any(REAL_TOOLBOX_V2.as_bytes(), None, &quiet())
        .await
        .unwrap();
    for loaded in [&legacy, &current] {
        let issues = validate_manifest(&loaded.manifest, &loaded.report, &[]);
        let errors: Vec<_> = issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }
    let names = |loaded: &crate::handlers::toolbox::v2::load::Loaded| -> (BTreeSet<String>, BTreeSet<String>) {
        (loaded.manifest.tools.keys().cloned().collect(), loaded.manifest.pipelines.keys().cloned().collect())
    };
    assert_eq!(names(&legacy), names(&current));
}
