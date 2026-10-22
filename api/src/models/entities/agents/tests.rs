//! Tests for the agent models

use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hash};
use uuid::Uuid;

use super::{AgentConfig, AgentEntity, AgentEntityRequest, AgentUpdateFields, ToolChoice};
use crate::models::entities::digests::{DigestTag, ThorDigest};
use crate::models::{
    Entity, EntityKinds, EntityMetadata, EntityMetadataUpdate, ModelApi, ModelAuthLevel,
    ModelAuthSource, ModelCapabilities, ModelMatcher, ModelProviderKind, ModelRequirements,
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

/// Build a matcher for an exact model id
///
/// # Arguments
///
/// * `model_id` - The model id to match
fn matcher(model_id: &str) -> ModelMatcher {
    ModelMatcher {
        model_id: model_id.to_owned(),
        ..ModelMatcher::default()
    }
}

/// A config with every field set
fn full_config() -> AgentConfig {
    AgentConfig {
        system_prompt: Some("You triage malware".into()),
        prompt: Some("Triage these samples".into()),
        models: vec![
            ModelMatcher {
                model_id: "claude-opus".into(),
                provider: Some(ModelProviderKind::Anthropic),
                api: Some(ModelApi::AnthropicMessages),
                api_versions: BTreeSet::from(["2023-06-01".to_owned()]),
            },
            matcher("gpt-5"),
        ],
        model_requirements: ModelRequirements {
            min_context_window: Some(100_000),
            min_output_tokens: Some(4_096),
            capabilities: ModelCapabilities {
                tool_calling: true,
                ..ModelCapabilities::default()
            },
        },
        max_tools: Some(8),
        max_turns: Some(40),
        max_swarm_size: Some(3),
        tool_tags: BTreeMap::from([("toolset".to_owned(), BTreeSet::from(["ghidra".to_owned()]))]),
        function_name: Some("triage_lead".into()),
        model_description: Some("Leads a malware triage".into()),
        tool_choice: ToolChoice::Function("yara_scan".into()),
        parallel_tool_calls: Some(false),
    }
}

/// Get the config hash of a config
///
/// # Arguments
///
/// * `config` - The config to hash
fn hash(config: &AgentConfig) -> String {
    config.config_hash().unwrap()
}

/// Build an agent entity around a config
///
/// # Arguments
///
/// * `name` - The agent's name
/// * `config` - The agent's config
/// * `model_auth` - The agent's model auth levels
fn agent_entity(name: &str, config: AgentConfig, model_auth: ModelAuthSource) -> Entity {
    // hash the config the way the API does
    let config_hash = hash(&config);
    Entity {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        kind: EntityKinds::Agent,
        metadata: EntityMetadata::Agent(AgentEntity {
            config,
            config_hash,
            model_auth,
            modified: Utc::now(),
            ..AgentEntity::default()
        }),
        description: None,
        submitter: "io".to_owned(),
        groups: vec!["minerva".to_owned()],
        tags: std::collections::HashMap::default(),
        image: None,
        created: Utc::now(),
    }
}

/// The agent types round trip through serde JSON
#[test]
fn serde_round_trips() {
    // a full config, entity, and request round trip
    let config = full_config();
    let back: AgentConfig = serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
    assert_eq!(back, config);
    round_trip_json(&AgentEntity {
        config: config.clone(),
        config_hash: hash(&config),
        model_auth: ModelAuthSource {
            levels: BTreeSet::from([ModelAuthLevel::User]),
        },
        modified: Utc::now(),
        ..AgentEntity::default()
    });
    round_trip_json(
        &AgentEntityRequest::new(config)
            .tool(Uuid::new_v4())
            .tool_set(Uuid::new_v4())
            .sub_agent(Uuid::new_v4())
            .harness(Uuid::new_v4())
            .model_auth(ModelAuthSource::default())
            .extend_target_groups(),
    );
}

/// Hash a value with the standard hasher
///
/// # Arguments
///
/// * `value` - The value to hash
fn std_hash<T: Hash>(value: &T) -> u64 {
    // a fixed hasher so equal values hash the same
    std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
        .hash_one(value)
}

/// Agent configs and requests implement `Hash`, and equal configs hash the same
#[test]
fn configs_hash_equal() {
    // two equal configs hash the same
    assert_eq!(std_hash(&full_config()), std_hash(&full_config()));
    // requests hash too
    let req = AgentEntityRequest::new(full_config());
    assert_eq!(std_hash(&req), std_hash(&req.clone()));
}

/// `config_hash` follows every config field, including model order, and never `model_auth`
#[test]
fn config_hash_fields() {
    // the hash of the full config
    let base = full_config();
    let base_hash = hash(&base);
    // the hash is a v1 digest
    assert!(
        base_hash.starts_with("v1:") && base_hash.len() == 67,
        "{base_hash}"
    );
    // every change of a config field changes the hash
    let mut changes: Vec<(&str, AgentConfig)> = Vec::new();
    let mut change = |name, edit: &dyn Fn(&mut AgentConfig)| {
        let mut config = base.clone();
        edit(&mut config);
        changes.push((name, config));
    };
    change("system_prompt", &|c| c.system_prompt = Some("other".into()));
    change("prompt", &|c| c.prompt = None);
    change("function_name", &|c| c.function_name = Some("other".into()));
    change("model_description", &|c| c.model_description = None);
    change("models order", &|c| c.models.reverse());
    change("models", &|c| c.models.push(matcher("gemini")));
    change("model_requirements", &|c| {
        c.model_requirements.min_context_window = Some(1);
    });
    change("capabilities", &|c| {
        c.model_requirements.capabilities.vision = true;
    });
    change("max_tools", &|c| c.max_tools = Some(9));
    change("max_turns", &|c| c.max_turns = None);
    change("max_swarm_size", &|c| c.max_swarm_size = Some(0));
    change("tool_tags", &|c| c.tool_tags.clear());
    change("tool_choice", &|c| c.tool_choice = ToolChoice::Auto);
    change("parallel_tool_calls", &|c| c.parallel_tool_calls = None);
    for (name, config) in changes {
        assert_ne!(hash(&config), base_hash, "{name} didn't change the hash");
    }
    // the model auth levels, the name, the id, and the edges never change it
    let first = agent_entity("triage", base.clone(), ModelAuthSource::default());
    let mut second = agent_entity(
        "renamed",
        base,
        ModelAuthSource {
            levels: BTreeSet::from([ModelAuthLevel::Group]),
        },
    );
    if let EntityMetadata::Agent(agent) = &mut second.metadata {
        agent.sub_agents.push(first.clone());
        agent.latest_tools.push(first.clone());
    }
    let hash_of = |entity: &Entity| match &entity.metadata {
        EntityMetadata::Agent(agent) => (agent.config_hash.clone(), hash(&agent.config)),
        _ => unreachable!(),
    };
    assert_eq!(hash_of(&first), hash_of(&second));
}

/// The limits round trip and change the hash, and a swarm size of 0 is accepted
#[test]
fn limits_round_trip_and_hash() {
    // each limit round trips and changes the hash
    let base = AgentConfig::default();
    for (name, config) in [
        (
            "max_turns",
            AgentConfig {
                max_turns: Some(12),
                ..AgentConfig::default()
            },
        ),
        (
            "max_swarm_size",
            AgentConfig {
                max_swarm_size: Some(0),
                ..AgentConfig::default()
            },
        ),
    ] {
        let back: AgentConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(back, config, "{name}");
        assert_ne!(hash(&config), hash(&base), "{name}");
        assert!(config.validate().is_ok(), "{name}");
    }
}

/// Toolsets round trip on entities and requests, and a missing list is empty
#[test]
fn tool_sets_default() {
    // a set toolset list round trips
    let id = Uuid::new_v4();
    let req = AgentEntityRequest::new(AgentConfig::default()).tool_set(id);
    let back: AgentEntityRequest =
        serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
    assert_eq!(back.tool_sets, vec![id]);
    // a request and an entity without toolsets read as empty
    let req: AgentEntityRequest = serde_json::from_value(json!({"config": {}})).unwrap();
    assert_eq!(req.tool_sets, Vec::<Uuid>::new());
    let entity: AgentEntity = serde_json::from_value(json!({
        "config": {},
        "config_hash": "v1:x",
        "modified": "2026-10-16T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(entity.tool_sets.len(), 0);
    assert!(entity.latest_tools.is_empty() && entity.pinned_tools.is_empty());
}

/// A config without its model fields reads as empty, and an entity without `model_auth`
/// allows every level
#[test]
fn missing_model_fields_default() {
    // a config written before the model fields existed
    let config: AgentConfig = serde_json::from_value(json!({"prompt": "go"})).unwrap();
    assert_eq!(config.models.len(), 0);
    assert_eq!(config.model_requirements, ModelRequirements::default());
    // an entity without model auth allows every level
    let entity: AgentEntity = serde_json::from_value(json!({
        "config": {},
        "config_hash": "v1:x",
        "modified": "2026-10-16T00:00:00Z"
    }))
    .unwrap();
    assert!(entity.model_auth.levels.is_empty());
}

/// Tool tags hash the same whatever order they're inserted in, and the limits are `u32`
#[test]
fn tool_tags_order_and_bounds() {
    // insert the same tags in two orders
    let mut first = BTreeMap::new();
    first.insert(
        "b".to_owned(),
        BTreeSet::from(["2".to_owned(), "1".to_owned()]),
    );
    first.insert("a".to_owned(), BTreeSet::from(["x".to_owned()]));
    let mut second = BTreeMap::new();
    second.insert("a".to_owned(), BTreeSet::from(["x".to_owned()]));
    second.insert(
        "b".to_owned(),
        BTreeSet::from(["1".to_owned(), "2".to_owned()]),
    );
    let first = AgentConfig {
        tool_tags: first,
        ..AgentConfig::default()
    };
    let second = AgentConfig {
        tool_tags: second,
        ..AgentConfig::default()
    };
    // they serialize to the same bytes and hash the same
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    assert_eq!(hash(&first), hash(&second));
    // each limit takes the largest u32 and refuses anything outside it
    for field in ["max_tools", "max_turns", "max_swarm_size"] {
        assert!(serde_json::from_value::<AgentConfig>(json!({ field: 4_294_967_295_u64 })).is_ok());
        assert!(
            serde_json::from_value::<AgentConfig>(json!({ field: 4_294_967_296_u64 })).is_err()
        );
        assert!(serde_json::from_value::<AgentConfig>(json!({ field: -1 })).is_err());
    }
}

/// The agent update variant round trips and sends exactly its set fields
#[test]
fn update_round_trip_and_fields() {
    // an update with every field set round trips
    let (tool, other, toolset, sub) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let update = EntityMetadataUpdate::Agent {
        config: Some(full_config()),
        model_auth: Some(ModelAuthSource::default()),
        harness: None,
        clear_harness: Some(true),
        add_tools: vec![tool],
        remove_tools: vec![other],
        add_tool_sets: vec![toolset],
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: vec![sub],
        extend_target_groups: true,
    };
    round_trip_json(&update);
    // its form sends only the fields that are set
    let config = full_config();
    let model_auth = ModelAuthSource::default();
    let fields = AgentUpdateFields {
        config: Some(&config),
        model_auth: Some(&model_auth),
        harness: None,
        clear_harness: Some(&true),
        add_tools: &[tool],
        remove_tools: &[other],
        add_tool_sets: &[toolset],
        remove_tool_sets: &[],
        add_sub_agents: &[],
        remove_sub_agents: &[sub],
        extend_target_groups: true,
    }
    .form_fields()
    .unwrap();
    let names = fields.iter().map(|(name, _)| *name).collect::<Vec<&str>>();
    assert_eq!(
        names,
        vec![
            "metadata[agent_config]",
            "metadata[agent_model_auth]",
            "metadata[clear_agent_harness]",
            "metadata[add_agent_tools][]",
            "metadata[remove_agent_tools][]",
            "metadata[add_agent_tool_sets][]",
            "metadata[remove_agent_sub_agents][]",
            "metadata[extend_target_groups]",
        ]
    );
}

/// An agent request sends its config as one field, and the widen flag only when it's set
#[test]
fn request_form_fields() {
    // a request with one tool and the widen flag
    let tool = Uuid::new_v4();
    let config = full_config();
    let req = AgentEntityRequest::new(config.clone())
        .tool(tool)
        .extend_target_groups();
    let fields = req.form_fields().unwrap();
    assert_eq!(
        fields,
        vec![
            ("kind", "Agent".to_owned()),
            (
                "metadata[agent_config]",
                serde_json::to_string(&config).unwrap()
            ),
            ("metadata[agent_tools][]", tool.to_string()),
            ("metadata[extend_target_groups]", "true".to_owned()),
        ]
    );
    // without the flag nothing about widening is sent
    let fields = AgentEntityRequest::new(config).form_fields().unwrap();
    assert!(
        fields
            .iter()
            .all(|(name, _)| *name != "metadata[extend_target_groups]")
    );
}

/// Function names follow the agentic function name rule
#[test]
fn function_name_rule() {
    // build a config with a delegation function name
    let named = |name: &str| AgentConfig {
        function_name: Some(name.to_owned()),
        ..AgentConfig::default()
    };
    // one character and 64 allowed characters are accepted
    assert!(named("a").validate().is_ok());
    let longest = format!("a{}", "b_-9".repeat(15)) + "xyz";
    assert_eq!(longest.len(), 64);
    assert!(named(&longest).validate().is_ok());
    // empty, too long, spaces, dots, and a leading digit are refused naming the field
    for bad in [
        "",
        &format!("{longest}c"),
        "has space",
        "dot.name",
        "1agent",
    ] {
        let err = named(bad).validate().unwrap_err();
        assert!(err.contains("function_name"), "{bad}: {err}");
    }
}

/// The tool call controls default to `Auto` and unset, keep old hashes, and change the hash
#[test]
fn tool_call_controls() {
    // a config written before the controls existed reads as Auto and unset
    let old: AgentConfig = serde_json::from_value(json!({"prompt": "go"})).unwrap();
    assert_eq!(old.tool_choice, ToolChoice::Auto);
    assert_eq!(old.parallel_tool_calls, None);
    // the defaults are pruned, so the digest value holds neither field
    let tagged = ThorDigest::tagged_value(DigestTag::AgentConfig, &old).unwrap();
    assert_eq!(tagged, json!({"t": "agent_config", "prompt": "go"}));
    // each control round trips and changes the hash
    for (name, config) in [
        (
            "Required",
            AgentConfig {
                tool_choice: ToolChoice::Required,
                ..old.clone()
            },
        ),
        (
            "None",
            AgentConfig {
                tool_choice: ToolChoice::None,
                ..old.clone()
            },
        ),
        (
            "Function",
            AgentConfig {
                tool_choice: ToolChoice::Function("yara_scan".into()),
                ..old.clone()
            },
        ),
        (
            "parallel_tool_calls",
            AgentConfig {
                parallel_tool_calls: Some(false),
                ..old.clone()
            },
        ),
    ] {
        let back: AgentConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(back, config, "{name}");
        assert_ne!(hash(&config), hash(&old), "{name}");
        assert!(config.validate().is_ok(), "{name}");
    }
    // a forced function must be a valid function name
    for bad in ["", "1scan"] {
        let config = AgentConfig {
            tool_choice: ToolChoice::Function(bad.to_owned()),
            ..AgentConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("tool_choice"), "{bad}: {err}");
    }
}

/// Models keep their order, need a model id, and change the hash, while model auth doesn't
#[test]
fn models_order_and_hash() {
    // the models keep their order through serde
    let config = AgentConfig {
        models: vec![matcher("b"), matcher("a"), matcher("c")],
        ..AgentConfig::default()
    };
    let back: AgentConfig = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
    let ids = back
        .models
        .iter()
        .map(|matcher| matcher.model_id.as_str())
        .collect::<Vec<&str>>();
    assert_eq!(ids, vec!["b", "a", "c"]);
    // a matcher without a model id is refused
    assert!(
        serde_json::from_value::<AgentConfig>(json!({"models": [{"provider": "OpenAi"}]})).is_err()
    );
    let blank = AgentConfig {
        models: vec![matcher(" ")],
        ..AgentConfig::default()
    };
    assert!(blank.validate().unwrap_err().contains("models[0]"));
    // models and requirements change the hash
    let requirements = AgentConfig {
        model_requirements: ModelRequirements {
            capabilities: ModelCapabilities {
                forced_tool_choice: true,
                ..ModelCapabilities::default()
            },
            ..ModelRequirements::default()
        },
        ..config.clone()
    };
    assert_ne!(hash(&config), hash(&AgentConfig::default()));
    assert_ne!(hash(&requirements), hash(&config));
}
