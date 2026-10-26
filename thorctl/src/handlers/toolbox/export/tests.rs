//! Tests for exporting tools, toolsets, agents, workflows, and pipelines as a toolbox

use chrono::Utc;
use clap::Parser;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use thorium::models::{
    AgentEntity, Entity, EntityKinds, EntityMetadata, NetworkPolicyRequest, Pipeline, ToolEntity,
    ToolKind, ToolSetEntity, ToolSetSelector, WorkflowCatalogEntity,
};
use uuid::Uuid;

use super::files::{assign_dirs, item_files};
use super::live::{VersionFilter, wanted_instances};
use super::snapshot::{BuildOptions, SnapInstance, SnapTool, Snapshot, build_manifest};
use super::{Action, OnConflict, Selection, reconcile};
use crate::args::toolbox::Toolbox;
use crate::handlers::toolbox::v2::load::ConvertReport;
use crate::handlers::toolbox::v2::plan_types::{ItemKind, ItemRef};
use crate::handlers::toolbox::v2::refs::uuid_paths;
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;
use crate::handlers::toolbox::v2::validate::{Severity, validate_manifest};

mod paging;

/// A plain Image tool spec for a container image
///
/// # Arguments
///
/// * `image` - The container image
pub(in crate::handlers::toolbox) fn image_spec(image: &str) -> Value {
    // start from a valid generated spec and set its image
    let mut spec =
        serde_json::to_value(thorium::test_utilities::generators::gen_image_spec()).unwrap();
    spec["Image"]["environment"]["K8s"]["image"] = json!(image);
    spec
}

/// An agentic tool spec backed by a container that pins a tool instance
///
/// # Arguments
///
/// * `function` - The function name
/// * `instance` - The pinned Image tool instance
pub(in crate::handlers::toolbox) fn container_spec(function: &str, instance: Uuid) -> Value {
    // an agentic tool whose container backing names the instance
    json!({"Agentic": {"function_name": function, "model_description": "does a thing", "timeout": 60,
        "input_schema": {"type": "object", "properties": {}},
        "backing": {"Container": {"tool_instance": instance}}}})
}

/// Build the entity of a tool root
///
/// # Arguments
///
/// * `id` - The tool's id
/// * `name` - The tool's name
/// * `groups` - The tool's groups
/// * `kind` - The kind of tool
/// * `latest` - The id of the tool's latest instance
pub(in crate::handlers::toolbox) fn tool_entity(
    id: Uuid,
    name: &str,
    groups: &[&str],
    kind: ToolKind,
    latest: Option<Uuid>,
) -> Entity {
    // a tool root with no instances listed (they are read separately)
    let mut meta = ToolEntity::new(kind, Utc::now());
    meta.latest = latest;
    Entity {
        id,
        name: name.to_owned(),
        kind: EntityKinds::Tool,
        metadata: EntityMetadata::Tool(meta),
        description: Some(format!("the {name} tool")),
        submitter: "europa".to_owned(),
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        tags: HashMap::new(),
        image: None,
        created: Utc::now(),
    }
}

/// Build one loaded version of a tool
///
/// # Arguments
///
/// * `id` - The instance's id
/// * `label` - The version label
/// * `spec` - The version's spec
pub(in crate::handlers::toolbox) fn instance(id: Uuid, label: &str, spec: Value) -> SnapInstance {
    // a loaded, non-deprecated instance
    SnapInstance {
        id,
        label: label.to_owned(),
        spec_hash: format!("hash-{label}"),
        deprecated: false,
        spec: Some(spec),
        provides: Vec::new(),
    }
}

/// Build a loaded tool whose latest is its first instance
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `groups` - The tool's groups
/// * `kind` - The kind of tool
/// * `instances` - The tool's instances, newest first
pub(in crate::handlers::toolbox) fn snap_tool(
    name: &str,
    groups: &[&str],
    kind: ToolKind,
    instances: Vec<SnapInstance>,
) -> SnapTool {
    // the newest instance is the tool's latest
    let latest = instances.first().map(|instance| instance.id);
    let entity = tool_entity(Uuid::new_v4(), name, groups, kind, latest);
    SnapTool::from_entity(&entity, instances).expect("a tool entity")
}

/// Build a pipeline with one stage per tool
///
/// # Arguments
///
/// * `group` - The pipeline's group
/// * `name` - The pipeline's name
/// * `stages` - Each stage's name and the id of the tool (or instance) behind it
pub(in crate::handlers::toolbox) fn pipeline(
    group: &str,
    name: &str,
    stages: &[(&str, Uuid)],
) -> Pipeline {
    // the stages run in the order given, one after the other
    Pipeline {
        group: group.to_owned(),
        name: name.to_owned(),
        creator: "europa".to_owned(),
        order: stages
            .iter()
            .map(|(stage, _)| vec![(*stage).to_owned()])
            .collect(),
        sla: 600,
        triggers: HashMap::new(),
        description: None,
        bans: HashMap::new(),
        stage_tools: stages
            .iter()
            .map(|(stage, id)| ((*stage).to_owned(), *id))
            .collect(),
    }
}

/// Build the manifest of a snapshot with default options, asserting it works
///
/// # Arguments
///
/// * `snapshot` - The snapshot to build from
pub(in crate::handlers::toolbox) fn built(snapshot: &Snapshot) -> ToolboxManifestV2 {
    // build with no override and refuse nothing
    build_manifest("moons", snapshot, &BuildOptions::default())
        .expect("the manifest builds")
        .manifest
}

/// Get the errors validating a manifest finds
///
/// # Arguments
///
/// * `manifest` - The manifest to validate
fn errors(manifest: &ToolboxManifestV2) -> Vec<String> {
    // validate as `toolbox validate` does and keep the errors
    validate_manifest(manifest, &ConvertReport::default(), &[])
        .into_iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(|issue| format!("{}: {}", issue.code, issue.message))
        .collect()
}

/// An export holds only v2 sections, names every version's spec, and keeps each tool's latest
#[test]
fn export_writes_only_v2_sections() {
    // a tool with two versions and a pipeline over it
    let latest = Uuid::new_v4();
    let tool = snap_tool(
        "clamav",
        &["static"],
        ToolKind::Image,
        vec![
            instance(latest, "latest", image_spec("scan:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("scan:1")),
        ],
    );
    let stage = tool.id;
    let snapshot = Snapshot {
        tools: vec![tool],
        pipelines: vec![pipeline("static", "antivirus", &[("clamav", stage)])],
        ..Snapshot::default()
    };
    let manifest = built(&snapshot);
    // the document has the v2 version and sections and no legacy ones
    let document = serde_json::to_value(&manifest).unwrap();
    assert_eq!(document["format_version"], 2);
    assert!(document.get("images").is_none());
    for key in document.as_object().unwrap().keys() {
        assert!(
            ["format_version", "name", "groups", "tools", "pipelines"].contains(&key.as_str()),
            "unexpected section {key}"
        );
    }
    // every version carries its spec, and the tool's latest label is kept
    let clamav = &manifest.tools["clamav"];
    assert_eq!(clamav.versions.len(), 2);
    assert!(
        clamav
            .versions
            .values()
            .all(|version| version.spec.is_some())
    );
    assert_eq!(clamav.latest.as_deref(), Some("latest"));
    assert_eq!(clamav.publish_order(), ["1.0.0", "latest"]);
    assert_eq!(clamav.groups, ["static"]);
    // the document reads back as a manifest and validates
    let text = serde_json::to_string_pretty(&manifest).unwrap();
    let again: ToolboxManifestV2 = serde_json::from_str(&text).unwrap();
    assert_eq!(again.counts(), manifest.counts());
    assert_eq!(errors(&manifest), Vec::<String>::new());
}

/// Building twice gives the same bytes
#[test]
fn export_is_deterministic() {
    // two tools whose order must not depend on how they were read
    let snapshot = Snapshot {
        tools: vec![
            snap_tool(
                "zeta",
                &["g"],
                ToolKind::Image,
                vec![instance(Uuid::new_v4(), "1", image_spec("z:1"))],
            ),
            snap_tool(
                "alpha",
                &["g"],
                ToolKind::Image,
                vec![instance(Uuid::new_v4(), "1", image_spec("a:1"))],
            ),
        ],
        ..Snapshot::default()
    };
    let first = serde_json::to_string_pretty(&built(&snapshot)).unwrap();
    let second = serde_json::to_string_pretty(&built(&snapshot)).unwrap();
    assert_eq!(first, second);
    assert!(first.find("alpha").unwrap() < first.find("zeta").unwrap());
}

/// Dependency tool ids, container pins, and stage tools are written as names
#[test]
fn ids_become_references() {
    // an image tool, a tool that depends on it, and an agentic tool pinning its first version
    let base = snap_tool(
        "base",
        &["g"],
        ToolKind::Image,
        vec![
            instance(Uuid::new_v4(), "2.0.0", image_spec("base:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("base:1")),
        ],
    );
    let pinned = base.instances[1].id;
    let mut dependent_spec = image_spec("dependent:1");
    dependent_spec["Image"]["data"]["dependencies"]["results"]["tools"] = json!([base.id]);
    let dependent = snap_tool(
        "dependent",
        &["g"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "1.0.0", dependent_spec)],
    );
    let agentic = snap_tool(
        "caller",
        &["g"],
        ToolKind::Agentic,
        vec![instance(
            Uuid::new_v4(),
            "1.0.0",
            container_spec("call_base", pinned),
        )],
    );
    // a pipeline following base's latest and one pinning its first version
    let snapshot = Snapshot {
        pipelines: vec![
            pipeline("g", "follows", &[("base", base.id)]),
            pipeline("g", "pins", &[("base", pinned)]),
        ],
        tools: vec![base, dependent, agentic],
        ..Snapshot::default()
    };
    let manifest = built(&snapshot);
    let spec_of = |name: &str| manifest.tools[name].versions["1.0.0"].spec.clone().unwrap();
    // the dependency names the tool and the container backing names the pinned version
    assert_eq!(
        spec_of("dependent")["Image"]["data"]["dependencies"]["results"]["tools"],
        json!(["ref:tool/base"])
    );
    assert_eq!(
        spec_of("caller")["Agentic"]["backing"]["Container"]["tool_instance"],
        "ref:tool/base@1.0.0"
    );
    // no id is left in any spec
    for tool in manifest.tools.values() {
        for version in tool.versions.values() {
            assert_eq!(
                uuid_paths(version.spec.as_ref().unwrap()),
                Vec::<String>::new()
            );
        }
    }
    // a tool id follows latest and an instance id pins its label
    let stage = |pipeline: &str| {
        manifest.pipelines[pipeline].versions["latest"].stage_tools["base"].clone()
    };
    assert_eq!(
        (stage("follows").tool.as_str(), stage("follows").version),
        ("base", None)
    );
    assert_eq!(stage("pins").version.as_deref(), Some("1.0.0"));
    assert_eq!(errors(&manifest), Vec::<String>::new());
}

/// A harness's provided tools and its env settings are exported by name
#[test]
fn harness_settings_are_exported() {
    // a native tool and a harness providing its first version
    let native = snap_tool(
        "native",
        &["g"],
        ToolKind::Agentic,
        vec![instance(
            Uuid::new_v4(),
            "1.0.0",
            json!({"Agentic": {"function_name": "native", "model_description": "x",
                "input_schema": {"type": "object", "properties": {}},
                "backing": {"HarnessNative": {"tool": "native"}}}}),
        )],
    );
    let mut harness = snap_tool(
        "pal",
        &["g"],
        ToolKind::Harness,
        vec![instance(
            Uuid::new_v4(),
            "1.0.0",
            json!({"Harness": {"workload": {"environment": {"K8s": {"image": "pal:1",
                "scheduling": {"resources": {"cpu": 1000, "memory": 512}}}}}}}),
        )],
    );
    harness.instances[0].provides = vec![native.instances[0].id];
    harness.harness_env = vec!["OpenAiApiKey=LITELLM_API_KEY".to_owned()];
    harness.disabled_models = vec!["gpt-5".to_owned()];
    let manifest = built(&Snapshot {
        tools: vec![native, harness],
        ..Snapshot::default()
    });
    let pal = &manifest.tools["pal"];
    assert_eq!(pal.harness_env, ["OpenAiApiKey=LITELLM_API_KEY"]);
    assert_eq!(pal.disabled_models, ["gpt-5"]);
    let provides = &pal.versions["1.0.0"].provides;
    assert_eq!(provides.len(), 1);
    assert_eq!(
        (provides[0].tool.as_str(), provides[0].version.as_deref()),
        ("native", Some("1.0.0"))
    );
}

/// Deprecated versions are skipped unless asked for, and `--latest-only` keeps the latest
#[test]
fn version_filters_pick_instances() {
    // three versions, the newest deprecated and the middle one the latest
    let mut tool = snap_tool(
        "scan",
        &["g"],
        ToolKind::Image,
        vec![
            instance(Uuid::new_v4(), "3.0.0", image_spec("s:3")),
            instance(Uuid::new_v4(), "2.0.0", image_spec("s:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("s:1")),
        ],
    );
    tool.instances[0].deprecated = true;
    tool.latest = Some(tool.instances[1].id);
    let ids = |tool: &SnapTool, filter| {
        let wanted = wanted_instances(tool, filter);
        tool.instances
            .iter()
            .filter(|instance| wanted.contains(&instance.id))
            .map(|instance| instance.label.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&tool, VersionFilter::default()), ["2.0.0", "1.0.0"]);
    assert_eq!(
        ids(
            &tool,
            VersionFilter {
                include_deprecated: true,
                latest_only: false
            }
        ),
        ["3.0.0", "2.0.0", "1.0.0"]
    );
    assert_eq!(
        ids(
            &tool,
            VersionFilter {
                include_deprecated: false,
                latest_only: true
            }
        ),
        ["2.0.0"]
    );
    // a tool with only deprecated versions keeps its newest
    for instance in &mut tool.instances {
        instance.deprecated = true;
    }
    assert_eq!(ids(&tool, VersionFilter::default()), ["3.0.0"]);
}

/// A version something pins is asked for even when the filter would skip it
#[test]
fn pinned_versions_are_needed_until_read() {
    // a tool pinning the second version of a tool whose spec was not read
    let mut base = snap_tool(
        "base",
        &["g"],
        ToolKind::Image,
        vec![
            instance(Uuid::new_v4(), "2.0.0", image_spec("b:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("b:1")),
        ],
    );
    let pinned = base.instances[1].id;
    base.instances[1].spec = None;
    let caller = snap_tool(
        "caller",
        &["g"],
        ToolKind::Agentic,
        vec![instance(
            Uuid::new_v4(),
            "1.0.0",
            container_spec("call", pinned),
        )],
    );
    let mut snapshot = Snapshot {
        tools: vec![base, caller],
        ..Snapshot::default()
    };
    // the pinned instance is needed, and nothing else
    let needed = snapshot.needed();
    assert_eq!(
        needed.instances.iter().copied().collect::<Vec<_>>(),
        [pinned]
    );
    assert!(needed.tools.is_empty() && needed.unknown.is_empty());
    // once its spec is read nothing is needed
    snapshot.tools[0].instances[1].spec = Some(image_spec("b:1"));
    assert!(snapshot.needed().is_empty());
    // an id the snapshot knows nothing about is looked up, and ignoring it stops the asking
    let unknown = Uuid::new_v4();
    snapshot.pipelines = vec![pipeline("g", "p", &[("s", unknown)])];
    assert!(snapshot.needed().unknown.contains(&unknown));
    snapshot.ignored.insert(unknown);
    assert!(snapshot.needed().is_empty());
}

/// Two different tools with one name can't share a manifest
#[test]
fn duplicate_names_are_refused() {
    // the same tool name in two groups
    let snapshot = Snapshot {
        tools: vec![
            snap_tool(
                "scan",
                &["a"],
                ToolKind::Image,
                vec![instance(Uuid::new_v4(), "1", image_spec("a:1"))],
            ),
            snap_tool(
                "scan",
                &["b"],
                ToolKind::Image,
                vec![instance(Uuid::new_v4(), "1", image_spec("b:1"))],
            ),
        ],
        ..Snapshot::default()
    };
    let err = build_manifest("moons", &snapshot, &BuildOptions::default())
        .expect_err("two tools named scan");
    assert!(
        err.to_string().contains("Two tools are named 'scan'"),
        "{err}"
    );
}

/// `--group-override` rewrites every item's group
#[test]
fn group_override_moves_items() {
    // a tool and a pipeline in group `a`
    let tool = snap_tool(
        "scan",
        &["a"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "1", image_spec("a:1"))],
    );
    let stage = tool.id;
    let snapshot = Snapshot {
        tools: vec![tool],
        pipelines: vec![pipeline("a", "p", &[("scan", stage)])],
        ..Snapshot::default()
    };
    let options = BuildOptions {
        group_override: Some("z".to_owned()),
        ..BuildOptions::default()
    };
    let manifest = build_manifest("moons", &snapshot, &options)
        .unwrap()
        .manifest;
    assert_eq!(manifest.tools["scan"].groups, ["z"]);
    let config = manifest.pipelines["p"].versions["latest"]
        .config
        .as_ref()
        .unwrap();
    assert_eq!(config.group, "z");
    assert_eq!(manifest.named_groups(), ["z"]);
}

/// An exported network policy names only the exported groups: the override group with
/// `--group-override`, else the groups of the exported items, never other groups that hold it
#[test]
fn exported_network_policies_name_only_exported_groups() {
    // a tool in group `a` and the policy it names, which groups `a` and `b` hold
    let tool = snap_tool(
        "scan",
        &["a"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "1", image_spec("a:1"))],
    );
    let policy = NetworkPolicyRequest {
        name: "allow-all".to_owned(),
        groups: vec!["a".to_owned(), "b".to_owned()],
        ingress: None,
        egress: None,
        forced_policy: false,
        default_policy: false,
    };
    let snapshot = Snapshot {
        tools: vec![tool],
        policies: vec![policy],
        ..Snapshot::default()
    };
    // the override puts the policy in the override group only
    let options = BuildOptions {
        group_override: Some("z".to_owned()),
        ..BuildOptions::default()
    };
    let manifest = build_manifest("moons", &snapshot, &options)
        .unwrap()
        .manifest;
    assert_eq!(manifest.network_policies.len(), 1);
    assert_eq!(manifest.network_policies[0].name, "allow-all");
    assert_eq!(manifest.network_policies[0].groups, ["z"]);
    assert_eq!(manifest.named_groups(), ["z"]);
    // without an override the policy is in the exported items' groups only
    let manifest = build_manifest("moons", &snapshot, &BuildOptions::default())
        .unwrap()
        .manifest;
    assert_eq!(manifest.network_policies.len(), 1);
    assert_eq!(manifest.network_policies[0].groups, ["a"]);
}

/// A tool version's directory carries the network policies its spec names, as policy files its
/// manifest bundles, so a build of the exported directory brings the policies along
#[test]
fn tool_files_bundle_the_network_policies_their_spec_names() {
    // a tool whose spec names the policy the export holds, and one it does not hold
    let mut spec = image_spec("a:1");
    spec["Image"]["environment"]["K8s"]["network_policies"] = json!(["allow-all", "elsewhere"]);
    let tool = snap_tool(
        "scan",
        &["a"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "1", spec)],
    );
    let policy = NetworkPolicyRequest {
        name: "allow-all".to_owned(),
        groups: vec!["a".to_owned()],
        ingress: None,
        egress: None,
        forced_policy: false,
        default_policy: false,
    };
    let snapshot = Snapshot {
        tools: vec![tool],
        policies: vec![policy],
        ..Snapshot::default()
    };
    let mut manifest = build_manifest("moons", &snapshot, &BuildOptions::default())
        .unwrap()
        .manifest;
    assign_dirs(&mut manifest, &HashMap::new()).unwrap();
    let files = item_files(
        &manifest,
        &ItemRef::new(ItemKind::Tool, "scan"),
        &HashMap::new(),
    )
    .unwrap();
    // the manifest bundles the held policy only, and its file is the policy as JSON
    let table: toml::Table = files[0].content.parse().unwrap();
    assert_eq!(
        table["network_policies_from"],
        toml::Value::Array(vec![toml::Value::String(
            "allow-all.policy.json".to_owned()
        )])
    );
    let policy_file = files
        .iter()
        .find(|file| file.path.ends_with("allow-all.policy.json"))
        .expect("the policy file");
    let written: NetworkPolicyRequest = serde_json::from_str(&policy_file.content).unwrap();
    assert_eq!(written.name, "allow-all");
    assert_eq!(written.groups, ["a"]);
    assert!(policy_file.content.ends_with("}\n"));
    assert_eq!(files.len(), 3);
}

/// A pipeline with a stage no tool backs is left out with a warning
#[test]
fn pipelines_without_stage_tools_are_skipped() {
    // a pipeline whose stage has no tool entry
    let mut broken = pipeline("g", "broken", &[("scan", Uuid::new_v4())]);
    broken.stage_tools.clear();
    let snapshot = Snapshot {
        pipelines: vec![broken],
        ..Snapshot::default()
    };
    let built = build_manifest("moons", &snapshot, &BuildOptions::default()).unwrap();
    assert!(built.manifest.pipelines.is_empty());
    assert!(
        built
            .warnings
            .iter()
            .any(|warning| warning.contains("pipeline broken")),
        "{:?}",
        built.warnings
    );
}

/// Toolsets, agents, and workflows are exported with their edges as names
#[test]
fn entities_are_exported_with_named_edges() {
    // a tool, a toolset including it, an agent using both, and a workflow running the agent
    let tool = snap_tool(
        "strings",
        &["g"],
        ToolKind::Agentic,
        vec![instance(
            Uuid::new_v4(),
            "1.0.0",
            json!({"Agentic": {"function_name": "strings", "model_description": "x",
                "input_schema": {"type": "object", "properties": {}},
                "backing": {"HarnessNative": {"tool": "strings"}}}}),
        )],
    );
    let tool_root = tool_entity(tool.id, "strings", &["g"], ToolKind::Agentic, None);
    let instance_entity = Entity {
        id: tool.instances[0].id,
        name: "strings@1.0.0".to_owned(),
        kind: EntityKinds::ToolInstance,
        metadata: EntityMetadata::ToolInstance(thorium::models::ToolInstanceEntity {
            tool: tool.id,
            version: thorium::models::ToolVersion::from("1.0.0"),
            previous: None,
            spec_hash: "h".to_owned(),
            deprecated: false,
            spec: thorium::models::ToolSpec::default(),
            portability: None,
            provides: Vec::new(),
        }),
        ..tool_root.clone()
    };
    let toolset = Entity {
        id: Uuid::new_v4(),
        name: "kit".to_owned(),
        kind: EntityKinds::ToolSet,
        metadata: EntityMetadata::ToolSet(ToolSetEntity {
            selector: Some(ToolSetSelector {
                key: "category".to_owned(),
                value: "triage".to_owned(),
            }),
            tools: vec![tool_root.clone()],
            ..ToolSetEntity::default()
        }),
        ..tool_root.clone()
    };
    let agent = Entity {
        id: Uuid::new_v4(),
        name: "triage".to_owned(),
        kind: EntityKinds::Agent,
        metadata: EntityMetadata::Agent(AgentEntity {
            latest_tools: vec![tool_root.clone()],
            pinned_tools: vec![instance_entity],
            tool_sets: vec![toolset.clone()],
            ..AgentEntity::default()
        }),
        ..tool_root.clone()
    };
    let workflow = Entity {
        id: Uuid::new_v4(),
        name: "flow".to_owned(),
        kind: EntityKinds::WorkflowCatalog,
        metadata: EntityMetadata::WorkflowCatalog(WorkflowCatalogEntity {
            entry_agent: Some(agent.id),
            agents: vec![agent.clone()],
            ..WorkflowCatalogEntity::default()
        }),
        ..tool_root.clone()
    };
    let manifest = built(&Snapshot {
        tools: vec![tool],
        toolsets: vec![toolset],
        agents: vec![agent],
        workflows: vec![workflow],
        ..Snapshot::default()
    });
    // the toolset includes the tool and keeps its selector
    let kit = &manifest.toolsets["kit"];
    assert_eq!(kit.selector.as_deref(), Some("category=triage"));
    assert_eq!(kit.tools[0].tool, "strings");
    // the agent follows the tool, pins its version, and uses the toolset
    let triage = &manifest.agents["triage"];
    let tools: Vec<_> = triage
        .tools
        .iter()
        .map(|tool| (tool.tool.as_str(), tool.version.as_deref()))
        .collect();
    assert_eq!(tools, [("strings", None), ("strings", Some("1.0.0"))]);
    assert_eq!(triage.toolsets[0].toolset, "kit");
    // the workflow runs the agent and starts with it
    let flow = &manifest.workflows["flow"];
    assert_eq!(flow.agents, ["triage"]);
    assert_eq!(flow.entry_agent.as_deref(), Some("triage"));
    assert_eq!(errors(&manifest), Vec::<String>::new());
}

/// `--strip-registry` clears the image url of image tools
#[test]
fn strip_registry_clears_image_urls() {
    // an image tool with a pinned url
    let snapshot = Snapshot {
        tools: vec![snap_tool(
            "scan",
            &["g"],
            ToolKind::Image,
            vec![instance(
                Uuid::new_v4(),
                "1",
                image_spec("ghcr.io/a/scan:1"),
            )],
        )],
        ..Snapshot::default()
    };
    let options = BuildOptions {
        strip_registry: true,
        ..BuildOptions::default()
    };
    let manifest = build_manifest("moons", &snapshot, &options)
        .unwrap()
        .manifest;
    let spec = manifest.tools["scan"].versions["1"].spec.as_ref().unwrap();
    assert_eq!(spec["Image"]["environment"]["K8s"]["image"], "");
}

/// Items land in per-item directories whose files read back as what the manifest holds
#[test]
fn files_follow_the_directory_layout() {
    // a tool with two versions, a pipeline, and a toolset
    let tool = snap_tool(
        "clamav",
        &["static"],
        ToolKind::Image,
        vec![
            instance(Uuid::new_v4(), "latest", image_spec("c:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("c:1")),
        ],
    );
    let stage = tool.id;
    let toolset = Entity {
        id: Uuid::new_v4(),
        name: "kit".to_owned(),
        kind: EntityKinds::ToolSet,
        metadata: EntityMetadata::ToolSet(ToolSetEntity::default()),
        ..tool_entity(Uuid::new_v4(), "x", &["static"], ToolKind::Image, None)
    };
    let mut manifest = built(&Snapshot {
        tools: vec![tool],
        toolsets: vec![toolset],
        pipelines: vec![pipeline("static", "antivirus", &[("clamav", stage)])],
        ..Snapshot::default()
    });
    assign_dirs(&mut manifest, &HashMap::new()).unwrap();
    // each version of a tool with several is in its own directory
    assert_eq!(
        manifest.tools["clamav"].versions["1.0.0"].dir,
        "tools/static/clamav/1.0.0"
    );
    assert_eq!(
        manifest.tools["clamav"].versions["latest"].dir,
        "tools/static/clamav/latest"
    );
    assert_eq!(
        manifest.pipelines["antivirus"].versions["latest"].dir,
        "pipelines/static/antivirus"
    );
    // a version's files are its manifest.toml and its spec as JSON
    let files = item_files(
        &manifest,
        &ItemRef::new(ItemKind::Tool, "clamav"),
        &HashMap::new(),
    )
    .unwrap();
    let paths: Vec<_> = files
        .iter()
        .map(|file| file.path.display().to_string())
        .collect();
    assert!(paths.contains(&"tools/static/clamav/latest/manifest.toml".to_owned()));
    assert!(paths.contains(&"tools/static/clamav/1.0.0/clamav.json".to_owned()));
    let table: toml::Table = files[0].content.parse().unwrap();
    assert_eq!(table["type"].as_str(), Some("tool"));
    assert_eq!(table["spec_from"].as_str(), Some("clamav.json"));
    assert!(files[1].content.ends_with("}\n"));
    let json: Value = serde_json::from_str(&files[1].content).unwrap();
    assert_eq!(
        json,
        manifest.tools["clamav"].versions["1.0.0"]
            .spec
            .clone()
            .unwrap()
    );
    // a pipeline's manifest names its stage tools and its config file holds no stage tools
    let files = item_files(
        &manifest,
        &ItemRef::new(ItemKind::Pipeline, "antivirus"),
        &HashMap::new(),
    )
    .unwrap();
    let table: toml::Table = files[0].content.parse().unwrap();
    assert_eq!(
        table["stage_tools"]["clamav"]["tool"].as_str(),
        Some("clamav")
    );
    let config: Value = serde_json::from_str(&files[1].content).unwrap();
    assert_eq!(config["stage_tools"], json!({}));
    // a toolset has a manifest and a YAML document
    let files = item_files(
        &manifest,
        &ItemRef::new(ItemKind::Toolset, "kit"),
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(
        files[1].path,
        PathBuf::from("toolsets/static/kit/kit.toolset.yaml")
    );
}

/// A named placement replaces the default directory, and one that escapes the toolbox is refused
#[test]
fn placements_stay_inside_the_toolbox() {
    // a tool placed in a chosen directory
    let tool = snap_tool(
        "clamav",
        &["static"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "latest", image_spec("c:1"))],
    );
    let mut manifest = built(&Snapshot {
        tools: vec![tool],
        ..Snapshot::default()
    });
    let overrides = HashMap::from([(
        ItemRef::new(ItemKind::Tool, "clamav"),
        "mine/clamav".to_owned(),
    )]);
    assign_dirs(&mut manifest, &overrides).unwrap();
    assert_eq!(
        manifest.tools["clamav"].versions["latest"].dir,
        "mine/clamav"
    );
    // a name that could climb out of the toolbox can't be a directory
    let tool = snap_tool(
        "..",
        &["static"],
        ToolKind::Image,
        vec![instance(Uuid::new_v4(), "latest", image_spec("c:1"))],
    );
    let mut manifest = built(&Snapshot {
        tools: vec![tool],
        ..Snapshot::default()
    });
    assert!(assign_dirs(&mut manifest, &HashMap::new()).is_err());
    // the selection refuses an absolute placement and one with `..`
    for dest in ["/abs/dir", "../up"] {
        let selection = Selection {
            tools: vec![crate::args::toolbox::ResourceSpec {
                group: "g".to_owned(),
                name: "t".to_owned(),
                dest: Some(dest.to_owned()),
            }],
            ..Selection::default()
        };
        assert!(super::dir_overrides(&selection).is_err(), "{dest}");
    }
}

/// Items already in the output are unchanged, replaced, left, or refused as asked
#[test]
fn reconcile_applies_the_conflict_policy() {
    // a manifest with two tools, and an output that holds one the same and one that differs
    let make = |image: &str| {
        let mut manifest = built(&Snapshot {
            tools: vec![
                snap_tool(
                    "same",
                    &["g"],
                    ToolKind::Image,
                    vec![instance(Uuid::new_v4(), "1", image_spec("same:1"))],
                ),
                snap_tool(
                    "changed",
                    &["g"],
                    ToolKind::Image,
                    vec![instance(Uuid::new_v4(), "1", image_spec(image))],
                ),
            ],
            ..Snapshot::default()
        });
        assign_dirs(&mut manifest, &HashMap::new()).unwrap();
        manifest
    };
    let new = make("changed:2");
    let actions = |policy| {
        let mut merged = make("changed:1");
        reconcile(&new, &mut merged, policy).map(|actions| {
            let spec = merged.tools["changed"].versions["1"].spec.clone().unwrap();
            (
                actions,
                spec["Image"]["environment"]["K8s"]["image"].clone(),
            )
        })
    };
    // overwrite replaces the differing tool
    let (done, image) = actions(OnConflict::Overwrite).unwrap();
    assert_eq!(
        done.iter().map(|(_, action)| *action).collect::<Vec<_>>(),
        [Action::Replace, Action::Unchanged]
    );
    assert_eq!(image, "changed:2");
    // skip leaves it as it was
    let (done, image) = actions(OnConflict::Skip).unwrap();
    assert_eq!(done[0].1, Action::Skipped);
    assert_eq!(image, "changed:1");
    // with no policy the export stops and names the item
    let err = actions(OnConflict::Fail).unwrap_err();
    assert!(err.to_string().contains("tool changed"), "{err}");
    // into an empty output everything is created
    let mut empty = ToolboxManifestV2::new("moons");
    let done = reconcile(&new, &mut empty, OnConflict::Fail).unwrap();
    assert!(done.iter().all(|(_, action)| *action == Action::Create));
    assert_eq!(empty.tools.len(), 2);
}

/// The command line selects items with the new flags and keeps `-i/--images` as hidden aliases
#[test]
fn selection_parses_the_flags_and_aliases() {
    // parse an export command line into its selection
    let parse = |argv: &[&str]| {
        let args = crate::args::Args::try_parse_from(
            ["thorctl", "toolbox", "export"]
                .into_iter()
                .chain(argv.iter().copied()),
        )
        .unwrap_or_else(|err| panic!("{argv:?} did not parse: {err}"));
        let crate::args::SubCommands::Toolbox(Toolbox::Export(cmd)) = args.cmd else {
            panic!("not an export command");
        };
        Selection::from_cmd(&cmd).unwrap()
    };
    // a group alone selects the whole group, and `--all-kinds` adds the other kinds
    let selection = parse(&["-g", "static"]);
    assert_eq!(selection.whole_group.as_deref(), Some("static"));
    assert!(selection.wants(ItemKind::Tool) && selection.wants(ItemKind::Pipeline));
    assert!(!selection.wants(ItemKind::Agent));
    assert!(parse(&["-g", "static", "--all-kinds"]).wants(ItemKind::Workflow));
    // named items resolve bare names against the group and select only their kinds
    let selection = parse(&["-g", "static", "-t", "a,b/c=dir", "--agents", "bot"]);
    assert_eq!(selection.whole_group, None);
    assert_eq!(selection.tools.len(), 2);
    assert_eq!(selection.tools[0].group, "static");
    assert_eq!(selection.tools[1].dest.as_deref(), Some("dir"));
    assert!(selection.wants(ItemKind::Agent) && !selection.wants(ItemKind::Pipeline));
    // the old image flags select tools
    for argv in [["-i", "static/a"], ["--images", "static/a"]] {
        assert_eq!(parse(&argv).tools.len(), 1, "{argv:?}");
    }
}
