//! Tests for the toolset edit view and the update it turns into

use std::collections::BTreeMap;
use thorium::models::{Entity, EntityMetadataUpdate, EntityUpdate, ToolSetSelector};
use uuid::Uuid;

use super::{Resolved, ToolSetEditable, ToolSetView, build_update, wanted};
use crate::handlers::agents::fixtures::{tool, tool_instance, toolset_with};
use crate::handlers::agents::refs::ToolRef;
use crate::handlers::edit::Editable;
use crate::utils::patch::{merge_patch, parse_set, parse_unset};

/// Build the view of a toolset entity
///
/// # Arguments
///
/// * `entity` - The toolset
fn view_of(entity: &Entity) -> ToolSetView {
    // the view users edit
    ToolSetEditable::to_view(entity)
}

/// The view's document, which `--set`, `--unset` and `--patch` edit
///
/// # Arguments
///
/// * `view` - The view to convert
fn document(view: &ToolSetView) -> serde_json::Value {
    // the same conversion the edit flow uses
    serde_json::to_value(view).unwrap()
}

/// Apply `--set` and `--unset` arguments to a view
///
/// # Arguments
///
/// * `view` - The view to edit
/// * `sets` - The `PATH=VALUE` arguments
/// * `unsets` - The `PATH` arguments
fn edited(view: &ToolSetView, sets: &[&str], unsets: &[&str]) -> ToolSetView {
    // apply each edit to the document
    let mut doc = document(view);
    for set in sets {
        parse_set(set).unwrap().apply(&mut doc).unwrap();
    }
    for unset in unsets {
        parse_unset(unset).unwrap().apply(&mut doc).unwrap();
    }
    serde_json::from_value(doc).unwrap()
}

/// A read-only toolset with a selector that includes one followed tool and one pinned tool
fn kit() -> (Entity, BTreeMap<&'static str, Entity>) {
    // a followed tool and a version of another tool
    let followed = tool("yara");
    let other = tool("capa");
    let pinned = tool_instance(&other, "1.0.0");
    let kit = toolset_with("kit", |meta| {
        meta.read_only = true;
        meta.selector = Some(ToolSetSelector::new("suite", "re"));
        meta.auto_instance = false;
        meta.tools.push(followed.clone());
        meta.tools.push(pinned.clone());
    });
    let mut edges = BTreeMap::new();
    edges.insert("followed", followed);
    edges.insert("pinned", pinned);
    (kit, edges)
}

/// Build the update a view change makes, with every new tool resolved to a fresh id
///
/// # Arguments
///
/// * `entity` - The toolset as fetched
/// * `after` - The edited view
fn update_for(entity: &Entity, after: &ToolSetView) -> Option<EntityUpdate> {
    // resolve each new tool to an id the way the command does
    let before = view_of(entity);
    let mut resolved = Resolved::default();
    for tool in wanted(&before, after) {
        resolved.tools.insert(tool, Uuid::new_v4());
    }
    build_update(entity, &before, after, &resolved).unwrap()
}

/// Unpack the toolset metadata update of an update
///
/// # Arguments
///
/// * `update` - The update
fn toolset_update(update: &EntityUpdate) -> &EntityMetadataUpdate {
    // a toolset edit sends toolset metadata
    update.metadata.as_ref().expect("no metadata update")
}

/// The view shows the settings, the selector as `key=value`, and the tools by name
#[test]
fn view_shows_settings_and_tools() {
    // a toolset with every setting
    let (entity, _) = kit();
    let view = view_of(&entity);
    assert_eq!(view.name, "kit");
    assert_eq!(view.id.as_deref(), Some(entity.id.to_string().as_str()));
    assert_eq!(view.groups, vec!["lab"]);
    assert!(view.read_only && !view.auto_instance);
    assert_eq!(view.selector.as_deref(), Some("suite=re"));
    assert_eq!(
        view.tools,
        vec![ToolRef::parse("capa@1.0.0"), ToolRef::parse("yara")]
    );
    // a toolset without a selector shows none
    let plain = toolset_with("plain", |_| {});
    assert_eq!(view_of(&plain).selector, None);
    assert_eq!(view_of(&plain).tools, Vec::<ToolRef>::new());
}

/// The view survives a trip through YAML, and an unknown or missing key is refused
#[test]
fn view_round_trips_and_refuses_bad_documents() {
    // write the view as YAML and read it back
    let (entity, _) = kit();
    let view = view_of(&entity);
    let yaml = crate::utils::curated_yaml(&view, ToolSetEditable::FIELD_ORDER).unwrap();
    let again: ToolSetView = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(again, view);
    // a key the view doesn't have is refused
    let mut doc = document(&view);
    doc["colour"] = "red".into();
    assert!(serde_json::from_value::<ToolSetView>(doc).is_err());
    // a missing list is refused instead of read as empty
    let mut doc = document(&view);
    doc.as_object_mut().unwrap().remove("tools");
    assert!(serde_json::from_value::<ToolSetView>(doc).is_err());
}

/// The checks that need no server refuse a bad view
#[test]
fn validation_refuses_bad_views() {
    // start from a good view
    let (entity, _) = kit();
    let good = view_of(&entity);
    assert!(ToolSetEditable::validate(&good, &entity).is_ok());
    // a blank name
    let err = ToolSetEditable::validate(&edited(&good, &["name=\" \""], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");
    // no groups, and a repeated group
    let err = ToolSetEditable::validate(&edited(&good, &["groups=[]"], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("group"), "{err}");
    let err = ToolSetEditable::validate(&edited(&good, &["groups=[lab, lab]"], &[]), &entity)
        .unwrap_err();
    assert!(err.to_string().contains("lab"), "{err}");
    // a repeated tool
    let err = ToolSetEditable::validate(
        &edited(&good, &["tools=[{tool: yara}, {tool: yara}]"], &[]),
        &entity,
    )
    .unwrap_err();
    assert!(err.to_string().contains("yara"), "{err}");
    // a selector without a value, and a blank version
    let err =
        ToolSetEditable::validate(&edited(&good, &["selector=suite"], &[]), &entity).unwrap_err();
    assert!(err.to_string().contains("key=value"), "{err}");
    let err = ToolSetEditable::validate(
        &edited(&good, &["tools=[{tool: yara, version: \" \"}]"], &[]),
        &entity,
    )
    .unwrap_err();
    assert!(err.to_string().contains("version"), "{err}");
}

/// `--set`, `--unset` and `--patch` change the view as described
#[test]
fn set_unset_and_patch_change_the_view() {
    let (entity, _) = kit();
    let view = view_of(&entity);
    // settings
    let changed = edited(&view, &["auto_instance=true", "read_only=false"], &[]);
    assert!(changed.auto_instance && !changed.read_only);
    // a selector is set and removed
    assert_eq!(
        edited(&view, &["selector=kind=disassembler"], &[])
            .selector
            .as_deref(),
        Some("kind=disassembler")
    );
    assert_eq!(edited(&view, &[], &["selector"]).selector, None);
    // a tool is appended with a pin
    let changed = edited(&view, &["tools[2]={tool: nmap, version: \"7.0\"}"], &[]);
    assert_eq!(changed.tools.len(), 3);
    assert!(changed.tools.contains(&ToolRef::parse("nmap@7.0")));
    // a merge patch nulls the description and renames
    let mut doc = document(&view);
    merge_patch(
        &mut doc,
        &serde_json::json!({"selector": null, "name": "kit2"}),
    );
    let patched: ToolSetView = serde_json::from_value(doc).unwrap();
    assert_eq!(patched.selector, None);
    assert_eq!(patched.name, "kit2");
}

/// A view with no changes builds no update
#[test]
fn no_change_builds_no_update() {
    // the unchanged view
    let (entity, _) = kit();
    assert!(update_for(&entity, &view_of(&entity)).is_none());
    // a change to the read-only id alone changes nothing
    let mut view = view_of(&entity);
    view.id = Some("someone else".to_owned());
    assert!(update_for(&entity, &view).is_none());
}

/// Name, description and group changes go in the entity update
#[test]
fn entity_fields_change() {
    let (mut entity, _) = kit();
    entity.description = Some("old".to_owned());
    let view = view_of(&entity);
    // a rename
    let update = update_for(&entity, &edited(&view, &["name=kit2"], &[])).unwrap();
    assert_eq!(update.name.as_deref(), Some("kit2"));
    assert!(update.metadata.is_none());
    // a new description, then none
    let update = update_for(&entity, &edited(&view, &["description=new"], &[])).unwrap();
    assert_eq!(update.description.as_deref(), Some("new"));
    let update = update_for(&entity, &edited(&view, &[], &["description"])).unwrap();
    assert!(update.clear_description && update.description.is_none());
    // groups are added and removed
    let update = update_for(&entity, &edited(&view, &["groups=[mars]"], &[])).unwrap();
    assert_eq!(update.add_groups, vec!["mars"]);
    assert_eq!(update.remove_groups, vec!["lab"]);
}

/// Settings and the selector go in the toolset metadata update
#[test]
fn settings_change() {
    let (entity, _) = kit();
    let view = view_of(&entity);
    // the flags change and nothing else does
    let update = update_for(
        &entity,
        &edited(&view, &["read_only=false", "auto_instance=true"], &[]),
    )
    .unwrap();
    let EntityMetadataUpdate::ToolSet {
        read_only,
        auto_instance,
        selector,
        clear_selector,
        add_tools,
        remove_tools,
        ..
    } = toolset_update(&update)
    else {
        panic!("expected a toolset update");
    };
    assert_eq!(*read_only, Some(false));
    assert_eq!(*auto_instance, Some(true));
    assert!(selector.is_none() && clear_selector.is_none());
    assert!(add_tools.is_empty() && remove_tools.is_empty());
    // a new selector replaces the old one
    let update = update_for(&entity, &edited(&view, &["selector=kind=decompiler"], &[])).unwrap();
    let EntityMetadataUpdate::ToolSet {
        selector,
        clear_selector,
        read_only,
        ..
    } = toolset_update(&update)
    else {
        panic!("expected a toolset update");
    };
    assert_eq!(*selector, Some(ToolSetSelector::new("kind", "decompiler")));
    assert!(clear_selector.is_none() && read_only.is_none());
    // removing the selector clears it
    let update = update_for(&entity, &edited(&view, &[], &["selector"])).unwrap();
    let EntityMetadataUpdate::ToolSet {
        selector,
        clear_selector,
        ..
    } = toolset_update(&update)
    else {
        panic!("expected a toolset update");
    };
    assert!(selector.is_none());
    assert_eq!(*clear_selector, Some(true));
}

/// Tools are diffed into the lists to add and remove, with pins kept apart from latest
#[test]
fn tools_are_diffed() {
    let (entity, edges) = kit();
    let view = view_of(&entity);
    // drop the pinned tool, keep the followed one, add a tool and pin another
    let changed = edited(
        &view,
        &["tools=[{tool: yara}, {tool: nmap}, {tool: capa, version: \"2.0.0\"}]"],
        &[],
    );
    assert_eq!(
        wanted(&view, &changed),
        vec![ToolRef::parse("capa@2.0.0"), ToolRef::parse("nmap")]
    );
    let update = update_for(&entity, &changed).unwrap();
    let EntityMetadataUpdate::ToolSet {
        add_tools,
        remove_tools,
        read_only,
        ..
    } = toolset_update(&update)
    else {
        panic!("expected a toolset update");
    };
    assert_eq!(add_tools.len(), 2);
    assert_eq!(remove_tools, &vec![edges["pinned"].id]);
    assert!(read_only.is_none());
    // pinning the tool that was followed removes the followed edge and adds the pin
    let repinned = edited(
        &view,
        &["tools=[{tool: yara, version: \"1.0.0\"}, {tool: capa, version: \"1.0.0\"}]"],
        &[],
    );
    let update = update_for(&entity, &repinned).unwrap();
    let EntityMetadataUpdate::ToolSet {
        add_tools,
        remove_tools,
        ..
    } = toolset_update(&update)
    else {
        panic!("expected a toolset update");
    };
    assert_eq!(add_tools.len(), 1);
    assert_eq!(remove_tools, &vec![edges["followed"].id]);
}

/// Parse `toolsets update` flags
///
/// # Arguments
///
/// * `flags` - The arguments after `thorctl toolsets update kit`
fn update_cmd(flags: &[&str]) -> crate::args::toolsets::UpdateToolSet {
    use clap::Parser;
    // parse the flags like the binary does
    let mut full = vec!["thorctl", "toolsets", "update", "kit"];
    full.extend_from_slice(flags);
    let crate::args::SubCommands::ToolSets(crate::args::toolsets::ToolSets::Update(cmd)) =
        crate::args::Args::try_parse_from(full).unwrap().cmd
    else {
        panic!("expected toolsets update");
    };
    cmd
}

/// Apply the edits the typed update flags make to a view
///
/// # Arguments
///
/// * `view` - The view to edit
/// * `flags` - The `toolsets update` flags
fn typed(view: &ToolSetView, flags: &[&str]) -> ToolSetView {
    // turn the flags into edits and apply them in order
    let ops = super::typed_ops(&update_cmd(flags), view);
    let mut doc = document(view);
    for op in ops {
        op.apply(&mut doc).unwrap();
    }
    serde_json::from_value(doc).unwrap()
}

/// The typed flags of `toolsets update` edit the view like the equivalent `--set` would
#[test]
fn typed_flags_edit_the_view() {
    let (entity, _) = kit();
    let view = view_of(&entity);
    // rename and describe, then clear the description
    let changed = typed(&view, &["--rename", "kit2", "-d", "tools for reversing"]);
    assert_eq!(changed.name, "kit2");
    assert_eq!(changed.description.as_deref(), Some("tools for reversing"));
    assert_eq!(typed(&changed, &["--clear-description"]).description, None);
    // groups are added and removed relative to the current groups
    let changed = typed(&view, &["--add-group", "mars", "--add-group", "lab"]);
    assert_eq!(changed.groups, vec!["lab", "mars"]);
    assert_eq!(
        typed(&changed, &["--remove-group", "lab"]).groups,
        vec!["mars"]
    );
    // auto instance is switched on and off
    assert!(typed(&view, &["--auto-instance", "on"]).auto_instance);
    assert!(!typed(&view, &["--auto-instance", "off"]).auto_instance);
}

/// Parse `toolsets create` flags
///
/// # Arguments
///
/// * `flags` - The arguments after `thorctl toolsets create`
fn create_cmd(flags: &[&str]) -> crate::args::toolsets::CreateToolSet {
    use clap::Parser;
    // parse the flags like the binary does
    let mut full = vec!["thorctl", "toolsets", "create"];
    full.extend_from_slice(flags);
    let crate::args::SubCommands::ToolSets(crate::args::toolsets::ToolSets::Create(cmd)) =
        crate::args::Args::try_parse_from(full).unwrap().cmd
    else {
        panic!("expected toolsets create");
    };
    cmd
}

/// A spec supplies what the flags don't, with defaults for the rest
#[test]
fn create_view_reads_a_spec() {
    // a spec with only a name, a group and a tool
    let spec = serde_json::json!({"name": "kit", "groups": ["lab"], "tools": [{"tool": "yara"}]});
    let view = super::create_view(Some(&spec), &create_cmd(&["--spec", "s.yaml"])).unwrap();
    assert_eq!(view.name, "kit");
    assert_eq!(view.groups, vec!["lab"]);
    assert_eq!(view.tools, vec![ToolRef::parse("yara")]);
    // everything else takes the defaults of a new toolset
    assert!(!view.read_only && view.auto_instance);
    assert_eq!((view.selector, view.description), (None, None));
    // a spec printed by `toolsets spec` is accepted, ignoring its id
    let (entity, _) = kit();
    let printed = serde_json::to_value(view_of(&entity)).unwrap();
    let view = super::create_view(Some(&printed), &create_cmd(&["--spec", "s.yaml"])).unwrap();
    assert_eq!(view.name, "kit");
    assert_eq!(view.selector.as_deref(), Some("suite=re"));
    assert!(view.read_only && !view.auto_instance);
    assert_eq!(view.tools.len(), 2);
}

/// Flags override a spec, and tools from flags are added to its tools
#[test]
fn create_flags_override_the_spec() {
    // a spec with settings the flags then change
    let spec = serde_json::json!({
        "name": "kit", "groups": ["lab"], "description": "from the spec",
        "selector": "suite=re", "auto_instance": true, "tools": [{"tool": "yara"}]
    });
    let cmd = create_cmd(&[
        "renamed",
        "--spec",
        "s.yaml",
        "-g",
        "mars",
        "-d",
        "from the flags",
        "--selector",
        "kind=asm",
        "--read-only",
        "--no-auto-instance",
        "--tool",
        "capa",
    ]);
    let view = super::create_view(Some(&spec), &cmd).unwrap();
    assert_eq!(view.name, "renamed");
    assert_eq!(view.groups, vec!["mars"]);
    assert_eq!(view.description.as_deref(), Some("from the flags"));
    assert_eq!(view.selector.as_deref(), Some("kind=asm"));
    assert!(view.read_only && !view.auto_instance);
    assert_eq!(
        view.tools,
        vec![ToolRef::parse("yara"), ToolRef::parse("capa")]
    );
    // flags alone make the same toolset as before specs existed
    let view =
        super::create_view(None, &create_cmd(&["kit", "-g", "lab", "--tool", "yara"])).unwrap();
    assert_eq!(view.name, "kit");
    assert_eq!(view.tools, vec![ToolRef::parse("yara")]);
    assert!(!view.read_only && view.auto_instance);
}

/// A spec with an unknown key, the wrong type or no name or group is refused
#[test]
fn create_view_refuses_bad_specs() {
    let cmd = create_cmd(&["--spec", "s.yaml"]);
    // an unknown key
    let spec = serde_json::json!({"name": "kit", "groups": ["lab"], "colour": "red"});
    let err = super::create_view(Some(&spec), &cmd).unwrap_err();
    assert!(err.to_string().contains("colour"), "{err}");
    // the wrong type
    let spec = serde_json::json!({"name": "kit", "groups": "lab"});
    assert!(super::create_view(Some(&spec), &cmd).is_err());
    // no name, and no group
    let spec = serde_json::json!({"groups": ["lab"]});
    let err = super::create_view(Some(&spec), &cmd).unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");
    let spec = serde_json::json!({"name": "kit"});
    let err = super::create_view(Some(&spec), &cmd).unwrap_err();
    assert!(err.to_string().contains("group"), "{err}");
    // a spec that is not a mapping
    assert!(super::create_view(Some(&serde_json::json!([1])), &cmd).is_err());
}

/// The create request carries the settings that differ from a new toolset's defaults
#[test]
fn create_request_carries_settings() {
    // a toolset with every setting
    let spec = serde_json::json!({
        "name": "kit", "groups": ["lab"], "read_only": true, "selector": "suite=re",
        "auto_instance": false, "tools": [{"tool": "yara"}, {"tool": "capa", "version": "1.0.0"}]
    });
    let cmd = create_cmd(&["--spec", "s.yaml", "--extend-target-groups"]);
    let view = super::create_view(Some(&spec), &cmd).unwrap();
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    let pin = Uuid::new_v4();
    let request = super::create_request(&view, &ids, &[pin], cmd.extend_target_groups).unwrap();
    assert_eq!(request.tools, vec![ids[0], ids[1], pin]);
    assert_eq!(request.read_only, Some(true));
    assert_eq!(request.auto_instance, Some(false));
    assert_eq!(request.selector, Some(ToolSetSelector::new("suite", "re")));
    assert!(request.extend_target_groups);
    // the defaults send nothing, so the API's own defaults apply
    let view = super::create_view(None, &create_cmd(&["kit", "-g", "lab"])).unwrap();
    let request = super::create_request(&view, &[], &[], false).unwrap();
    assert_eq!(request.read_only, None);
    assert_eq!(request.auto_instance, None);
    assert!(request.selector.is_none() && request.tools.is_empty());
}
