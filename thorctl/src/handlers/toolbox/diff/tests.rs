//! Tests for diffing a toolbox against what Thorium holds

use serde_json::json;
use thorium::models::{EntityKinds, EntityMetadata, ToolKind, ToolSetEntity};
use uuid::Uuid;

use super::classify::{plan, version_hash};
use super::{DiffReport, fill_legacy_specs, is_actionable};
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::export::live::LiveView;
use crate::handlers::toolbox::export::live::override_groups;
use crate::handlers::toolbox::export::snapshot::Snapshot;
use crate::handlers::toolbox::export::tests::{
    container_spec, image_spec, instance, pipeline, snap_tool, tool_entity,
};
use crate::handlers::toolbox::v2::graph::waves;
use crate::handlers::toolbox::v2::load::parse_any;
use crate::handlers::toolbox::v2::plan_types::{
    ItemKind, ItemRef, ItemState, Plan, PolicyState, VersionClass,
};
use crate::handlers::toolbox::v2::types::{ToolRef, ToolboxManifestV2};

/// Build the manifest of some live tools, toolsets, and pipelines
///
/// # Arguments
///
/// * `snapshot` - What the manifest is built from
fn manifest_of(snapshot: &Snapshot) -> ToolboxManifestV2 {
    // the same conversion an export uses
    crate::handlers::toolbox::export::tests::built(snapshot)
}

/// Build what Thorium would hold if the manifest had been imported
///
/// # Arguments
///
/// * `manifest` - The manifest that was imported
fn imported(manifest: &ToolboxManifestV2) -> LiveView {
    // tools are added in dependency order, each with the hash Thorium would compute
    let mut live = LiveView {
        live: manifest.clone(),
        ..LiveView::default()
    };
    for item in waves(manifest).expect("no cycles").into_iter().flatten() {
        if item.kind != ItemKind::Tool {
            continue;
        }
        let tool = &manifest.tools[&item.name];
        let mut instances = Vec::new();
        for label in tool.publish_order().into_iter().rev() {
            let version = &tool.versions[&label];
            let spec = version.spec.as_ref().unwrap();
            let hash = version_hash(spec, &version.provides, &live).expect("a hash");
            let mut made = instance(Uuid::new_v4(), &label, spec.clone());
            made.spec_hash = hash;
            instances.push(made);
        }
        let groups: Vec<&str> = tool.groups.iter().map(String::as_str).collect();
        live.tools.push(snap_tool(
            &item.name,
            &groups,
            tool.kind.unwrap_or(ToolKind::Image),
            instances,
        ));
    }
    live
}

/// Find an item's plan
///
/// # Arguments
///
/// * `plan` - The plan
/// * `kind` - The kind of item
/// * `name` - The item's name
fn item<'a>(
    plan: &'a Plan,
    kind: ItemKind,
    name: &str,
) -> &'a crate::handlers::toolbox::v2::plan_types::ItemPlan {
    // the plan holds every item of the manifest
    plan.items
        .iter()
        .find(|found| found.item == ItemRef::new(kind, name))
        .expect("the item is in the plan")
}

/// A toolbox with one tool, a toolset, and a pipeline over the tool
fn small_snapshot() -> Snapshot {
    // a tool with two versions and the things built on it
    let tool = snap_tool(
        "clamav",
        &["static"],
        ToolKind::Image,
        vec![
            instance(Uuid::new_v4(), "2.0.0", image_spec("c:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("c:1")),
        ],
    );
    let stage = tool.id;
    let toolset = thorium::models::Entity {
        id: Uuid::new_v4(),
        name: "kit".to_owned(),
        kind: EntityKinds::ToolSet,
        metadata: EntityMetadata::ToolSet(ToolSetEntity {
            tools: vec![tool_entity(
                tool.id,
                "clamav",
                &["static"],
                ToolKind::Image,
                None,
            )],
            ..ToolSetEntity::default()
        }),
        ..tool_entity(Uuid::new_v4(), "x", &["static"], ToolKind::Image, None)
    };
    Snapshot {
        tools: vec![tool],
        toolsets: vec![toolset],
        pipelines: vec![pipeline("static", "antivirus", &[("clamav", stage)])],
        ..Snapshot::default()
    }
}

/// An empty Thorium has every item and version new
#[test]
fn everything_is_new_in_an_empty_thorium() {
    // plan a toolbox against nothing
    let manifest = manifest_of(&small_snapshot());
    let plan = plan(&manifest, &LiveView::default());
    for planned in &plan.items {
        assert_eq!(planned.state, ItemState::Create, "{}", planned.item);
    }
    let tool = item(&plan, ItemKind::Tool, "clamav");
    assert!(
        tool.versions
            .iter()
            .all(|version| version.class == VersionClass::New)
    );
    assert_eq!(tool.versions.len(), 2);
    assert!(plan.has_changes() && is_actionable(&plan));
}

/// A toolbox identical to what Thorium holds has nothing to do
#[test]
fn an_identical_toolbox_changes_nothing() {
    // plan a toolbox against what importing it would have made
    let manifest = manifest_of(&small_snapshot());
    let live = imported(&manifest);
    let planned_all = plan(&manifest, &live);
    for planned in &planned_all.items {
        assert_eq!(planned.state, ItemState::Unchanged, "{}", planned.item);
        assert!(
            planned
                .versions
                .iter()
                .all(|v| v.class == VersionClass::Unchanged)
        );
    }
    assert!(!planned_all.has_changes() && !is_actionable(&planned_all));
}

/// A version Thorium lacks is new, and a label with another spec is a conflict
#[test]
fn tool_versions_are_classified_by_spec_hash() {
    // Thorium holds 1.0.0 as in the toolbox but 2.0.0 with another spec
    let manifest = manifest_of(&small_snapshot());
    let mut live = imported(&manifest);
    live.tools[0].instances[0].spec_hash = "another".to_owned();
    // and the toolbox gains a third version
    let mut manifest = manifest;
    let clamav = manifest.tools.get_mut("clamav").unwrap();
    let mut third = clamav.versions["1.0.0"].clone();
    third.spec = Some(image_spec("c:3"));
    clamav.versions.insert("3.0.0".to_owned(), third);
    let plan = plan(&manifest, &live);
    let tool = item(&plan, ItemKind::Tool, "clamav");
    let classes: Vec<_> = tool
        .versions
        .iter()
        .map(|version| (version.label.as_str(), version.class))
        .collect();
    assert_eq!(
        classes,
        [
            ("1.0.0", VersionClass::Unchanged),
            ("3.0.0", VersionClass::NewVersion),
            // the tool's latest label is published last
            ("2.0.0", VersionClass::LabelConflict),
        ]
    );
    assert_eq!(tool.state, ItemState::Update);
    // the conflict names the label, and its replacement label carries the new hash
    assert_eq!(
        tool.conflict.as_ref().unwrap().label.as_deref(),
        Some("2.0.0")
    );
    let conflicted = &tool.versions[2];
    assert!(
        conflicted
            .publish_label
            .as_ref()
            .unwrap()
            .starts_with("2.0.0+")
    );
}

/// A version Thorium has under another label is unchanged, since the spec is what matters
#[test]
fn a_spec_under_another_label_is_unchanged() {
    // Thorium holds the same spec labeled differently
    let manifest = manifest_of(&small_snapshot());
    let mut live = imported(&manifest);
    live.tools[0].instances[0].label = "renamed".to_owned();
    let plan = plan(&manifest, &live);
    let tool = item(&plan, ItemKind::Tool, "clamav");
    assert_eq!(tool.versions[0].class, VersionClass::Unchanged);
}

/// Specs that refer to other tools hash the same once the references resolve
#[test]
fn references_resolve_before_hashing() {
    // a tool pinning a version of another, and a tool depending on it
    let base = snap_tool(
        "base",
        &["g"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "1.0.0", image_spec("b:1"))],
    );
    let pinned = base.instances[0].id;
    let mut dependent = image_spec("d:1");
    dependent["Image"]["data"]["dependencies"]["results"]["tools"] = json!([base.id]);
    let manifest = manifest_of(&Snapshot {
        tools: vec![
            base.clone(),
            snap_tool(
                "dep",
                &["g"],
                ToolKind::Image,
                vec![instance(Uuid::new_v4(), "1", dependent)],
            ),
            snap_tool(
                "caller",
                &["g"],
                ToolKind::Agentic,
                vec![instance(
                    Uuid::new_v4(),
                    "1",
                    container_spec("call", pinned),
                )],
            ),
        ],
        ..Snapshot::default()
    });
    // the references in the manifest resolve against a Thorium that holds the tools
    let live = imported(&manifest);
    let plan = plan(&manifest, &live);
    for name in ["base", "dep", "caller"] {
        assert_eq!(
            item(&plan, ItemKind::Tool, name).state,
            ItemState::Unchanged,
            "{name}"
        );
    }
    // with the base tool gone, nothing can be hashed and the dependent version is new
    let missing = LiveView {
        tools: live
            .tools
            .iter()
            .filter(|tool| tool.name == "dep")
            .cloned()
            .collect(),
        ..LiveView::default()
    };
    let plan = super::classify::plan(&manifest, &missing);
    assert_eq!(
        item(&plan, ItemKind::Tool, "dep").versions[0].class,
        VersionClass::LabelConflict
    );
    // the hash of a spec naming a tool Thorium lacks can't be computed
    let spec = manifest.tools["dep"].versions["1"].spec.as_ref().unwrap();
    assert!(version_hash(spec, &[], &LiveView::default()).is_err());
    // a harness's provided tools contribute their hashes
    let provides = [ToolRef {
        tool: "base".to_owned(),
        version: Some("1.0.0".to_owned()),
    }];
    let plain_hash = version_hash(&image_spec("h:1"), &[], &live).unwrap();
    let with_native = version_hash(&image_spec("h:1"), &provides, &live).unwrap();
    assert_ne!(plain_hash, with_native);
}

/// A toolset or pipeline that differs is updated with its difference shown
#[test]
fn in_place_items_show_their_differences() {
    // Thorium holds the toolset and pipeline as in the toolbox
    let manifest = manifest_of(&small_snapshot());
    let mut live = imported(&manifest);
    // then the toolbox's toolset gains a description and its pipeline another SLA
    let mut changed = manifest.clone();
    changed.toolsets.get_mut("kit").unwrap().description = Some("triage kit".to_owned());
    changed
        .pipelines
        .get_mut("antivirus")
        .unwrap()
        .versions
        .get_mut("latest")
        .unwrap()
        .config
        .as_mut()
        .unwrap()
        .sla = Some(1_000);
    let plan_changed = plan(&changed, &live);
    let toolset = item(&plan_changed, ItemKind::Toolset, "kit");
    assert_eq!(toolset.state, ItemState::Update);
    let diff = toolset.diff.as_ref().unwrap();
    assert!(diff.contains("+description: triage kit"), "{diff}");
    assert!(diff.contains("--- live/toolset kit") && diff.contains("+++ toolbox/toolset kit"));
    assert_eq!(
        item(&plan_changed, ItemKind::Pipeline, "antivirus").state,
        ItemState::Update
    );
    // a pipeline that leaves its SLA unset matches whatever Thorium has
    let mut unset = manifest.clone();
    unset
        .pipelines
        .get_mut("antivirus")
        .unwrap()
        .versions
        .get_mut("latest")
        .unwrap()
        .config
        .as_mut()
        .unwrap()
        .sla = None;
    assert_eq!(
        item(&plan(&unset, &live), ItemKind::Pipeline, "antivirus").state,
        ItemState::Unchanged
    );
    // an item Thorium lacks is created
    live.live.toolsets.clear();
    assert_eq!(
        item(&plan(&manifest, &live), ItemKind::Toolset, "kit").state,
        ItemState::Create
    );
}

/// A network policy is new, reused, or differing
#[test]
fn policies_are_new_reused_or_differing() {
    // a toolbox defining three policies
    let mut manifest = manifest_of(&small_snapshot());
    let policy = |name: &str, forced: bool| thorium::models::NetworkPolicyRequest {
        name: name.to_owned(),
        groups: vec!["static".to_owned()],
        ingress: None,
        egress: None,
        forced_policy: forced,
        default_policy: false,
    };
    manifest.network_policies = vec![
        policy("new", false),
        policy("same", false),
        policy("differs", true),
    ];
    let mut live = LiveView::default();
    live.policies
        .insert("same".to_owned(), Some(policy("same", false)));
    live.policies
        .insert("differs".to_owned(), Some(policy("differs", false)));
    live.policies.insert("new".to_owned(), None);
    let plan = plan(&manifest, &live);
    let states: Vec<_> = plan
        .policies
        .iter()
        .map(|p| (p.name.as_str(), p.state))
        .collect();
    assert_eq!(
        states,
        [
            ("differs", PolicyState::Differs),
            ("new", PolicyState::Create),
            ("same", PolicyState::Reuse),
        ]
    );
}

/// The report lists changed versions one per line and counts the rest
#[test]
fn the_report_lists_changes_and_counts_the_rest() {
    // a toolbox with an unchanged version, a conflict, and a new tool
    let manifest = manifest_of(&small_snapshot());
    let mut live = imported(&manifest);
    live.tools[0].instances[0].spec_hash = "another".to_owned();
    let mut manifest = manifest;
    manifest
        .tools
        .insert("fresh".to_owned(), manifest.tools["clamav"].clone());
    let report = DiffReport::new(plan(&manifest, &live), Vec::new());
    let text = report.lines(false).join("\n");
    assert!(text.contains("~ clamav@2.0.0 (label conflict"), "{text}");
    assert!(text.contains("+ fresh@1.0.0 (new)"), "{text}");
    assert!(!text.contains("clamav@1.0.0"), "{text}");
    assert!(
        text.contains("tool versions: 2 new, 0 new version, 1 label conflict, 1 unchanged"),
        "{text}"
    );
    assert!(report.changed);
    // the JSON document has the plan, the counts, and the flag
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["changed"], true);
    assert_eq!(value["versions"]["label_conflict"], 1);
    assert!(value["plan"]["items"].is_array());
    // nothing to do reads as unchanged
    let manifest = manifest_of(&small_snapshot());
    let report = DiffReport::new(plan(&manifest, &imported(&manifest)), Vec::new());
    assert!(!report.changed);
}

/// A group override moves every item before the comparison
#[test]
fn the_group_override_replaces_every_group() {
    // a toolbox in group `static`
    let mut manifest = manifest_of(&small_snapshot());
    override_groups(&mut manifest, "elsewhere");
    assert_eq!(manifest.named_groups(), ["elsewhere"]);
    assert_eq!(manifest.tools["clamav"].groups, ["elsewhere"]);
    assert_eq!(manifest.toolsets["kit"].groups, ["elsewhere"]);
    let config = manifest.pipelines["antivirus"].versions["latest"]
        .config
        .as_ref()
        .unwrap();
    assert_eq!(config.group, "elsewhere");
}

/// A legacy toolbox is compared as the tools its images convert to
#[tokio::test]
async fn legacy_images_are_compared_as_tools() {
    // the small legacy toolbox has an image with a config
    let text = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/toolbox_v2/small.legacy.json"
    ));
    let bar = Bar::new_or_quiet("test", "test", BarKind::UnboundIO, true);
    let mut manifest = parse_any(text.as_bytes(), None, &bar)
        .await
        .unwrap()
        .manifest;
    let version = &manifest.tools["clamav"].versions["latest"];
    assert!(version.spec.is_none() && version.legacy.is_some());
    // converting gives the version the spec of the image's config
    let mut notes = Vec::new();
    fill_legacy_specs(&mut manifest, &mut notes);
    let spec = manifest.tools["clamav"].versions["latest"]
        .spec
        .as_ref()
        .expect("a spec");
    assert!(spec.get("Image").is_some(), "{spec}");
    // and an import of it into an empty Thorium would create it
    let planned = plan(&manifest, &LiveView::default());
    assert_eq!(
        item(&planned, ItemKind::Tool, "clamav").versions[0].class,
        VersionClass::New
    );
}
