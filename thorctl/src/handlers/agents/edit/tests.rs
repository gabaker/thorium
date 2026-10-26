//! Tests for the AI agent edit view and the update it turns into

use std::collections::BTreeMap;
use thorium::models::{
    AgentConfig, Entity, EntityMetadataUpdate, EntityUpdate, ModelAuthLevel, ModelAuthSource,
};
use uuid::Uuid;

use super::{AgentEditable, AgentView, Resolved, build_update, strict_config, wanted};
use crate::args::agents::ModelAuthArg;
use crate::handlers::agents::fixtures::{agent, tool, tool_instance, toolset};
use crate::handlers::agents::refs::{ToolRef, ToolSetRef};
use crate::handlers::edit::Editable;
use crate::utils::patch::{merge_patch, parse_set, parse_unset};

/// Build the view of an agent entity
///
/// # Arguments
///
/// * `entity` - The agent
fn view_of(entity: &Entity) -> AgentView {
    // the view users edit
    AgentEditable::to_view(entity)
}

/// The view's document, which `--set`, `--unset` and `--patch` edit
///
/// # Arguments
///
/// * `view` - The view to convert
fn document(view: &AgentView) -> serde_json::Value {
    // the same conversion the edit flow uses
    serde_json::to_value(view).unwrap()
}

/// Read a view back from an edited document
///
/// # Arguments
///
/// * `document` - The edited document
fn view_from(document: serde_json::Value) -> Result<AgentView, serde_json::Error> {
    // the same conversion the edit flow uses
    serde_json::from_value(document)
}

/// Apply `--set` and `--unset` arguments to a view
///
/// # Arguments
///
/// * `view` - The view to edit
/// * `sets` - The `PATH=VALUE` arguments
/// * `unsets` - The `PATH` arguments
fn edited(view: &AgentView, sets: &[&str], unsets: &[&str]) -> AgentView {
    // apply each edit to the document
    let mut doc = document(view);
    for set in sets {
        parse_set(set).unwrap().apply(&mut doc).unwrap();
    }
    for unset in unsets {
        parse_unset(unset).unwrap().apply(&mut doc).unwrap();
    }
    view_from(doc).unwrap()
}

/// An agent with every kind of edge
fn lead() -> (Entity, BTreeMap<&'static str, Entity>) {
    // two tools (one followed, one pinned), a toolset, a sub-agent and a pinned harness
    let followed = tool("yara");
    let pinned_tool = tool("capa");
    let pinned = tool_instance(&pinned_tool, "1.0.0");
    let harness_tool = tool("pal");
    let harness = tool_instance(&harness_tool, "2.0.0");
    let kit = toolset("ghidra");
    let scout = agent("scout", |_| {});
    let lead = agent("lead", |meta| {
        meta.config.max_turns = Some(8);
        meta.config.function_name = Some("lead_fn".to_owned());
        meta.latest_tools.push(followed.clone());
        meta.pinned_tools.push(pinned.clone());
        meta.tool_sets.push(kit.clone());
        meta.sub_agents.push(scout.clone());
        meta.harness = Some(Box::new(harness.clone()));
    });
    let mut edges = BTreeMap::new();
    edges.insert("followed", followed);
    edges.insert("pinned", pinned);
    edges.insert("kit", kit);
    edges.insert("scout", scout);
    edges.insert("harness", harness);
    (lead, edges)
}

/// Build the update a view change makes, with every new reference resolved to a fresh id
///
/// # Arguments
///
/// * `entity` - The agent as fetched
/// * `after` - The edited view
fn update_for(entity: &Entity, after: &AgentView) -> Option<EntityUpdate> {
    // resolve each new reference to an id the way the command does
    let before = view_of(entity);
    let wants = wanted(&before, after);
    let mut resolved = Resolved::default();
    for tool in &wants.tools {
        resolved.tools.insert(tool.clone(), Uuid::new_v4());
    }
    for toolset in &wants.toolsets {
        resolved.toolsets.insert(toolset.clone(), Uuid::new_v4());
    }
    for sub_agent in &wants.sub_agents {
        resolved
            .sub_agents
            .insert(sub_agent.clone(), Uuid::new_v4());
    }
    if wants.harness.is_some() {
        resolved.harness = Some(Uuid::new_v4());
    }
    build_update(entity, &before, after, &resolved).unwrap()
}

/// Unpack the agent metadata update of an update
///
/// # Arguments
///
/// * `update` - The update
fn agent_update(update: &EntityUpdate) -> &EntityMetadataUpdate {
    // an agent edit sends agent metadata
    update.metadata.as_ref().expect("no metadata update")
}

/// The view lists edges by name, hides empty model auth, and sorts every list
#[test]
fn view_names_every_edge() {
    // an agent with every kind of edge
    let (entity, _) = lead();
    let view = view_of(&entity);
    // names, not ids, identify the edges
    assert_eq!(view.name, "lead");
    assert_eq!(view.id.as_deref(), Some(entity.id.to_string().as_str()));
    assert_eq!(view.groups, vec!["lab"]);
    assert_eq!(view.tools.len(), 2);
    assert!(view.tools.contains(&ToolRef::parse("yara")));
    assert!(view.tools.contains(&ToolRef::parse("capa@1.0.0")));
    assert_eq!(view.toolsets, vec![ToolSetRef::parse("ghidra")]);
    assert_eq!(view.sub_agents, vec!["scout"]);
    assert_eq!(view.harness, Some(ToolRef::parse("pal@2.0.0")));
    // every level may supply model auth when none is listed
    assert_eq!(view.model_auth, None);
    // the config is the agent's config
    assert_eq!(view.config["max_turns"], 8);
}

/// Listed model auth levels appear by name in precedence order
#[test]
fn view_names_model_auth_levels() {
    // an agent limited to two levels
    let entity = agent("lead", |meta| {
        meta.model_auth = ModelAuthSource {
            levels: [ModelAuthLevel::User, ModelAuthLevel::Group]
                .into_iter()
                .collect(),
        };
    });
    // the levels are listed by name
    assert_eq!(
        view_of(&entity).model_auth,
        Some(vec![ModelAuthArg::Group, ModelAuthArg::User])
    );
}

/// The view survives a trip through YAML, and an unknown key is refused
#[test]
fn view_round_trips_and_refuses_unknown_keys() {
    // write the view as YAML and read it back
    let (entity, _) = lead();
    let view = view_of(&entity);
    let yaml = crate::utils::curated_yaml(&view, AgentEditable::FIELD_ORDER).unwrap();
    assert!(yaml.starts_with("'*id*'") || yaml.starts_with("\"*id*\"") || yaml.starts_with("*id*"));
    let again: AgentView = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(again, view);
    // a key the view doesn't have is refused
    let mut doc = document(&view);
    doc["colour"] = "red".into();
    assert!(view_from(doc).is_err());
    // a list that is missing is refused instead of read as empty
    let mut doc = document(&view);
    doc.as_object_mut().unwrap().remove("tools");
    assert!(view_from(doc).is_err());
}

/// The checks that need no server refuse a bad view
#[test]
fn validation_refuses_bad_views() {
    // start from a good view
    let (entity, _) = lead();
    let good = view_of(&entity);
    assert!(AgentEditable::validate(&good, &entity).is_ok());
    // a blank name
    let err = AgentEditable::validate(&edited(&good, &["name=\"  \""], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");
    // no groups
    let err = AgentEditable::validate(&edited(&good, &["groups=[]"], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("group"), "{err}");
    // a repeated group
    let err =
        AgentEditable::validate(&edited(&good, &["groups=[lab, lab]"], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("lab"), "{err}");
    // a repeated tool
    let err = AgentEditable::validate(
        &edited(&good, &["tools=[{tool: yara}, {tool: yara}]"], &[]),
        &entity,
    )
    .unwrap_err();
    assert!(err.to_string().contains("yara"), "{err}");
    // an agent that delegates to itself
    let err =
        AgentEditable::validate(&edited(&good, &["sub_agents=[lead]"], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("itself"), "{err}");
    // a config the API would refuse
    let err = AgentEditable::validate(
        &edited(&good, &["config.function_name=\"bad name\""], &[]),
        &entity,
    )
    .unwrap_err();
    assert!(err.to_string().contains("function_name"), "{err}");
    // a config key that doesn't exist
    let err =
        AgentEditable::validate(&edited(&good, &["config.max_turn=5"], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("max_turn"), "{err}");
}

/// A config document is read strictly
#[test]
fn config_is_read_strictly() {
    // a good config reads back as typed
    let config = strict_config(&serde_json::json!({"max_turns": 3})).unwrap();
    assert_eq!(config.max_turns, Some(3));
    // an unknown key, a wrong type and a non-object are refused
    assert!(strict_config(&serde_json::json!({"max_turn": 3})).is_err());
    assert!(strict_config(&serde_json::json!({"max_turns": "many"})).is_err());
    assert!(strict_config(&serde_json::json!([1])).is_err());
}

/// `--set`, `--unset` and `--patch` change the view as described
#[test]
fn set_unset_and_patch_change_the_view() {
    let (entity, _) = lead();
    let view = view_of(&entity);
    // a nested config value
    let changed = edited(&view, &["config.max_turns=5"], &[]);
    assert_eq!(changed.config["max_turns"], 5);
    // a list value in flow syntax
    let changed = edited(&view, &["groups=[lab, mars]"], &[]);
    assert_eq!(changed.groups, vec!["lab", "mars"]);
    // an index into a list
    let changed = edited(&view, &["sub_agents[1]=probe"], &[]);
    assert_eq!(changed.sub_agents, vec!["scout", "probe"]);
    // removing an optional field
    let described = edited(&view, &["description=checks files"], &[]);
    assert_eq!(described.description.as_deref(), Some("checks files"));
    assert_eq!(edited(&described, &[], &["description"]).description, None);
    // a merge patch nulls a field and sets another
    let mut doc = document(&described);
    merge_patch(
        &mut doc,
        &serde_json::json!({"description": null, "config": {"max_tools": 4}}),
    );
    let patched = view_from(doc).unwrap();
    assert_eq!(patched.description, None);
    assert_eq!(patched.config["max_tools"], 4);
    assert_eq!(patched.config["max_turns"], 8);
}

/// A view with no changes builds no update
#[test]
fn no_change_builds_no_update() {
    // the unchanged view
    let (entity, _) = lead();
    assert!(update_for(&entity, &view_of(&entity)).is_none());
    // a change to a read-only field alone changes nothing either
    let mut view = view_of(&entity);
    view.id = Some("someone else".to_owned());
    assert!(update_for(&entity, &view).is_none());
    // an explicit empty model auth is the same as none
    let mut view = view_of(&entity);
    view.model_auth = Some(Vec::new());
    assert!(update_for(&entity, &view).is_none());
}

/// Name, description and group changes go in the entity update
#[test]
fn entity_fields_change() {
    let (entity, _) = lead();
    let view = view_of(&entity);
    // a rename
    let update = update_for(&entity, &edited(&view, &["name=captain"], &[])).unwrap();
    assert_eq!(update.name.as_deref(), Some("captain"));
    assert!(update.metadata.is_none() && update.description.is_none());
    // a description is set and then cleared
    let described = edited(&view, &["description=hello"], &[]);
    let update = update_for(&entity, &described).unwrap();
    assert_eq!(update.description.as_deref(), Some("hello"));
    assert!(!update.clear_description);
    let with_description = agent("lead", |_| {});
    let mut start = view_of(&with_description);
    start.description = Some("old".to_owned());
    let mut with_description = with_description;
    with_description.description = Some("old".to_owned());
    let update = update_for(&with_description, &edited(&start, &[], &["description"])).unwrap();
    assert!(update.clear_description);
    assert!(update.description.is_none());
    // groups are added and removed
    let update = update_for(&entity, &edited(&view, &["groups=[mars, venus]"], &[])).unwrap();
    assert_eq!(update.add_groups, vec!["mars", "venus"]);
    assert_eq!(update.remove_groups, vec!["lab"]);
}

/// A config change replaces the config, and a model auth change sets the levels
#[test]
fn config_and_model_auth_change() {
    let (entity, _) = lead();
    let view = view_of(&entity);
    // a config change sends the whole typed config
    let update = update_for(&entity, &edited(&view, &["config.max_turns=5"], &[])).unwrap();
    let EntityMetadataUpdate::Agent { config, .. } = agent_update(&update) else {
        panic!("expected an agent update");
    };
    let config: &AgentConfig = config.as_ref().expect("no config");
    assert_eq!(config.max_turns, Some(5));
    assert_eq!(config.function_name.as_deref(), Some("lead_fn"));
    // limiting model auth sets the levels
    let update = update_for(
        &entity,
        &edited(&view, &["model_auth=[user, workflow]"], &[]),
    )
    .unwrap();
    let EntityMetadataUpdate::Agent {
        model_auth, config, ..
    } = agent_update(&update)
    else {
        panic!("expected an agent update");
    };
    assert!(config.is_none());
    let levels = &model_auth.as_ref().expect("no model auth").levels;
    assert!(levels.contains(&ModelAuthLevel::User) && levels.contains(&ModelAuthLevel::Workflow));
    assert_eq!(levels.len(), 2);
    // clearing it sends an empty set, which allows every level
    let limited = agent("lead", |meta| {
        meta.model_auth = ModelAuthSource {
            levels: [ModelAuthLevel::Group].into_iter().collect(),
        };
    });
    let start = view_of(&limited);
    let update = update_for(&limited, &edited(&start, &["model_auth=null"], &[])).unwrap();
    let EntityMetadataUpdate::Agent { model_auth, .. } = agent_update(&update) else {
        panic!("expected an agent update");
    };
    assert_eq!(model_auth.as_ref().map(|auth| auth.levels.len()), Some(0));
}

/// Adding and removing tools, toolsets and sub-agents are diffed into the edge lists
#[test]
fn edges_are_diffed() {
    let (entity, edges) = lead();
    let view = view_of(&entity);
    // drop the pinned tool and the toolset, add a tool and a pinned tool
    let changed = edited(
        &view,
        &[
            "tools=[{tool: yara}, {tool: nmap}, {tool: capa, version: \"2.0.0\"}]",
            "toolsets=[]",
        ],
        &[],
    );
    let wants = wanted(&view, &changed);
    assert_eq!(
        wants.tools,
        vec![ToolRef::parse("capa@2.0.0"), ToolRef::parse("nmap")]
    );
    assert!(wants.toolsets.is_empty() && wants.harness.is_none());
    let update = update_for(&entity, &changed).unwrap();
    let EntityMetadataUpdate::Agent {
        add_tools,
        remove_tools,
        add_tool_sets,
        remove_tool_sets,
        add_sub_agents,
        remove_sub_agents,
        harness,
        clear_harness,
        ..
    } = agent_update(&update)
    else {
        panic!("expected an agent update");
    };
    // the removed edges use the ids the agent already has
    assert_eq!(add_tools.len(), 2);
    assert_eq!(remove_tools, &vec![edges["pinned"].id]);
    assert_eq!(add_tool_sets, &Vec::<Uuid>::new());
    assert_eq!(remove_tool_sets, &vec![edges["kit"].id]);
    assert!(add_sub_agents.is_empty() && remove_sub_agents.is_empty());
    assert!(harness.is_none() && clear_harness.is_none());
    // a sub-agent is swapped
    let changed = edited(&view, &["sub_agents=[probe]"], &[]);
    let update = update_for(&entity, &changed).unwrap();
    let EntityMetadataUpdate::Agent {
        add_sub_agents,
        remove_sub_agents,
        ..
    } = agent_update(&update)
    else {
        panic!("expected an agent update");
    };
    assert_eq!(add_sub_agents.len(), 1);
    assert_eq!(remove_sub_agents, &vec![edges["scout"].id]);
}

/// Replacing the harness clears it in the same update, and removing it only clears
#[test]
fn harness_changes() {
    let (entity, _) = lead();
    let view = view_of(&entity);
    // a different harness is cleared and set at once
    let update = update_for(&entity, &edited(&view, &["harness.tool=other"], &[])).unwrap();
    let EntityMetadataUpdate::Agent {
        harness,
        clear_harness,
        ..
    } = agent_update(&update)
    else {
        panic!("expected an agent update");
    };
    assert!(harness.is_some());
    assert_eq!(*clear_harness, Some(true));
    // removing it only clears it
    let update = update_for(&entity, &edited(&view, &[], &["harness"])).unwrap();
    let EntityMetadataUpdate::Agent {
        harness,
        clear_harness,
        ..
    } = agent_update(&update)
    else {
        panic!("expected an agent update");
    };
    assert!(harness.is_none());
    assert_eq!(*clear_harness, Some(true));
    // adding one to an agent without a harness only sets it
    let bare = agent("bare", |_| {});
    let start = view_of(&bare);
    let update = update_for(&bare, &edited(&start, &["harness.tool=pal"], &[])).unwrap();
    let EntityMetadataUpdate::Agent {
        harness,
        clear_harness,
        ..
    } = agent_update(&update)
    else {
        panic!("expected an agent update");
    };
    assert!(harness.is_some());
    assert_eq!(*clear_harness, None);
}

/// Parse `agents update` flags
///
/// # Arguments
///
/// * `flags` - The arguments after `thorctl agents update lead`
fn update_cmd(flags: &[&str]) -> crate::args::agents::UpdateAgent {
    use clap::Parser;
    // parse the flags like the binary does
    let mut full = vec!["thorctl", "agents", "update", "lead"];
    full.extend_from_slice(flags);
    let crate::args::SubCommands::Agents(crate::args::agents::Agents::Update(cmd)) =
        crate::args::Args::try_parse_from(full).unwrap().cmd
    else {
        panic!("expected agents update");
    };
    cmd
}

/// Apply the edits the typed update flags make to a view
///
/// # Arguments
///
/// * `view` - The view to edit
/// * `flags` - The `agents update` flags
fn typed(view: &AgentView, flags: &[&str]) -> AgentView {
    // turn the flags into edits and apply them in order
    let ops = super::typed_ops(&update_cmd(flags), view).unwrap();
    let mut doc = document(view);
    for op in ops {
        op.apply(&mut doc).unwrap();
    }
    view_from(doc).unwrap()
}

/// The typed flags of `agents update` edit the view like the equivalent `--set` would
#[test]
fn typed_flags_edit_the_view() {
    let (entity, _) = lead();
    let view = view_of(&entity);
    // rename and describe
    let changed = typed(&view, &["--rename", "captain", "-d", "leads"]);
    assert_eq!(changed.name, "captain");
    assert_eq!(changed.description.as_deref(), Some("leads"));
    // clear the description again
    assert_eq!(typed(&changed, &["--clear-description"]).description, None);
    // groups are added and removed relative to the current groups
    let changed = typed(&view, &["--add-group", "mars", "--add-group", "lab"]);
    assert_eq!(changed.groups, vec!["lab", "mars"]);
    let changed = typed(&changed, &["--remove-group", "lab"]);
    assert_eq!(changed.groups, vec!["mars"]);
    // model auth is listed once, in precedence order, and cleared
    let changed = typed(
        &view,
        &[
            "--model-auth",
            "user",
            "--model-auth",
            "group",
            "--model-auth",
            "user",
        ],
    );
    assert_eq!(
        changed.model_auth,
        Some(vec![ModelAuthArg::Group, ModelAuthArg::User])
    );
    assert_eq!(typed(&changed, &["--clear-model-auth"]).model_auth, None);
}

/// A config file replaces the config, and an unreadable one is refused
#[test]
fn typed_config_file_replaces_the_config() {
    let (entity, _) = lead();
    let view = view_of(&entity);
    // write a config file
    let dir = std::env::temp_dir().join(format!("agent-edit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, "max_turns: 2\nfunction_name: other_fn\n").unwrap();
    let changed = typed(&view, &["--config", path.to_str().unwrap()]);
    assert_eq!(changed.config["max_turns"], 2);
    assert_eq!(changed.config["function_name"], "other_fn");
    // the file's config replaces the old one entirely
    assert_eq!(changed.config["max_tools"], serde_json::Value::Null);
    // a file that is not a config is refused before any request
    std::fs::write(&path, "max_turns: [unclosed").unwrap();
    let cmd = update_cmd(&["--config", path.to_str().unwrap()]);
    assert!(super::typed_ops(&cmd, &view).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
