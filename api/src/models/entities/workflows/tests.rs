//! Tests for the workflow catalog and workflow instance models

use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hash};
use uuid::Uuid;

use super::{
    InstancedAgent, InstancedAgentToolset, InstancedTool, InstancedToolBinding, InstancedToolSpec,
    RESULT_KEY_DELIMITER, WorkflowCatalogEntity, WorkflowCatalogEntityRequest,
    WorkflowCatalogUpdateFields, WorkflowInstanceEntity, WorkflowInstanceEntityRequest,
    WorkflowInstanceStatus, WorkflowInstanceTrigger, WorkflowLimits, WorkflowResultKey,
};
use crate::models::{
    AgentConfig, EntityMetadataRequest, EntityMetadataUpdate, ModelAuthLevel, ModelAuthSource,
    ModelMatcher, ToolKind, ToolMechanism, ToolSpec,
};

/// Serialize a value to JSON and back, checking it serializes the same both times
///
/// # Arguments
///
/// * `value` - The value to round trip
fn round_trip_json<T: Serialize + DeserializeOwned>(value: &T) {
    // compare the serialized values since not every type is comparable
    let first = serde_json::to_value(value).unwrap();
    let back: T = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), first);
}

/// Hash a value with the standard library's hasher
///
/// # Arguments
///
/// * `value` - The value to hash
fn std_hash<T: Hash>(value: &T) -> u64 {
    // a hasher with fixed keys, so equal values hash equally
    std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
        .hash_one(value)
}

/// Build a tool resolved into a workflow instance
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `spec_hash` - The tool instance's spec hash
fn tool(name: &str, spec_hash: &str) -> InstancedTool {
    InstancedTool {
        tool: Uuid::new_v4(),
        tool_instance: Uuid::new_v4(),
        name: name.to_owned(),
        groups: vec!["minerva".into()],
        kind: ToolKind::Agentic,
        version: "1.0.0".into(),
        spec_hash: spec_hash.to_owned(),
        function_name: Some(name.replace('-', "_")),
        pinned: false,
        dependency: false,
    }
}

/// Build an agent resolved into a workflow instance with its toolset digest
///
/// # Arguments
///
/// * `name` - The agent's name
/// * `tools` - The tools the agent was given
fn agent(name: &str, tools: &[&InstancedTool]) -> InstancedAgent {
    // a config that names the agent
    let config = AgentConfig {
        system_prompt: Some(format!("You are {name}")),
        function_name: Some(name.replace('-', "_")),
        model_description: Some(format!("Delegate to {name}")),
        ..AgentConfig::default()
    };
    let config_hash = config.config_hash().unwrap();
    let mut agent = InstancedAgent {
        agent: Uuid::new_v4(),
        name: name.to_owned(),
        groups: vec!["minerva".into()],
        config,
        config_hash,
        harness: None,
        tools: tools.iter().map(|tool| tool.tool_instance).collect(),
        tool_sets: Vec::new(),
        bindings: Vec::new(),
        sub_agents: Vec::new(),
        toolset_digest: String::new(),
    };
    let owned = tools.iter().map(|tool| (*tool).clone()).collect::<Vec<_>>();
    agent.toolset_digest = agent.compute_toolset_digest(&owned).unwrap();
    agent
}

/// A ready workflow instance with an entry agent that delegates to a sub-agent and every
/// field set
fn instance() -> WorkflowInstanceEntity {
    // two tools, one per agent
    let (decompile, strings) = (tool("decompile", "v1:aa"), tool("strings", "v1:bb"));
    let mut entry = agent("triage", &[&decompile]);
    let sub = agent("reverser", &[&strings]);
    entry.sub_agents.push(sub.agent);
    let mut instance = WorkflowInstanceEntity {
        workflow: Uuid::new_v4(),
        workflow_groups: vec!["minerva".into(), "mars".into()],
        revision: 2,
        previous: Some(Uuid::new_v4()),
        status: WorkflowInstanceStatus::Ready,
        warnings: vec!["decompile 0.9 is deprecated".into()],
        trigger: WorkflowInstanceTrigger::Manual,
        triggered_by: "europa".into(),
        entry_agent: Some(entry.agent),
        limits: WorkflowLimits {
            max_alive_sub_agents: Some(4),
        },
        default_models: vec![ModelMatcher {
            model_id: "claude-sonnet".into(),
            ..ModelMatcher::default()
        }],
        agents: vec![entry, sub],
        tools: vec![decompile, strings],
        digest: String::new(),
    };
    instance.digest = instance.compute_digest(&BTreeMap::new()).unwrap();
    instance
}

/// Tool bindings round-trip, default to none, sort by tool instance, and are part of the digest
#[test]
fn tool_bindings() {
    // a binding round-trips with its mechanism as a plain string
    let binding = InstancedToolBinding {
        tool_instance: Uuid::new_v4(),
        mechanism: ToolMechanism::Cli,
    };
    round_trip_json(&binding);
    let value = serde_json::to_value(binding).unwrap();
    assert_eq!(value["mechanism"], json!("Cli"));
    // an agent stored before bindings existed reads with none
    let mut stored = serde_json::to_value(&instance().agents[0]).unwrap();
    stored.as_object_mut().unwrap().remove("bindings");
    let parsed: InstancedAgent = serde_json::from_value(stored).unwrap();
    assert_eq!(parsed.bindings, Vec::new());
    // bindings sort by tool instance
    let mut sorted = instance();
    let tool = sorted.tools[0].tool_instance;
    let (low, high) = if Uuid::nil() < tool {
        (Uuid::nil(), tool)
    } else {
        (tool, Uuid::nil())
    };
    sorted.agents[0].bindings = vec![
        InstancedToolBinding {
            tool_instance: high,
            mechanism: ToolMechanism::Mcp,
        },
        InstancedToolBinding {
            tool_instance: low,
            mechanism: ToolMechanism::ThoriumApi,
        },
    ];
    sorted.sort_lists();
    let order = sorted
        .agents
        .iter()
        .find(|agent| agent.bindings.len() == 2)
        .unwrap();
    assert_eq!(order.bindings[0].tool_instance, low);
    assert_eq!(order.bindings[1].tool_instance, high);
    // an instance without bindings keeps the digest it had before bindings existed
    let base = instance();
    let entry = base.entry_agent.unwrap();
    // binding a tool changes the digest, and so does binding it to another mechanism
    let bound = |mechanism: ToolMechanism| {
        let mut changed = base.clone();
        let agent = changed
            .agents
            .iter_mut()
            .find(|agent| agent.agent == entry)
            .unwrap();
        let tool = agent.tools[0];
        agent.bindings = vec![InstancedToolBinding {
            tool_instance: tool,
            mechanism,
        }];
        changed.compute_digest(&BTreeMap::new()).unwrap()
    };
    let api = bound(ToolMechanism::ThoriumApi);
    let cli = bound(ToolMechanism::Cli);
    assert_ne!(api, base.digest);
    assert_ne!(api, cli);
    // the same bindings give the same digest
    assert_eq!(bound(ToolMechanism::Cli), cli);
}

/// Every workflow struct and enum round-trips through serde JSON
#[test]
fn serde_round_trips() {
    // a catalog with every field set
    let catalog = WorkflowCatalogEntity {
        entry_agent: Some(Uuid::new_v4()),
        auto_instance: false,
        limits: WorkflowLimits {
            max_alive_sub_agents: Some(3),
        },
        default_models: vec![ModelMatcher {
            model_id: "gpt-5".into(),
            ..ModelMatcher::default()
        }],
        model_auth: ModelAuthSource {
            levels: [ModelAuthLevel::User].into(),
        },
        agents: Vec::new(),
        modified: Utc::now(),
        latest_instance: Some(Uuid::new_v4()),
        newest_instance: Some(Uuid::new_v4()),
        highest_revision: 7,
    };
    round_trip_json(&catalog);
    round_trip_json(&catalog.limits);
    // an instance, its agents, its tools, every status, and every trigger
    let instance = instance();
    round_trip_json(&instance);
    round_trip_json(&instance.agents[0]);
    round_trip_json(&instance.tools[0]);
    round_trip_json(&WorkflowInstanceStatus::Invalid {
        errors: vec!["no entry agent".into()],
    });
    for trigger in [
        WorkflowInstanceTrigger::WorkflowUpdated,
        WorkflowInstanceTrigger::ToolUpdated {
            tool: Uuid::new_v4(),
            tool_instance: Uuid::new_v4(),
        },
        WorkflowInstanceTrigger::AgentUpdated {
            agent: Uuid::new_v4(),
        },
        WorkflowInstanceTrigger::ToolSetUpdated {
            tool_set: Uuid::new_v4(),
            tool_set_instance: Uuid::new_v4(),
        },
        WorkflowInstanceTrigger::Manual,
    ] {
        round_trip_json(&trigger);
    }
    // the requests and the update variant
    let req = WorkflowCatalogEntityRequest::default()
        .agent(Uuid::new_v4())
        .entry_agent(Uuid::new_v4())
        .auto_instance(false)
        .limits(WorkflowLimits {
            max_alive_sub_agents: Some(2),
        })
        .default_model(ModelMatcher {
            model_id: "claude-opus".into(),
            ..ModelMatcher::default()
        })
        .model_auth(ModelAuthSource::default());
    round_trip_json(&req);
    round_trip_json(&WorkflowInstanceEntityRequest::new(&instance).unwrap());
    let update = EntityMetadataUpdate::WorkflowCatalog {
        entry_agent: Some(Uuid::new_v4()),
        clear_entry_agent: None,
        auto_instance: Some(true),
        limits: Some(WorkflowLimits::default()),
        default_models: Some(vec![ModelMatcher {
            model_id: "gemini".into(),
            ..ModelMatcher::default()
        }]),
        model_auth: Some(ModelAuthSource::default()),
        add_agents: vec![Uuid::new_v4()],
        remove_agents: vec![Uuid::new_v4()],
        extend_target_groups: true,
    };
    round_trip_json(&update);
    // the harness toolset read
    let toolset = InstancedAgentToolset {
        agent: instance.agents[0].clone(),
        tools: vec![InstancedToolSpec {
            tool: instance.tools[0].clone(),
            spec: ToolSpec::default(),
        }],
    };
    round_trip_json(&toolset);
}

/// A catalog without `auto_instance`, `default_models`, or `model_auth` reads them as their
/// defaults, and an instance without `triggered_by` or `tool_sets` reads them empty
#[test]
fn missing_fields_default() {
    // a catalog with only its required fields
    let catalog: WorkflowCatalogEntity = serde_json::from_value(json!({
        "entry_agent": null,
        "modified": "2026-10-16T00:00:00Z",
    }))
    .unwrap();
    assert!(catalog.auto_instance);
    assert_eq!(catalog.default_models.len(), 0);
    assert_eq!(catalog.model_auth, ModelAuthSource::default());
    assert_eq!(catalog.limits, WorkflowLimits::default());
    assert_eq!(catalog.latest_instance, None);
    assert_eq!(catalog.newest_instance, None);
    assert_eq!(catalog.highest_revision, 0);
    // an instance without triggered_by or an agent's tool_sets
    let mut value = serde_json::to_value(instance()).unwrap();
    value.as_object_mut().unwrap().remove("triggered_by");
    value["agents"][0]
        .as_object_mut()
        .unwrap()
        .remove("tool_sets");
    let read: WorkflowInstanceEntity = serde_json::from_value(value).unwrap();
    assert_eq!(read.triggered_by, String::new());
    assert_eq!(read.agents[0].tool_sets.len(), 0);
    // a catalog never serializes a latest or newest instance it doesn't have
    let value = serde_json::to_value(WorkflowCatalogEntity::default()).unwrap();
    assert!(value.get("latest_instance").is_none(), "{value}");
    assert!(value.get("newest_instance").is_none(), "{value}");
}

/// The status and each trigger serialize in their externally tagged form
#[test]
fn enum_forms() {
    // a unit variant is its name and a data variant wraps its fields
    assert_eq!(
        serde_json::to_value(WorkflowInstanceStatus::Ready).unwrap(),
        json!("Ready")
    );
    assert_eq!(
        serde_json::to_value(WorkflowInstanceStatus::Invalid {
            errors: vec!["no entry agent".into()],
        })
        .unwrap(),
        json!({"Invalid": {"errors": ["no entry agent"]}})
    );
    let (tool, tool_instance, agent) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let cases = [
        (
            WorkflowInstanceTrigger::WorkflowUpdated,
            json!("WorkflowUpdated"),
        ),
        (
            WorkflowInstanceTrigger::ToolUpdated {
                tool,
                tool_instance,
            },
            json!({"ToolUpdated": {"tool": tool, "tool_instance": tool_instance}}),
        ),
        (
            WorkflowInstanceTrigger::AgentUpdated { agent },
            json!({"AgentUpdated": {"agent": agent}}),
        ),
        (
            WorkflowInstanceTrigger::ToolSetUpdated {
                tool_set: tool,
                tool_set_instance: tool_instance,
            },
            json!({"ToolSetUpdated": {"tool_set": tool, "tool_set_instance": tool_instance}}),
        ),
        (WorkflowInstanceTrigger::Manual, json!("Manual")),
    ];
    for (trigger, expected) in cases {
        assert_eq!(serde_json::to_value(&trigger).unwrap(), expected);
    }
    // the status names its intrinsic tag value
    assert_eq!(WorkflowInstanceStatus::Ready.as_str(), "Ready");
    assert_eq!(
        WorkflowInstanceStatus::Invalid { errors: Vec::new() }.as_str(),
        "Invalid"
    );
}

/// The run-wide limits and default models are part of the digest, and a list set to its
/// default hashes like one that was never set
#[test]
fn limits_and_default_models_in_digest() {
    // the same instance with different limits hashes differently
    let base = instance();
    let digest = base.compute_digest(&BTreeMap::new()).unwrap();
    let mut limited = base.clone();
    limited.limits.max_alive_sub_agents = Some(5);
    assert_ne!(limited.compute_digest(&BTreeMap::new()).unwrap(), digest);
    // different default models hash differently
    let mut models = base.clone();
    models.default_models.push(ModelMatcher {
        model_id: "claude-haiku".into(),
        ..ModelMatcher::default()
    });
    assert_ne!(models.compute_digest(&BTreeMap::new()).unwrap(), digest);
    // reordering the default models changes the digest, since they are in priority order
    let mut two = base.clone();
    two.default_models = vec![
        ModelMatcher {
            model_id: "a".into(),
            ..ModelMatcher::default()
        },
        ModelMatcher {
            model_id: "b".into(),
            ..ModelMatcher::default()
        },
    ];
    let mut swapped = two.clone();
    swapped.default_models.reverse();
    assert_ne!(
        two.compute_digest(&BTreeMap::new()).unwrap(),
        swapped.compute_digest(&BTreeMap::new()).unwrap()
    );
    // cleared default models and unset limits hash like they were never set
    let mut cleared = base.clone();
    cleared.default_models = Vec::new();
    cleared.limits = WorkflowLimits::default();
    let mut value = serde_json::to_value(&cleared).unwrap();
    value.as_object_mut().unwrap().remove("default_models");
    value.as_object_mut().unwrap().remove("limits");
    let never: WorkflowInstanceEntity = serde_json::from_value(value).unwrap();
    assert_eq!(
        cleared.compute_digest(&BTreeMap::new()).unwrap(),
        never.compute_digest(&BTreeMap::new()).unwrap()
    );
    // an instance never records the catalog's model auth
    let value = serde_json::to_value(&base).unwrap();
    assert!(value.get("model_auth").is_none(), "{value}");
}

/// Names, ids, revisions, triggers, groups, warnings, status, and the user who caused an
/// instance never change its digest
#[test]
fn digest_ignores_identity() {
    // change everything the digest leaves out
    let base = instance();
    let digest = base.compute_digest(&BTreeMap::new()).unwrap();
    let mut renamed = base.clone();
    renamed.triggered_by = "callisto".into();
    renamed.revision = 9;
    renamed.previous = None;
    renamed.trigger = WorkflowInstanceTrigger::WorkflowUpdated;
    renamed.workflow_groups = vec!["vesta".into()];
    renamed.warnings.clear();
    renamed.status = WorkflowInstanceStatus::Invalid {
        errors: vec!["broken".into()],
    };
    for agent in &mut renamed.agents {
        agent.name = format!("{}-renamed", agent.name);
        agent.groups = vec!["vesta".into()];
    }
    for tool in &mut renamed.tools {
        tool.name = format!("{}-renamed", tool.name);
        tool.groups = vec!["vesta".into()];
    }
    assert_eq!(renamed.compute_digest(&BTreeMap::new()).unwrap(), digest);
    // changing a tool's spec hash, which the agent's tools name, changes it
    let mut changed = base.clone();
    changed.tools[0].spec_hash = "v1:cc".into();
    assert_ne!(changed.compute_digest(&BTreeMap::new()).unwrap(), digest);
    // a toolset instance's digest is part of its agent's node
    let mut with_set = base.clone();
    let set = Uuid::new_v4();
    with_set.agents[0].tool_sets.push(set);
    let digests = BTreeMap::from([(set, "v1:dd".to_owned())]);
    let first = with_set.compute_digest(&digests).unwrap();
    let digests = BTreeMap::from([(set, "v1:ee".to_owned())]);
    assert_ne!(with_set.compute_digest(&digests).unwrap(), first);
    // an agent's toolset digest covers only its tools' spec hashes, in any order
    let (left, right) = (tool("left", "v1:01"), tool("right", "v1:02"));
    let forward = agent("forward", &[&left, &right]);
    let backward = agent("backward", &[&right, &left]);
    assert_eq!(forward.toolset_digest, backward.toolset_digest);
    assert!(forward.toolset_digest.starts_with("v1:"));
}

/// Two identical agents hash differently from one, since agents are hashed as a multiset
#[test]
fn digest_counts_identical_agents() {
    // one agent and the same agent twice under different ids
    let base = instance();
    let mut doubled = base.clone();
    let mut copy = doubled.agents[1].clone();
    copy.agent = Uuid::new_v4();
    doubled.agents.push(copy);
    assert_ne!(
        doubled.compute_digest(&BTreeMap::new()).unwrap(),
        base.compute_digest(&BTreeMap::new()).unwrap()
    );
}

/// Sorting puts agents, tools, and each agent's lists in id order
#[test]
fn sort_lists_orders_by_id() {
    // an instance whose lists are out of order
    let mut sorted = instance();
    sorted.agents.reverse();
    sorted.tools.reverse();
    let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    sorted.agents[0].sub_agents = vec![ids[2], ids[0], ids[1]];
    sorted.sort_lists();
    // every list is in id order
    assert!(sorted.agents.windows(2).all(|w| w[0].agent <= w[1].agent));
    assert!(
        sorted
            .tools
            .windows(2)
            .all(|w| (w[0].tool, w[0].tool_instance) <= (w[1].tool, w[1].tool_instance))
    );
    for agent in &sorted.agents {
        assert!(agent.sub_agents.windows(2).all(|w| w[0] <= w[1]));
        assert!(agent.tools.windows(2).all(|w| w[0] <= w[1]));
    }
    // the name is the workflow's name and revision
    assert_eq!(WorkflowInstanceEntity::name_for("Triage", 4), "Triage#r4");
    assert!(sorted.is_ready());
}

/// The requests hash, so entity requests holding them can be hashed
#[test]
fn requests_hash() {
    // equal requests hash equally
    let req = WorkflowCatalogEntityRequest::default().agent(Uuid::nil());
    assert_eq!(std_hash(&req), std_hash(&req.clone()));
    let instance = WorkflowInstanceEntityRequest::new(&instance()).unwrap();
    assert_eq!(std_hash(&instance), std_hash(&instance.clone()));
    // the entity requests that wrap them hash too
    let wrapped = EntityMetadataRequest::WorkflowCatalog(req);
    assert_eq!(std_hash(&wrapped), std_hash(&wrapped.clone()));
}

/// Requests and updates send their fields under the documented form names
#[test]
fn form_fields() {
    // a catalog request sends its agents, entry agent, settings, and model fields
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let req = WorkflowCatalogEntityRequest::default()
        .agent(first)
        .agent(second)
        .entry_agent(first)
        .auto_instance(false)
        .limits(WorkflowLimits {
            max_alive_sub_agents: Some(2),
        })
        .default_model(ModelMatcher {
            model_id: "claude".into(),
            ..ModelMatcher::default()
        })
        .model_auth(ModelAuthSource {
            levels: [ModelAuthLevel::Group].into(),
        })
        .extend_target_groups();
    let fields = req.form_fields().unwrap();
    let get = |name: &str| {
        fields
            .iter()
            .filter(|(field, _)| *field == name)
            .map(|(_, value)| value.clone())
            .collect::<Vec<String>>()
    };
    assert_eq!(get("kind"), vec!["WorkflowCatalog".to_owned()]);
    assert_eq!(
        get("metadata[workflow_agents][]"),
        vec![first.to_string(), second.to_string()]
    );
    assert_eq!(
        get("metadata[workflow_entry_agent]"),
        vec![first.to_string()]
    );
    assert_eq!(get("metadata[workflow_auto_instance]"), vec!["false"]);
    assert_eq!(
        get("metadata[workflow_limits]"),
        vec![r#"{"max_alive_sub_agents":2}"#.to_owned()]
    );
    let models: Vec<ModelMatcher> =
        serde_json::from_str(&get("metadata[workflow_default_models]")[0]).unwrap();
    assert_eq!(models[0].model_id, "claude");
    let auth: ModelAuthSource =
        serde_json::from_str(&get("metadata[workflow_model_auth]")[0]).unwrap();
    assert!(auth.levels.contains(&ModelAuthLevel::Group));
    assert_eq!(get("metadata[extend_target_groups]"), vec!["true"]);
    // an instance request sends the whole instance as one field
    let instance = WorkflowInstanceEntityRequest::new(&instance()).unwrap();
    let fields = instance.form_fields();
    assert_eq!(fields[0], ("kind", "WorkflowInstance".to_owned()));
    assert_eq!(fields[1].0, "metadata[workflow_instance]");
    // an update sends only the fields that are set, and an empty model list clears them
    let update = WorkflowCatalogUpdateFields {
        entry_agent: None,
        clear_entry_agent: Some(&true),
        auto_instance: None,
        limits: None,
        default_models: Some(&[]),
        model_auth: None,
        add_agents: &[second],
        remove_agents: &[first],
        extend_target_groups: false,
    };
    let fields = update.form_fields().unwrap();
    assert_eq!(
        fields,
        vec![
            ("metadata[clear_workflow_entry_agent]", "true".to_owned()),
            ("metadata[workflow_default_models]", "[]".to_owned()),
            ("metadata[add_workflow_agents][]", second.to_string()),
            ("metadata[remove_workflow_agents][]", first.to_string()),
        ]
    );
}

/// No workflow type carries run settings, which are planned but not part of this model
#[test]
fn run_settings_are_planned_only() {
    // the model's source declares no run settings field or type
    let source = include_str!("../workflows.rs");
    assert!(!source.contains("run_settings"));
    assert!(!source.contains("WorkflowRunSettings"));
    // no serialized workflow value has a run settings key
    let catalog = serde_json::to_value(WorkflowCatalogEntity::default()).unwrap();
    let req = serde_json::to_value(WorkflowCatalogEntityRequest::default()).unwrap();
    for value in [catalog, req] {
        assert!(value.get("run_settings").is_none(), "{value}");
    }
}

/// Hashing an instance at the largest size a submission may have stays fast, since every
/// lookup is by id rather than a scan
#[test]
fn digest_at_size_limits_is_fast() {
    // the most tools an instance may list, and the most agents, each with the most tools
    let limits = super::CrawlLimits::default();
    let tools = (0..limits.max_instance_tools())
        .map(|index| tool(&format!("tool-{index}"), &format!("v1:{index:064x}")))
        .collect::<Vec<InstancedTool>>();
    let agents = (0..limits.max_agents_per_workflow)
        .map(|index| {
            let start = (index * 7) % (tools.len() - limits.max_tools_per_agent);
            let given = tools[start..start + limits.max_tools_per_agent]
                .iter()
                .collect::<Vec<&InstancedTool>>();
            let mut built = agent(&format!("agent-{index}"), &[]);
            built.tools = given.iter().map(|tool| tool.tool_instance).collect();
            built
        })
        .collect::<Vec<InstancedAgent>>();
    let mut instance = instance();
    instance.entry_agent = Some(agents[0].agent);
    instance.agents = agents;
    instance.tools = tools;
    // every agent's toolset digest and the instance digest are computed quickly
    let start = std::time::Instant::now();
    for agent in &instance.agents {
        agent.compute_toolset_digest(&instance.tools).unwrap();
    }
    instance.compute_digest(&BTreeMap::new()).unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(3), "{elapsed:?}");
}

/// A result key holds the instance's name and its full id, and splits back into both
#[test]
fn result_key_round_trips() {
    // build the key of an instance and split it again
    let id = Uuid::new_v4();
    let key = WorkflowResultKey::build("malware-triage#r3", id);
    assert_eq!(key, format!("malware-triage#r3~{id}"));
    let parsed = WorkflowResultKey::parse(&key).unwrap();
    assert_eq!(parsed.name, "malware-triage#r3");
    assert_eq!(parsed.instance, id);
    // the display name hides the id
    assert_eq!(WorkflowResultKey::display_name(&key), "malware-triage#r3");
}

/// Two instances with the same name and revision never share a key
#[test]
fn result_keys_differ_for_same_named_instances() {
    // two workflows named alike at the same revision have the same instance name
    let name = WorkflowInstanceEntity::name_for("Triage", 1);
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    assert_ne!(
        WorkflowResultKey::build(&name, first),
        WorkflowResultKey::build(&name, second)
    );
    // each key still names its own instance
    let key = WorkflowResultKey::build(&name, second);
    assert_eq!(WorkflowResultKey::parse(&key).unwrap().instance, second);
}

/// The delimiter is never the slash that splits repo result paths
#[test]
fn result_key_delimiter_is_not_a_path_separator() {
    // the delimiter can't be a path separator, and the key adds no other
    assert_ne!(RESULT_KEY_DELIMITER, '/');
    let key = WorkflowResultKey::build("Triage#r1", Uuid::new_v4());
    assert!(!key.contains('/'), "{key}");
}

/// Names holding the delimiter or an id still split on the real id at the end
#[test]
fn result_key_parses_odd_names() {
    // a name that already looks like a key keeps all of itself
    let (inner, outer) = (Uuid::new_v4(), Uuid::new_v4());
    let name = format!("odd~{inner}");
    let key = WorkflowResultKey::build(&name, outer);
    let parsed = WorkflowResultKey::parse(&key).unwrap();
    assert_eq!((parsed.name, parsed.instance), (name.as_str(), outer));
    // a name with multi-byte characters splits at a character boundary
    let key = WorkflowResultKey::build("tría\u{1f9ea}ge", outer);
    assert_eq!(
        WorkflowResultKey::parse(&key).unwrap().name,
        "tría\u{1f9ea}ge"
    );
}

/// Keys that don't end with the delimiter and an id aren't a workflow run's key
#[test]
fn result_key_parse_refuses_other_keys() {
    // tool names, empty names, short keys, and a missing delimiter all fail to parse
    let id = Uuid::new_v4();
    for key in [
        "sundial".to_owned(),
        "Triage#r1".to_owned(),
        String::new(),
        format!("~{id}"),
        format!("Triage#r1-{id}"),
        format!("Triage#r1~{}", &id.to_string()[1..]),
        format!("Triage#r1~{}", id.to_string().replace('-', "x")),
    ] {
        assert!(WorkflowResultKey::parse(&key).is_none(), "{key}");
        assert_eq!(WorkflowResultKey::display_name(&key), key);
    }
}
