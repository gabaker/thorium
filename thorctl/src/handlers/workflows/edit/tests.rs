//! Tests for the workflow edit view and the update it turns into

use clap::Parser;
use std::collections::BTreeMap;
use thorium::models::{
    Entity, EntityKinds, EntityMetadata, EntityMetadataUpdate, EntityUpdate, ModelAuthLevel,
    ModelAuthSource, ModelMatcher, WorkflowCatalogEntity, WorkflowLimits,
};
use uuid::Uuid;

use super::{
    Resolved, WorkflowEditable, WorkflowView, build_update, create_request, create_view, typed_ops,
    wanted,
};
use crate::args::agents::ModelAuthArg;
use crate::args::workflows::{CreateWorkflow, UpdateWorkflow, Workflows};
use crate::args::{Args, SubCommands};
use crate::handlers::agents::fixtures::{agent, entity};
use crate::handlers::edit::Editable;
use crate::utils::patch::{merge_patch, parse_set, parse_unset};

/// Build a workflow in the group `lab` with its agents and settings filled in
///
/// # Arguments
///
/// * `name` - The workflow's name
/// * `fill` - Sets the workflow's agents and settings
fn workflow(name: &str, fill: impl FnOnce(&mut WorkflowCatalogEntity)) -> Entity {
    // start from a workflow that builds instances automatically
    let mut meta = WorkflowCatalogEntity {
        auto_instance: true,
        ..WorkflowCatalogEntity::default()
    };
    fill(&mut meta);
    entity(
        name,
        EntityKinds::WorkflowCatalog,
        EntityMetadata::WorkflowCatalog(meta),
        &["lab"],
    )
}

/// A workflow with two agents, an entry agent, limits, default models and model auth
fn triage() -> (Entity, Entity, Entity) {
    // two agents, the first of which receives the inputs
    let first = agent("triage", |_| {});
    let second = agent("reverser", |_| {});
    let entity = workflow("pipeline", |meta| {
        meta.agents = vec![first.clone(), second.clone()];
        meta.entry_agent = Some(first.id);
        meta.limits = WorkflowLimits {
            max_alive_sub_agents: Some(4),
        };
        meta.default_models = vec![ModelMatcher {
            model_id: "claude-sonnet".to_owned(),
            ..ModelMatcher::default()
        }];
        meta.model_auth = ModelAuthSource {
            levels: [ModelAuthLevel::User, ModelAuthLevel::Group]
                .into_iter()
                .collect(),
        };
    });
    (entity, first, second)
}

/// Build the view of a workflow entity
///
/// # Arguments
///
/// * `entity` - The workflow
fn view_of(entity: &Entity) -> WorkflowView {
    // the view users edit
    WorkflowEditable::to_view(entity)
}

/// The view's document, which `--set`, `--unset` and `--patch` edit
///
/// # Arguments
///
/// * `view` - The view to convert
fn document(view: &WorkflowView) -> serde_json::Value {
    // the same conversion the edit flow uses
    serde_json::to_value(view).unwrap()
}

/// Read a view back from an edited document
///
/// # Arguments
///
/// * `document` - The edited document
fn view_from(document: serde_json::Value) -> Result<WorkflowView, serde_json::Error> {
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
fn edited(view: &WorkflowView, sets: &[&str], unsets: &[&str]) -> WorkflowView {
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

/// Build the update a view change makes, with every new agent resolved to a fresh id
///
/// # Arguments
///
/// * `entity` - The workflow as fetched
/// * `after` - The edited view
fn update_for(entity: &Entity, after: &WorkflowView) -> Option<EntityUpdate> {
    // resolve each new agent to an id the way the command does
    let before = view_of(entity);
    let mut resolved = Resolved::default();
    for name in wanted(&before, after) {
        resolved.agents.insert(name, Uuid::new_v4());
    }
    build_update(entity, &before, after, &resolved).unwrap()
}

/// Unpack the workflow metadata update of an update
///
/// # Arguments
///
/// * `update` - The update
fn workflow_update(update: &EntityUpdate) -> &EntityMetadataUpdate {
    // a workflow edit sends workflow metadata
    update.metadata.as_ref().expect("no metadata update")
}

/// Parse an `update` command line
///
/// # Arguments
///
/// * `flags` - The arguments after `thorctl workflows update pipeline`
fn update_cmd(flags: &[&str]) -> UpdateWorkflow {
    // parse the whole command line
    let mut argv = vec!["thorctl", "workflows", "update", "pipeline"];
    argv.extend_from_slice(flags);
    match Args::try_parse_from(argv).unwrap().cmd {
        SubCommands::Workflows(Workflows::Update(cmd)) => cmd,
        _ => panic!("expected workflows update"),
    }
}

/// The view names agents and the entry agent, hides empty model auth, and sorts its lists
#[test]
fn view_names_agents_and_settings() {
    // a workflow with two agents and settings
    let (entity, _, _) = triage();
    let view = view_of(&entity);
    // names, not ids, identify the agents
    assert_eq!(view.name, "pipeline");
    assert_eq!(view.id.as_deref(), Some(entity.id.to_string().as_str()));
    assert_eq!(view.groups, vec!["lab"]);
    assert_eq!(view.agents, vec!["reverser", "triage"]);
    assert_eq!(view.entry_agent.as_deref(), Some("triage"));
    // the settings read back
    assert_eq!(view.limits.max_alive_sub_agents, Some(4));
    assert_eq!(view.default_models[0].model_id, "claude-sonnet");
    assert!(view.auto_instance);
    assert_eq!(
        view.model_auth,
        Some(vec![ModelAuthArg::Group, ModelAuthArg::User])
    );
    // a workflow with no model auth levels allows every level
    let bare = workflow("bare", |_| {});
    assert_eq!(view_of(&bare).model_auth, None);
    assert_eq!(view_of(&bare).entry_agent, None);
}

/// The view survives a trip through YAML, and an unknown key or a missing list is refused
#[test]
fn view_round_trips_and_refuses_bad_documents() {
    // write the view as YAML and read it back
    let (entity, _, _) = triage();
    let view = view_of(&entity);
    let yaml = crate::utils::curated_yaml(&view, WorkflowEditable::FIELD_ORDER).unwrap();
    let again: WorkflowView = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(again, view);
    // a key the view doesn't have is refused, at the top level and in the limits
    let mut doc = document(&view);
    doc["colour"] = "red".into();
    assert!(view_from(doc).is_err());
    let mut doc = document(&view);
    doc["limits"]["colour"] = "red".into();
    assert!(view_from(doc).is_err());
    // a list that is missing is refused instead of read as empty
    for key in ["agents", "groups", "default_models", "limits"] {
        let mut doc = document(&view);
        doc.as_object_mut().unwrap().remove(key);
        assert!(view_from(doc).is_err(), "{key}");
    }
}

/// The checks that need no server refuse a bad view
#[test]
fn validation_refuses_bad_views() {
    // start from a good view
    let (entity, _, _) = triage();
    let good = view_of(&entity);
    assert!(WorkflowEditable::validate(&good, &entity).is_ok());
    // a blank name
    let bad = edited(&good, &["name=\"  \""], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");
    // no groups, and a repeated group
    let bad = edited(&good, &["groups=[]"], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("group"), "{err}");
    let bad = edited(&good, &["groups=[lab, lab]"], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("lab"), "{err}");
    // a repeated agent
    let bad = edited(&good, &["agents=[triage, triage]"], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("triage"), "{err}");
    // an entry agent that isn't one of the agents
    let bad = edited(&good, &["entry_agent=stranger"], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("stranger"), "{err}");
    // an entry agent whose agent was dropped from the list
    let bad = edited(&good, &["agents=[reverser]"], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("triage"), "{err}");
    // a repeated model auth level and a blank default model
    let bad = edited(&good, &["model_auth=[user, user]"], &[]);
    assert!(WorkflowEditable::validate(&bad, &entity).is_err());
    let bad = edited(&good, &["default_models=[{model_id: \" \"}]"], &[]);
    let err = WorkflowEditable::validate(&bad, &entity).unwrap_err();
    assert!(err.to_string().contains("model"), "{err}");
    // clearing the entry agent is fine
    let fine = edited(&good, &[], &["entry_agent"]);
    assert!(WorkflowEditable::validate(&fine, &entity).is_ok());
}

/// `--set`, `--unset` and `--patch` change the view as described
#[test]
fn set_unset_and_patch_change_the_view() {
    let (entity, _, _) = triage();
    let view = view_of(&entity);
    // a nested limit
    let changed = edited(&view, &["limits.max_alive_sub_agents=9"], &[]);
    assert_eq!(changed.limits.max_alive_sub_agents, Some(9));
    // a list value in flow syntax, and a default model with its matcher fields
    let changed = edited(
        &view,
        &["default_models=[{model_id: gpt-5, provider: null}]"],
        &[],
    );
    assert_eq!(changed.default_models[0].model_id, "gpt-5");
    // removing an optional field
    let described = edited(&view, &["description=triages files"], &[]);
    assert_eq!(described.description.as_deref(), Some("triages files"));
    assert_eq!(edited(&described, &[], &["description"]).description, None);
    // a merge patch nulls a field and sets another
    let mut doc = document(&described);
    merge_patch(
        &mut doc,
        &serde_json::json!({"description": null, "limits": {"max_alive_sub_agents": 2}}),
    );
    let patched = view_from(doc).unwrap();
    assert_eq!(patched.description, None);
    assert_eq!(patched.limits.max_alive_sub_agents, Some(2));
}

/// A view with no changes builds no update
#[test]
fn no_change_builds_no_update() {
    // the unchanged view
    let (entity, _, _) = triage();
    assert!(update_for(&entity, &view_of(&entity)).is_none());
    // a change to a read-only field alone changes nothing either
    let mut view = view_of(&entity);
    view.id = Some("someone else".to_owned());
    assert!(update_for(&entity, &view).is_none());
    // an explicit empty model auth is the same as none
    let bare = workflow("bare", |_| {});
    let mut view = view_of(&bare);
    view.model_auth = Some(Vec::new());
    assert!(update_for(&bare, &view).is_none());
}

/// Name, description and group changes go in the entity update
#[test]
fn entity_fields_change() {
    let (entity, _, _) = triage();
    let view = view_of(&entity);
    // a rename
    let update = update_for(&entity, &edited(&view, &["name=sorting"], &[])).unwrap();
    assert_eq!(update.name.as_deref(), Some("sorting"));
    assert!(update.metadata.is_none() && update.description.is_none());
    // a description is set
    let update = update_for(&entity, &edited(&view, &["description=hello"], &[])).unwrap();
    assert_eq!(update.description.as_deref(), Some("hello"));
    assert!(!update.clear_description);
    // and cleared
    let mut described = workflow("described", |_| {});
    described.description = Some("old".to_owned());
    let start = view_of(&described);
    let update = update_for(&described, &edited(&start, &[], &["description"])).unwrap();
    assert!(update.clear_description && update.description.is_none());
    // groups are added and removed
    let update = update_for(&entity, &edited(&view, &["groups=[mars, venus]"], &[])).unwrap();
    assert_eq!(update.add_groups, vec!["mars", "venus"]);
    assert_eq!(update.remove_groups, vec!["lab"]);
}

/// Limits, default models, model auth and automatic instancing go in the metadata update
#[test]
fn settings_change() {
    let (entity, _, _) = triage();
    let view = view_of(&entity);
    // a limit change sends the whole limits
    let update = update_for(
        &entity,
        &edited(&view, &["limits.max_alive_sub_agents=9"], &[]),
    )
    .unwrap();
    let EntityMetadataUpdate::WorkflowCatalog {
        limits,
        default_models,
        model_auth,
        auto_instance,
        add_agents,
        remove_agents,
        ..
    } = workflow_update(&update)
    else {
        panic!("expected a workflow update");
    };
    assert_eq!(limits.as_ref().unwrap().max_alive_sub_agents, Some(9));
    assert!(default_models.is_none() && model_auth.is_none() && auto_instance.is_none());
    assert!(add_agents.is_empty() && remove_agents.is_empty());
    // a limit can be unset
    let update = update_for(
        &entity,
        &edited(&view, &[], &["limits.max_alive_sub_agents"]),
    )
    .unwrap();
    let EntityMetadataUpdate::WorkflowCatalog { limits, .. } = workflow_update(&update) else {
        panic!("expected a workflow update");
    };
    assert_eq!(limits.as_ref().unwrap().max_alive_sub_agents, None);
    // default models are replaced as a list, in order
    let models = "default_models=[{model_id: gpt-5}, {model_id: claude-sonnet}]";
    let update = update_for(&entity, &edited(&view, &[models], &[])).unwrap();
    let EntityMetadataUpdate::WorkflowCatalog { default_models, .. } = workflow_update(&update)
    else {
        panic!("expected a workflow update");
    };
    let ids: Vec<&str> = default_models
        .as_ref()
        .unwrap()
        .iter()
        .map(|matcher| matcher.model_id.as_str())
        .collect();
    assert_eq!(ids, vec!["gpt-5", "claude-sonnet"]);
    // model auth replaces the levels, and none allows every level
    let update = update_for(&entity, &edited(&view, &["model_auth=[harness]"], &[])).unwrap();
    let EntityMetadataUpdate::WorkflowCatalog { model_auth, .. } = workflow_update(&update) else {
        panic!("expected a workflow update");
    };
    assert_eq!(
        model_auth
            .as_ref()
            .unwrap()
            .levels
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![ModelAuthLevel::Harness]
    );
    let update = update_for(&entity, &edited(&view, &[], &["model_auth"])).unwrap();
    let EntityMetadataUpdate::WorkflowCatalog { model_auth, .. } = workflow_update(&update) else {
        panic!("expected a workflow update");
    };
    assert!(model_auth.as_ref().unwrap().levels.is_empty());
    // automatic instancing is switched
    let update = update_for(&entity, &edited(&view, &["auto_instance=false"], &[])).unwrap();
    let EntityMetadataUpdate::WorkflowCatalog { auto_instance, .. } = workflow_update(&update)
    else {
        panic!("expected a workflow update");
    };
    assert_eq!(*auto_instance, Some(false));
}

/// Agents are diffed by name: new names are looked up, and removals use the ids the workflow has
#[test]
fn agents_are_diffed_from_names() {
    let (entity, first, second) = triage();
    let view = view_of(&entity);
    // only the new name needs a lookup
    let after = edited(&view, &["agents=[triage, scout]"], &[]);
    assert_eq!(wanted(&view, &after), vec!["scout".to_owned()]);
    assert_eq!(wanted(&view, &view), Vec::<String>::new());
    // the edit adds the resolved id and removes the id the workflow already has
    let scout = Uuid::new_v4();
    let mut resolved = Resolved::default();
    resolved.agents.insert("scout".to_owned(), scout);
    let update = build_update(&entity, &view, &after, &resolved)
        .unwrap()
        .unwrap();
    let EntityMetadataUpdate::WorkflowCatalog {
        add_agents,
        remove_agents,
        entry_agent,
        clear_entry_agent,
        ..
    } = workflow_update(&update)
    else {
        panic!("expected a workflow update");
    };
    assert_eq!(add_agents, &vec![scout]);
    assert_eq!(remove_agents, &vec![second.id]);
    assert!(entry_agent.is_none() && clear_entry_agent.is_none());
    // an agent the lookup missed is an error, not a silent skip
    let err = build_update(&entity, &view, &after, &Resolved::default()).unwrap_err();
    assert!(err.to_string().contains("scout"), "{err}");
    // the entry agent keeps its id when only the other agent changes
    assert_eq!(first.id, entity_entry(&entity));
}

/// Get the entry agent of a workflow entity
///
/// # Arguments
///
/// * `entity` - The workflow
fn entity_entry(entity: &Entity) -> Uuid {
    // the entry agent of the fixture
    match &entity.metadata {
        EntityMetadata::WorkflowCatalog(meta) => meta.entry_agent.expect("no entry agent"),
        _ => panic!("expected a workflow"),
    }
}

/// The entry agent is set to an existing or a new agent, and cleared
#[test]
fn entry_agent_changes() {
    let (entity, _, second) = triage();
    let view = view_of(&entity);
    // switching to the other agent sets its id
    let update = update_for(&entity, &edited(&view, &["entry_agent=reverser"], &[])).unwrap();
    let EntityMetadataUpdate::WorkflowCatalog {
        entry_agent,
        clear_entry_agent,
        ..
    } = workflow_update(&update)
    else {
        panic!("expected a workflow update");
    };
    assert_eq!(*entry_agent, Some(second.id));
    assert!(clear_entry_agent.is_none());
    // clearing it sets the clear flag
    let update = update_for(&entity, &edited(&view, &[], &["entry_agent"])).unwrap();
    let EntityMetadataUpdate::WorkflowCatalog {
        entry_agent,
        clear_entry_agent,
        ..
    } = workflow_update(&update)
    else {
        panic!("expected a workflow update");
    };
    assert!(entry_agent.is_none());
    assert_eq!(*clear_entry_agent, Some(true));
    // a new agent can become the entry agent in the same edit
    let after = edited(
        &view,
        &["agents=[triage, reverser, scout]", "entry_agent=scout"],
        &[],
    );
    let mut resolved = Resolved::default();
    let scout = Uuid::new_v4();
    resolved.agents.insert("scout".to_owned(), scout);
    let update = build_update(&entity, &view, &after, &resolved)
        .unwrap()
        .unwrap();
    let EntityMetadataUpdate::WorkflowCatalog { entry_agent, .. } = workflow_update(&update) else {
        panic!("expected a workflow update");
    };
    assert_eq!(*entry_agent, Some(scout));
}

/// The typed flags of `workflows update` become edits of the view
#[test]
fn typed_flags_become_edits() {
    let (entity, _, _) = triage();
    let view = view_of(&entity);
    // apply the edits a command's flags make to the view
    let applied = |flags: &[&str]| -> WorkflowView {
        let ops = typed_ops(&update_cmd(flags), &view).unwrap();
        let mut doc = document(&view);
        for op in &ops {
            op.apply(&mut doc).unwrap();
        }
        view_from(doc).unwrap()
    };
    // the entity fields
    let changed = applied(&["--rename", "sorting", "-d", "sorts files"]);
    assert_eq!(changed.name, "sorting");
    assert_eq!(changed.description.as_deref(), Some("sorts files"));
    assert_eq!(applied(&["--clear-description"]).description, None);
    // group changes are relative to the current groups
    let changed = applied(&["--add-group", "mars", "--remove-group", "lab"]);
    assert_eq!(changed.groups, vec!["mars"]);
    assert_eq!(applied(&["--add-group", "lab"]).groups, vec!["lab"]);
    // the limit and the automatic instancing switch
    let changed = applied(&["--max-alive-sub-agents", "7", "--auto-instance", "off"]);
    assert_eq!(changed.limits.max_alive_sub_agents, Some(7));
    assert!(!changed.auto_instance);
    // default models replace the list, keeping the matcher of a model that was already listed
    let mut matcher_view = view.clone();
    matcher_view.default_models[0]
        .api_versions
        .insert("2023-06-01".to_owned());
    let ops = typed_ops(
        &update_cmd(&[
            "--default-model",
            "gpt-5",
            "--default-model",
            "claude-sonnet",
        ]),
        &matcher_view,
    )
    .unwrap();
    let mut doc = document(&matcher_view);
    for op in &ops {
        op.apply(&mut doc).unwrap();
    }
    let changed = view_from(doc).unwrap();
    assert_eq!(changed.default_models[0].model_id, "gpt-5");
    assert_eq!(changed.default_models[1], matcher_view.default_models[0]);
    assert_eq!(
        applied(&["--clear-default-models"]).default_models,
        Vec::<ModelMatcher>::new()
    );
    // model auth replaces the levels, and can be cleared
    let changed = applied(&["--model-auth", "harness", "--model-auth", "user"]);
    assert_eq!(
        changed.model_auth,
        Some(vec![ModelAuthArg::Harness, ModelAuthArg::User])
    );
    assert_eq!(applied(&["--clear-model-auth"]).model_auth, None);
    // the typed flags build the same update as the equivalent --set
    let by_flag = update_for(&entity, &applied(&["--max-alive-sub-agents", "7"])).unwrap();
    let by_set = update_for(
        &entity,
        &edited(&view, &["limits.max_alive_sub_agents=7"], &[]),
    )
    .unwrap();
    let flag_limits = format!("{:?}", workflow_update(&by_flag));
    assert_eq!(flag_limits, format!("{:?}", workflow_update(&by_set)));
}

/// The first two lines of an edit buffer name the workflow and say how agents are written
#[test]
fn header_explains_the_view() {
    // the header names the workflow and its agents
    let (entity, _, _) = triage();
    let header = WorkflowEditable::header(&entity);
    assert!(header.contains("# Editing workflow 'pipeline'"), "{header}");
    assert!(header.contains("agents: [NAME]"), "{header}");
    assert!(header.lines().all(|line| line.starts_with('#')), "{header}");
    // the id is a read-only field
    let doc: BTreeMap<String, serde_json::Value> =
        serde_json::from_value(document(&view_of(&entity))).unwrap();
    assert!(doc.contains_key("*id*"));
}

/// Parse a `create` command line
///
/// # Arguments
///
/// * `flags` - The arguments after `thorctl workflows create`
fn create_cmd(flags: &[&str]) -> CreateWorkflow {
    // parse the whole command line
    let mut argv = vec!["thorctl", "workflows", "create"];
    argv.extend_from_slice(flags);
    match Args::try_parse_from(argv).unwrap().cmd {
        SubCommands::Workflows(Workflows::Create(cmd)) => cmd,
        _ => panic!("expected workflows create"),
    }
}

/// A new workflow starts from the flags alone, as `workflows create` always has
#[test]
fn create_view_from_flags() {
    // every create flag
    let cmd = create_cmd(&[
        "triage",
        "-g",
        "lab",
        "-g",
        "mars",
        "--agent",
        "first",
        "--agent",
        "second",
        "--entry",
        "first",
        "--default-model",
        "claude-sonnet",
        "--default-model",
        "gpt-5",
        "--max-alive-sub-agents",
        "4",
        "--model-auth",
        "user",
        "--no-auto-instance",
        "-d",
        "triages files",
    ]);
    let view = create_view(None, &cmd).unwrap();
    assert_eq!(view.name, "triage");
    assert_eq!(view.groups, vec!["lab", "mars"]);
    assert_eq!(view.agents, vec!["first", "second"]);
    assert_eq!(view.entry_agent.as_deref(), Some("first"));
    assert_eq!(view.default_models.len(), 2);
    assert_eq!(view.default_models[1].model_id, "gpt-5");
    assert_eq!(view.limits.max_alive_sub_agents, Some(4));
    assert_eq!(view.model_auth, Some(vec![ModelAuthArg::User]));
    assert!(!view.auto_instance);
    assert_eq!(view.description.as_deref(), Some("triages files"));
    // a workflow with only a name and a group has the defaults of a new workflow
    let view = create_view(None, &create_cmd(&["bare", "-g", "lab"])).unwrap();
    assert!(view.agents.is_empty() && view.entry_agent.is_none());
    assert!(view.auto_instance && view.model_auth.is_none());
    assert_eq!(view.limits.max_alive_sub_agents, None);
}

/// A spec is the view `workflows spec` prints, and flags replace what it sets
#[test]
fn create_view_from_spec_and_flags() {
    // a printed spec makes the workflow on its own
    let (entity, _, _) = triage();
    let spec = document(&view_of(&entity));
    let view = create_view(Some(&spec), &create_cmd(&["--spec", "-"])).unwrap();
    assert_eq!(view.name, "pipeline");
    assert_eq!(view.agents, vec!["reverser", "triage"]);
    assert_eq!(view.entry_agent.as_deref(), Some("triage"));
    assert_eq!(view.limits.max_alive_sub_agents, Some(4));
    assert_eq!(
        view.model_auth,
        Some(vec![ModelAuthArg::Group, ModelAuthArg::User])
    );
    // the read-only id of the spec is not kept as anything
    assert!(spec["*id*"].is_string());
    // a spec that sets only some keys takes the defaults for the rest
    let sparse = serde_json::json!({"name": "sparse", "groups": ["lab"]});
    let view = create_view(Some(&sparse), &create_cmd(&["--spec", "-"])).unwrap();
    assert!(view.agents.is_empty() && view.auto_instance);
    // flags replace the spec's scalars, groups and lists, and agents are added to its agents
    let cmd = create_cmd(&[
        "renamed",
        "--spec",
        "-",
        "-g",
        "venus",
        "--agent",
        "late",
        "--default-model",
        "gpt-5",
        "--model-auth",
        "harness",
        "--max-alive-sub-agents",
        "9",
        "--no-auto-instance",
    ]);
    let view = create_view(Some(&spec), &cmd).unwrap();
    assert_eq!(view.name, "renamed");
    assert_eq!(view.groups, vec!["venus"]);
    assert_eq!(view.agents, vec!["reverser", "triage", "late"]);
    assert_eq!(view.default_models.len(), 1);
    assert_eq!(view.default_models[0].model_id, "gpt-5");
    assert_eq!(view.model_auth, Some(vec![ModelAuthArg::Harness]));
    assert_eq!(view.limits.max_alive_sub_agents, Some(9));
    assert!(!view.auto_instance);
}

/// A spec the view can't read, or that names nothing to create, is refused
#[test]
fn create_view_refuses_bad_specs() {
    // an unknown key is named
    let spec = serde_json::json!({"name": "x", "groups": ["lab"], "colour": "red"});
    let err = create_view(Some(&spec), &create_cmd(&["--spec", "-"])).unwrap_err();
    assert!(err.to_string().contains("colour"), "{err}");
    // a spec with no name or no group fails the same checks as an edit
    let spec = serde_json::json!({"groups": ["lab"]});
    let err = create_view(Some(&spec), &create_cmd(&["--spec", "-"])).unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");
    let spec = serde_json::json!({"name": "x"});
    let err = create_view(Some(&spec), &create_cmd(&["--spec", "-"])).unwrap_err();
    assert!(err.to_string().contains("group"), "{err}");
    // a repeated agent is refused, and a wrong type is named
    let spec = serde_json::json!({"name": "x", "groups": ["lab"], "agents": ["a", "a"]});
    assert!(create_view(Some(&spec), &create_cmd(&["--spec", "-"])).is_err());
    let spec = serde_json::json!({"name": "x", "groups": ["lab"], "limits": "many"});
    assert!(create_view(Some(&spec), &create_cmd(&["--spec", "-"])).is_err());
}

/// The create request carries only what differs from a new workflow, and checks the entry agent
#[test]
fn create_request_checks_the_entry_agent() {
    // a workflow with every setting
    let cmd = create_cmd(&[
        "triage",
        "-g",
        "lab",
        "--max-alive-sub-agents",
        "4",
        "--default-model",
        "gpt-5",
        "--model-auth",
        "user",
        "--no-auto-instance",
    ]);
    let view = create_view(None, &cmd).unwrap();
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let request = create_request(&view, &[first, second], Some(second), true).unwrap();
    assert_eq!(request.agents, vec![first, second]);
    assert_eq!(request.entry_agent, Some(second));
    assert_eq!(
        request.limits.as_ref().unwrap().max_alive_sub_agents,
        Some(4)
    );
    assert_eq!(request.default_models.len(), 1);
    assert_eq!(request.auto_instance, Some(false));
    assert!(request.extend_target_groups);
    assert_eq!(
        request
            .model_auth
            .unwrap()
            .levels
            .into_iter()
            .collect::<Vec<_>>(),
        vec![ModelAuthLevel::User]
    );
    // a new workflow with defaults sends no settings
    let bare = create_view(None, &create_cmd(&["bare", "-g", "lab"])).unwrap();
    let request = create_request(&bare, &[], None, false).unwrap();
    assert!(request.limits.is_none() && request.model_auth.is_none());
    assert!(request.auto_instance.is_none() && request.entry_agent.is_none());
    assert!(!request.extend_target_groups);
    // an entry agent that isn't one of the agents is refused before any request
    let err = create_request(&bare, &[first], Some(second), false).unwrap_err();
    assert!(err.to_string().contains("entry"), "{err}");
    // an agent named twice, by name and by id, is refused
    let err = create_request(&bare, &[first, first], None, false).unwrap_err();
    assert!(err.to_string().contains("twice"), "{err}");
}
