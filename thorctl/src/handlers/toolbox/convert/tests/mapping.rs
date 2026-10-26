//! Tests for each rule that maps a legacy toolbox to the current format

use serde_json::{Value, json};
use thorium::client::migrations::{ImageContext, ToolIdMap, convert_image};
use thorium::models::{Image, ImageScaler, ToolKind, ToolSpec, entities::digests::ThorDigest};

use super::{
    SMALL_LEGACY, convert_default, convert_with, entry, issue_codes, pipeline, request, stored,
    toolbox,
};
use crate::handlers::toolbox::convert::{ConvertOptions, convert_document, render_manifest};
use crate::handlers::toolbox::v2::load::SourceFormat;

/// A legacy image becomes a tool of kind Image holding its spec, build, group, and description
#[tokio::test]
async fn an_image_becomes_an_image_tool() {
    // a described image with a container image
    let mut config = request("static", "clamav", Some("ghcr.io/example/clamav:latest"));
    config.description = Some("Antivirus".to_owned());
    let converted = convert_default(&toolbox(
        json!({"clamav": {"latest": entry(&config)}}),
        json!({}),
    ))
    .await;
    let tool = &converted.manifest.tools["clamav"];
    // the tool keeps the image's identity
    assert_eq!(tool.kind, Some(ToolKind::Image));
    assert_eq!(tool.groups, ["static"]);
    assert_eq!(tool.description.as_deref(), Some("Antivirus"));
    // its only version is the label the image had, holding a spec and no leftover legacy config
    assert_eq!(tool.versions.len(), 1);
    let version = &tool.versions["latest"];
    assert!(version.legacy.is_none() && version.spec_from.is_none());
    let spec = version.spec.as_ref().expect("a spec");
    assert_eq!(
        spec["Image"]["environment"]["K8s"]["image"],
        "ghcr.io/example/clamav:latest"
    );
    // the build information is kept beside the spec
    let build = version.build.as_ref().expect("a build");
    assert_eq!(build.build_path, "images/clamav");
    assert_eq!(build.image_tags, ["ghcr.io/example/clamav:latest"]);
    assert!(build.build_image);
    // and the counts say what was converted
    assert_eq!(converted.summary.tools, 1);
    assert_eq!(converted.summary.tool_versions, 1);
    assert_eq!(converted.summary.issues, []);
}

/// Every scaler's spec has the spec hash the live migration gives the stored image
#[tokio::test]
async fn specs_hash_like_the_live_migration() {
    // an image of each scaler, with env a scaler may drop, and a dependency between two of them
    let mut requests = Vec::new();
    for (name, scaler) in [
        ("kube", ImageScaler::K8s),
        ("bare", ImageScaler::BareMetal),
        ("win", ImageScaler::Windows),
        ("vm", ImageScaler::Kvm),
        ("ext", ImageScaler::External),
    ] {
        let mut config = request("corn", name, Some(&format!("repo/{name}:1.0"))).scaler(scaler);
        config
            .env
            .insert("LOG_LEVEL".to_owned(), Some("debug".to_owned()));
        config
            .env
            .insert("API_TOKEN".to_owned(), Some("tok".to_owned()));
        requests.push(config);
    }
    requests[0].dependencies.results.images = vec!["bare".to_owned()];
    let images: serde_json::Map<String, Value> = requests
        .iter()
        .map(|config| (config.name.clone(), json!({"latest": entry(config)})))
        .collect();
    let converted = convert_default(&toolbox(Value::Object(images), json!({}))).await;
    // the live conversion of the stored images
    let stored_images: Vec<Image> = requests.iter().map(stored).collect();
    let map = ToolIdMap::from_images(&stored_images);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    for (config, image) in requests.iter().zip(&stored_images) {
        let live = convert_image(image, &ctx).expect("live conversion");
        let live_hash = ThorDigest::tool_spec(&live.tool.spec, None).expect("hash");
        // the manifest holds references where the live spec holds ids, so put the ids back
        let mut text = converted.manifest.tools[&config.name].versions["latest"]
            .spec
            .as_ref()
            .expect("a spec")
            .to_string();
        for other in &requests {
            let id = ToolIdMap::tool_id(&other.group, &other.name).to_string();
            text = text.replace(&format!("ref:tool/{}", other.name), &id);
        }
        let spec: ToolSpec = serde_json::from_str(&text).expect("a typed spec");
        assert_eq!(spec, live.tool.spec, "{}", config.name);
        let hash = ThorDigest::tool_spec(&spec, None).expect("hash");
        assert_eq!(hash, live_hash, "{}", config.name);
    }
}

/// Dependency names become tool references, and a name that matches no tool is dropped and
/// reported while the tool is kept
#[tokio::test]
async fn dependency_names_become_references() {
    let mut a = request("static", "a", Some("repo/a:1"));
    a.dependencies.results.images = vec!["b".to_owned(), "gone".to_owned()];
    a.dependencies.children.images = vec!["b".to_owned()];
    let b = request("static", "b", Some("repo/b:1"));
    let document = toolbox(
        json!({"a": {"latest": entry(&a)}, "b": {"latest": entry(&b)}}),
        json!({}),
    );
    let converted = convert_default(&document).await;
    let spec = converted.manifest.tools["a"].versions["latest"]
        .spec
        .as_ref()
        .unwrap();
    let dependencies = &spec["Image"]["data"]["dependencies"];
    // the surviving names are references and the unknown one is gone
    assert_eq!(dependencies["results"]["tools"], json!(["ref:tool/b"]));
    assert_eq!(dependencies["children"]["tools"], json!(["ref:tool/b"]));
    // the dropped name is an issue that keeps the tool
    assert_eq!(issue_codes(&converted), ["dropped_dependency:results"]);
    assert!(!converted.summary.issues[0].skipped);
    assert!(converted.summary.has_issues());
    assert!(converted.manifest.tools.contains_key("a"));
}

/// A tool whose whole dependency list names nothing converted is skipped, and so is the pipeline
/// that runs it
#[tokio::test]
async fn an_emptied_dependency_list_skips_the_tool() {
    let mut a = request("static", "a", Some("repo/a:1"));
    a.dependencies.results.images = vec!["gone".to_owned()];
    let document = toolbox(
        json!({"a": {"latest": entry(&a)}}),
        json!({"p": pipeline("static", "p", &[("a", "latest")])}),
    );
    let converted = convert_default(&document).await;
    assert!(converted.manifest.tools.is_empty());
    assert!(converted.manifest.pipelines.is_empty());
    assert_eq!(
        issue_codes(&converted),
        [
            "invalid_value:dependencies.results.images",
            "invalid_value:stages.*"
        ]
    );
    assert!(converted.summary.issues.iter().all(|issue| issue.skipped));
    assert_eq!(converted.summary.tool_versions, 0);
}

/// A container image entry with no image is skipped like the live migration skips it, and an
/// entry for a scaler that needs no image is not
#[tokio::test]
async fn images_without_a_container_image_are_skipped() {
    let kube = request("static", "kube", None);
    let bare = request("static", "bare", None).scaler(ImageScaler::BareMetal);
    let document = toolbox(
        json!({"kube": {"latest": entry(&kube)}, "bare": {"latest": entry(&bare)}}),
        json!({}),
    );
    let converted = convert_default(&document).await;
    // the container entry is skipped with the migration's code, naming the image and label
    assert_eq!(issue_codes(&converted), ["missing_field:image"]);
    assert_eq!(converted.summary.issues[0].item, "image kube@latest");
    assert!(converted.summary.issues[0].skipped);
    assert!(!converted.manifest.tools.contains_key("kube"));
    // the bare metal entry converts
    assert!(converted.manifest.tools.contains_key("bare"));
}

/// `--fill-image-from-tags` gives an entry with no image its first build tag and says so
#[tokio::test]
async fn fill_image_from_tags_fills_and_notes() {
    let kube = request("static", "kube", None);
    let document = toolbox(json!({"kube": {"latest": entry(&kube)}}), json!({}));
    let options = ConvertOptions {
        fill_image_from_tags: true,
        ..ConvertOptions::default()
    };
    let converted = convert_with(&document, &options).await;
    // the entry converted with its first tag as its image
    let spec = converted.manifest.tools["kube"].versions["latest"]
        .spec
        .as_ref()
        .unwrap();
    assert_eq!(
        spec["Image"]["environment"]["K8s"]["image"],
        "ghcr.io/example/kube:latest"
    );
    // nothing was skipped, and a note says where the image came from
    assert_eq!(converted.summary.issues, []);
    assert!(
        converted
            .summary
            .notes
            .iter()
            .any(|note| note.code == "note:filled:image" && note.item == "image kube@latest")
    );
}

/// Pipeline stages pin the version the legacy pipeline named, or follow the latest with
/// `--follow-latest`
#[tokio::test]
async fn stages_pin_versions_unless_following_latest() {
    // an image with two labels, and a pipeline pinning the second
    let first = request("static", "a", Some("repo/a:1"));
    let second = request("static", "a", Some("repo/a:2"));
    let document = toolbox(
        json!({"a": {"1.0": entry(&first), "2.0": entry(&second)}}),
        json!({"p": pipeline("static", "p", &[("a", "2.0")])}),
    );
    let converted = convert_default(&document).await;
    let version = &converted.manifest.pipelines["p"].versions["latest"];
    // the stage runs the tool a, pinned to 2.0
    assert_eq!(version.stage_tools["a"].tool, "a");
    assert_eq!(version.stage_tools["a"].version.as_deref(), Some("2.0"));
    assert!(!version.incomplete);
    // the pipeline config lost its stage tools and kept the rest
    let config = version.config.as_ref().unwrap();
    assert_eq!(
        (config.group.as_str(), config.name.as_str(), config.sla),
        ("static", "p", Some(100))
    );
    // the mapping is reported
    assert_eq!(converted.summary.mappings.len(), 1);
    assert_eq!(
        converted.summary.mappings[0].version.as_deref(),
        Some("2.0")
    );
    assert_eq!(converted.summary.pipelines, 1);
    // following latest leaves the version off
    let options = ConvertOptions {
        follow_latest: true,
        ..ConvertOptions::default()
    };
    let converted = convert_with(&document, &options).await;
    let stage = &converted.manifest.pipelines["p"].versions["latest"].stage_tools["a"];
    assert_eq!((stage.tool.as_str(), stage.version.as_deref()), ("a", None));
    assert_eq!(converted.summary.mappings[0].version, None);
}

/// A pipeline with a stage whose tool was not converted is left out, and kept flagged when asked
#[tokio::test]
async fn incomplete_pipelines_are_dropped_or_kept() {
    // the small fixture has a pipeline staging an image that can't convert
    let document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    let converted = convert_default(&document).await;
    assert!(!converted.manifest.pipelines.contains_key("scan-all"));
    assert!(converted.manifest.pipelines.contains_key("antivirus"));
    assert_eq!(
        issue_codes(&converted),
        ["missing_field:image", "invalid_value:stages.*"]
    );
    assert_eq!(converted.summary.issues[1].item, "pipeline scan-all@latest");
    // keeping incomplete pipelines puts it back, flagged, with the stages that do resolve
    let options = ConvertOptions {
        keep_incomplete_pipelines: true,
        ..ConvertOptions::default()
    };
    let kept = convert_with(&document, &options).await;
    let version = &kept.manifest.pipelines["scan-all"].versions["latest"];
    assert!(version.incomplete);
    assert!(
        version.stage_tools.contains_key("clamav") && !version.stage_tools.contains_key("noimage")
    );
    // it is still an issue (so strict conversions fail), but it is no longer skipped
    assert!(kept.summary.has_issues());
    assert!(
        kept.summary
            .issues
            .iter()
            .any(|issue| issue.code == "invalid_value:stages.*" && !issue.skipped)
    );
    assert_eq!(kept.summary.pipelines, 2);
}

/// A pipeline stage in another group than its tool is unresolved
#[tokio::test]
async fn stages_resolve_inside_the_pipelines_group() {
    let tool = request("static", "a", Some("repo/a:1"));
    let document = toolbox(
        json!({"a": {"latest": entry(&tool)}}),
        json!({"p": pipeline("dynamic", "p", &[("a", "latest")])}),
    );
    let converted = convert_default(&document).await;
    assert!(converted.manifest.pipelines.is_empty());
    assert_eq!(issue_codes(&converted), ["invalid_value:stages.*"]);
}

/// Entries with build information and no config become build contexts, and builds that are
/// switched off keep saying so
#[tokio::test]
async fn build_only_entries_become_builds() {
    let document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    let converted = convert_default(&document).await;
    // the build-only image is a build, not a tool
    assert_eq!(
        converted.manifest.builds["base"]["1"].build_path,
        "images/base"
    );
    assert!(!converted.manifest.tools.contains_key("base"));
    assert_eq!(converted.summary.builds, 1);
    // a tool whose build is off keeps its build entry with the flag
    let mut tool = entry(&request("static", "off", Some("repo/off:1")));
    tool["build_image"] = json!(false);
    let converted = convert_default(&toolbox(json!({"off": {"latest": tool}}), json!({}))).await;
    let build = converted.manifest.tools["off"].versions["latest"]
        .build
        .as_ref()
        .unwrap();
    assert!(!build.build_image);
    // and that survives being written
    let text = render_manifest(&converted.manifest).unwrap();
    assert!(text.contains("\"build_image\": false"));
}

/// Names no tool can have are skipped
#[tokio::test]
async fn unusable_names_are_skipped() {
    let config = request("static", "bad name", Some("repo/a:1"));
    let converted = convert_default(&toolbox(
        json!({"bad name": {"latest": entry(&config)}}),
        json!({}),
    ))
    .await;
    assert_eq!(issue_codes(&converted), ["invalid_value:name"]);
    assert!(converted.manifest.tools.is_empty());
}

/// An image whose versions are in different groups is a collision, which `--group-override`
/// resolves by putting everything in one group
#[tokio::test]
async fn group_collisions_need_an_override() {
    let one = request("alpha", "a", Some("repo/a:1"));
    let two = request("beta", "a", Some("repo/a:2"));
    let document = toolbox(
        json!({"a": {"1": entry(&one), "2": entry(&two)}}),
        json!({"p": pipeline("alpha", "p", &[("a", "1")])}),
    );
    let converted = convert_default(&document).await;
    assert_eq!(converted.summary.collisions, ["a"]);
    assert!(converted.summary.has_issues());
    // the override gives every tool and pipeline the one group
    let options = ConvertOptions {
        group_override: Some("gamma".to_owned()),
        ..ConvertOptions::default()
    };
    let converted = convert_with(&document, &options).await;
    assert_eq!(converted.summary.collisions, Vec::<String>::new());
    assert_eq!(converted.manifest.tools["a"].groups, ["gamma"]);
    assert_eq!(converted.manifest.tools["a"].versions.len(), 2);
    let config = converted.manifest.pipelines["p"].versions["latest"]
        .config
        .as_ref()
        .unwrap();
    assert_eq!(config.group, "gamma");
    assert_eq!(converted.summary.issues, []);
}

/// Network policies bundled with several images are written once
#[tokio::test]
async fn bundled_network_policies_are_deduplicated() {
    let mut first = entry(&request("static", "a", Some("repo/a:1")));
    let mut second = entry(&request("static", "b", Some("repo/b:1")));
    let policy =
        json!({"name": "allow-all", "groups": ["static"], "ingress": null, "egress": null});
    first["network_policies"] = json!([policy]);
    second["network_policies"] = json!([policy]);
    let converted = convert_default(&toolbox(
        json!({"a": {"latest": first}, "b": {"latest": second}}),
        json!({}),
    ))
    .await;
    assert_eq!(converted.manifest.network_policies.len(), 1);
    assert_eq!(converted.summary.network_policies, 1);
}

/// Env variable names that look like secrets are warned about, and the values are never printed
#[tokio::test]
async fn secret_looking_env_is_a_warning() {
    let mut config = request("static", "a", Some("repo/a:1"));
    config
        .env
        .insert("API_TOKEN".to_owned(), Some("hunter2".to_owned()));
    let converted = convert_default(&toolbox(
        json!({"a": {"latest": entry(&config)}}),
        json!({}),
    ))
    .await;
    assert_eq!(converted.summary.issues, []);
    assert_eq!(converted.summary.warnings.len(), 1);
    assert_eq!(converted.summary.warnings[0].code, "warning:secret-env");
    assert!(converted.summary.warnings[0].message.contains("API_TOKEN"));
    // the report names the variable only
    let report = serde_json::to_string(&converted.summary).unwrap();
    assert!(!report.contains("hunter2"));
    assert!(converted.summary.has_issues());
}

/// Fields the spec's environment never reads are noted, naming env variables only
#[tokio::test]
async fn dropped_fields_are_noted() {
    let mut config = request("static", "bare", Some("repo/a:1")).scaler(ImageScaler::BareMetal);
    config
        .env
        .insert("LOG_LEVEL".to_owned(), Some("debug".to_owned()));
    let converted = convert_default(&toolbox(
        json!({"bare": {"latest": entry(&config)}}),
        json!({}),
    ))
    .await;
    assert_eq!(converted.summary.issues, []);
    assert!(
        converted
            .summary
            .notes
            .iter()
            .any(|note| note.code == "note:dropped-field"
                && note.message.contains("LOG_LEVEL")
                && !note.message.contains("debug"))
    );
}

/// A later version's differing description is noted and the first one is kept
#[tokio::test]
async fn differing_descriptions_are_noted() {
    let mut first = request("static", "a", Some("repo/a:1"));
    first.description = Some("one".to_owned());
    let mut second = request("static", "a", Some("repo/a:2"));
    second.description = Some("two".to_owned());
    let converted = convert_default(&toolbox(
        json!({"a": {"1": entry(&first), "2": entry(&second)}}),
        json!({}),
    ))
    .await;
    assert_eq!(
        converted.manifest.tools["a"].description.as_deref(),
        Some("one")
    );
    assert!(
        converted
            .summary
            .notes
            .iter()
            .any(|note| note.item.contains('a') && note.message.contains("differs"))
    );
}

/// A manifest that is already in the current format converts to itself
#[tokio::test]
async fn current_manifests_convert_to_themselves() {
    // convert a legacy toolbox, then convert its result
    let document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    let once = convert_default(&document).await;
    let text = render_manifest(&once.manifest).unwrap();
    let again = convert_document(
        text.as_bytes(),
        None,
        &ConvertOptions::default(),
        &super::quiet(),
    )
    .await
    .unwrap();
    // the second conversion read a v2 manifest and wrote the same bytes
    assert_eq!(again.summary.source, SourceFormat::V2);
    assert_eq!(render_manifest(&again.manifest).unwrap(), text);
    assert_eq!(again.summary.issues, []);
    assert_eq!(again.summary.tools, once.summary.tools);
    // the options do not touch a current manifest's pipelines
    let options = ConvertOptions {
        follow_latest: true,
        ..ConvertOptions::default()
    };
    let followed = convert_document(text.as_bytes(), None, &options, &super::quiet())
        .await
        .unwrap();
    assert_eq!(render_manifest(&followed.manifest).unwrap(), text);
}

/// The converted text is stable: sorted, two-space JSON ending in a newline, the same every time
#[tokio::test]
async fn converted_text_is_deterministic() {
    let document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    let first = render_manifest(&convert_default(&document).await.manifest).unwrap();
    for _ in 0..3 {
        let again = render_manifest(&convert_default(&document).await.manifest).unwrap();
        assert_eq!(again, first);
    }
    assert!(first.ends_with("}\n") && first.starts_with("{\n  \"format_version\": 2,\n"));
    // the legacy sections are gone and the current ones are there
    assert!(
        !first.contains("\"images\"")
            && first.contains("\"tools\"")
            && first.contains("\"stage_tools\"")
    );
}

/// The small legacy toolbox converts to the recorded manifest and report
#[tokio::test]
async fn the_small_toolbox_matches_its_goldens() {
    let document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    let converted = convert_default(&document).await;
    super::assert_golden(
        "small.v2.json",
        &render_manifest(&converted.manifest).unwrap(),
    );
    super::assert_golden(
        "small.report.json",
        &crate::utils::output::to_json(&converted.summary).unwrap(),
    );
}
