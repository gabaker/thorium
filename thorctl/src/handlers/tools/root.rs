//! Editing a tool's root: its name, description, groups and tags, and a harness's env and
//! disabled models
//!
//! A tool's versions are immutable, so nothing here publishes a version; `tools edit` of a
//! version's spec does that (see the `versions` module).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::{
    Entity, EntityUpdate, HarnessEnv, TagDeleteRequest, TagRequest, ToolEntity, ToolKind,
};
use thorium::{Error, Thorium};

use super::harness_update;
use super::target::{resolve, tool_meta};
use crate::args::tools::UpdateTool;
use crate::handlers::edit::{Editable, default_header};

/// A tool as the root edit reads it
#[derive(Debug, Clone)]
pub struct RootData {
    /// The tool entity as the API returned it
    pub entity: Entity,
    /// The tool's own settings
    pub meta: ToolEntity,
}

/// The document users edit for a tool's root
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootView {
    /// The tool's id (read-only)
    #[serde(rename = "*id*", default)]
    pub id: String,
    /// The tool's kind (read-only, fixed for the life of the tool)
    #[serde(rename = "*kind*", default)]
    pub kind: String,
    /// The tool's name
    pub name: String,
    /// What the tool is for
    #[serde(default)]
    pub description: Option<String>,
    /// The groups the tool is in
    #[serde(default)]
    pub groups: Vec<String>,
    /// The tool's tags, each key with its values
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
    /// The model values a harness's runs get as env vars (harness tools only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_env: Option<HarnessEnv>,
    /// Exact model ids no run of a harness may resolve to (harness tools only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_models: Option<Vec<String>>,
    /// The tool's latest version (read-only)
    #[serde(rename = "*latest*", default)]
    pub latest: String,
}

/// What a root edit changes
#[derive(Debug, Clone)]
pub struct RootUpdate {
    /// The changes to the entity itself and to a harness's settings
    pub entity: EntityUpdate,
    /// The tag values to add, by key
    pub add_tags: BTreeMap<String, Vec<String>>,
    /// The tag key and value pairs to remove
    pub remove_tags: Vec<(String, String)>,
}

impl RootUpdate {
    /// Whether the update changes nothing
    fn is_empty(&self) -> bool {
        // nothing is renamed, regrouped, changed in metadata or retagged
        self.entity.name.is_none()
            && self.entity.description.is_none()
            && !self.entity.clear_description
            && self.entity.add_groups.is_empty()
            && self.entity.remove_groups.is_empty()
            && self.entity.metadata.is_none()
            && self.add_tags.is_empty()
            && self.remove_tags.is_empty()
    }
}

/// [`Editable`] for a tool's root
pub struct Root;

/// List the values of a set of strings that another set lacks, in order
///
/// # Arguments
///
/// * `have` - The values to look in
/// * `lack` - The values to leave out
fn missing(have: &[String], lack: &[String]) -> Vec<String> {
    // keep each value the other list doesn't hold, once
    let lack: BTreeSet<&String> = lack.iter().collect();
    let have: BTreeSet<&String> = have.iter().collect();
    have.difference(&lack)
        .map(|value| (*value).clone())
        .collect()
}

/// The (key, value) pairs of a tag map
///
/// # Arguments
///
/// * `tags` - The tags to list
fn tag_pairs(tags: &BTreeMap<String, Vec<String>>) -> BTreeSet<(String, String)> {
    // pair each key with each of its values
    tags.iter()
        .flat_map(|(key, values)| values.iter().map(|value| (key.clone(), value.clone())))
        .collect()
}

/// Diff an edited document against a tool into one update
///
/// Returns `None` when the document holds no change.
///
/// # Arguments
///
/// * `data` - The tool as fetched
/// * `view` - The edited document
pub fn diff(data: &RootData, view: &RootView) -> Result<Option<RootUpdate>, Error> {
    // diff the document against the tool as it is now
    let before = Root::to_view(data);
    let mut entity = EntityUpdate::default();
    // a new name renames the tool
    if view.name != before.name {
        entity.name = Some(view.name.clone());
    }
    // a new description replaces the old one, and none clears it
    if view.description != before.description {
        match &view.description {
            Some(text) => entity.description = Some(text.clone()),
            None => entity.clear_description = true,
        }
    }
    // groups added and removed are sent as such
    entity.add_groups = missing(&view.groups, &before.groups);
    entity.remove_groups = missing(&before.groups, &view.groups);
    // a harness's env is replaced and models are disabled or enabled to match the document
    let mut disable = BTreeSet::new();
    let mut enable = BTreeSet::new();
    let mut harness_env = None;
    let mut clear_env = None;
    if let (Some(new), Some(old)) = (&view.disabled_models, &before.disabled_models) {
        disable = missing(new, old).into_iter().collect();
        enable = missing(old, new).into_iter().collect();
    }
    if let Some(new) = view
        .harness_env
        .as_ref()
        .filter(|_| view.harness_env != before.harness_env)
    {
        if *new == HarnessEnv::default() {
            clear_env = Some(true);
        } else {
            harness_env = Some(serde_json::to_string(new)?);
        }
    }
    if harness_env.is_some() || clear_env.is_some() || !disable.is_empty() || !enable.is_empty() {
        entity.metadata = harness_update(harness_env, clear_env, disable, enable).metadata;
    }
    // tags added and removed are separate requests
    let (old_tags, new_tags) = (tag_pairs(&before.tags), tag_pairs(&view.tags));
    let mut add_tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, value) in new_tags.difference(&old_tags) {
        add_tags.entry(key.clone()).or_default().push(value.clone());
    }
    let remove_tags = old_tags.difference(&new_tags).cloned().collect();
    let update = RootUpdate {
        entity,
        add_tags,
        remove_tags,
    };
    // nothing to send means nothing changed
    Ok((!update.is_empty()).then_some(update))
}

impl Editable for Root {
    /// Tool roots are edited through the Thorium client
    type Client = Thorium;
    /// The tool's id or name; any version in it is ignored
    type Target = String;
    /// The tool entity with its parsed tool metadata
    type Data = RootData;
    /// The tool root document users edit: name, groups, tags, and harness settings
    type View = RootView;
    /// The entity update plus the tags to add and remove
    type Update = RootUpdate;
    /// Tools are called `Tool` in messages
    const KIND: &'static str = "Tool";
    /// The tool root document's keys
    const FIELD_ORDER: &'static [&'static str] = &[
        "id",
        "kind",
        "name",
        "description",
        "groups",
        "tags",
        "harness_env",
        "disabled_models",
        "latest",
    ];

    /// Read the tool
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `target` - The tool's id or name
    async fn fetch(client: &Thorium, target: &String) -> Result<RootData, Error> {
        // find the tool by id or name, ignoring any version in the target
        let found = resolve(client, target, false).await?;
        Ok(RootData {
            meta: tool_meta(&found.tool)?,
            entity: found.tool,
        })
    }

    /// Show the tool's root settings
    ///
    /// # Arguments
    ///
    /// * `data` - The tool as fetched
    fn to_view(data: &RootData) -> RootView {
        // only harness tools have a harness env and disabled models
        let harness = data.meta.kind == ToolKind::Harness;
        // list each user tag key with its sorted values, leaving out the submitter tag Thorium sets
        let mut tags = BTreeMap::new();
        for (key, values) in &data.entity.tags {
            let values: BTreeSet<String> = values
                .keys()
                .filter(|value| !(key == "submitter" && **value == data.entity.submitter))
                .cloned()
                .collect();
            if !values.is_empty() {
                tags.insert(key.clone(), values.into_iter().collect());
            }
        }
        RootView {
            id: data.entity.id.to_string(),
            kind: data.meta.kind.to_string(),
            name: data.entity.name.clone(),
            description: data.entity.description.clone(),
            groups: data.entity.groups.clone(),
            tags,
            harness_env: harness.then(|| data.meta.harness_env.clone().unwrap_or_default()),
            disabled_models: harness.then(|| data.meta.disabled_models.iter().cloned().collect()),
            latest: match (&data.meta.latest_version, data.meta.latest) {
                (Some(version), Some(id)) => format!("{version} ({id})"),
                _ => "none".to_owned(),
            },
        }
    }

    /// The tool's id
    ///
    /// # Arguments
    ///
    /// * `data` - The tool as fetched
    fn id(data: &RootData) -> String {
        data.entity.id.to_string()
    }

    /// The tool's name
    ///
    /// # Arguments
    ///
    /// * `data` - The tool as fetched
    fn name(data: &RootData) -> String {
        data.entity.name.clone()
    }

    /// Refuse an empty name, no groups, empty tags, and harness settings on another kind
    ///
    /// # Arguments
    ///
    /// * `view` - The edited tool root view
    /// * `data` - The tool as fetched
    fn validate(view: &RootView, data: &RootData) -> Result<(), Error> {
        // the tool needs a name and at least one group
        if view.name.trim().is_empty() {
            return Err(Error::new("name can't be empty"));
        }
        if view.groups.is_empty() || view.groups.iter().any(|group| group.trim().is_empty()) {
            return Err(Error::new("a tool needs at least one group"));
        }
        // a tag needs a key and values with text
        for (key, values) in &view.tags {
            if key.trim().is_empty() || values.iter().any(|value| value.trim().is_empty()) {
                return Err(Error::new("tag keys and values can't be empty"));
            }
        }
        // only harnesses have an env and disabled models
        let harness = data.meta.kind == ToolKind::Harness;
        if !harness && (view.harness_env.is_some() || view.disabled_models.is_some()) {
            return Err(Error::new(format!(
                "harness_env and disabled_models only apply to harness tools, and {} is a {} tool",
                data.entity.name, data.meta.kind
            )));
        }
        Ok(())
    }

    /// Diff the document against the tool into one update
    ///
    /// # Arguments
    ///
    /// * `_client` - The Thorium client, unused here
    /// * `data` - The tool as fetched
    /// * `view` - The edited tool root view
    // the trait's method is async for resources that look names up, and this one doesn't
    #[allow(clippy::unused_async_trait_impl)]
    async fn calculate_update(
        _client: &Thorium,
        data: &RootData,
        view: RootView,
    ) -> Result<Option<RootUpdate>, Error> {
        // the update needs nothing from Thorium
        diff(data, &view)
    }

    /// Send the update: tag removals, the entity update, then tag additions
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `_target` - The tool's id or name, unused since `data` names the tool
    /// * `data` - The tool as fetched
    /// * `update` - The tag and entity changes to send
    async fn send_update(
        client: &Thorium,
        _target: &String,
        data: &RootData,
        update: &RootUpdate,
    ) -> Result<(), Error> {
        let id = data.entity.id;
        // remove each tag from the groups it is visible in
        for (key, value) in &update.remove_tags {
            let groups: BTreeSet<String> = data
                .entity
                .tags
                .get(key)
                .and_then(|values| values.get(value))
                .map_or_else(BTreeSet::new, |groups| groups.iter().cloned().collect());
            let req = TagDeleteRequest::<Entity>::default()
                .groups(groups)
                .add(key, value);
            client.entities.untag(id, &req).await?;
        }
        // change the entity itself
        let entity = &update.entity;
        let changes_entity = entity.name.is_some()
            || entity.description.is_some()
            || entity.clear_description
            || !entity.add_groups.is_empty()
            || !entity.remove_groups.is_empty()
            || entity.metadata.is_some();
        if changes_entity {
            client.entities.update(id, entity.clone()).await?;
        }
        // add each tag to every group the tool is in afterwards
        let mut groups: BTreeSet<String> = data.entity.groups.iter().cloned().collect();
        groups.extend(entity.add_groups.iter().cloned());
        for group in &entity.remove_groups {
            groups.remove(group);
        }
        for (key, values) in &update.add_tags {
            let req = TagRequest::<Entity>::default()
                .groups(groups.iter().cloned().collect::<Vec<String>>())
                .add_values(key, values.clone());
            client.entities.tag(id, &req).await?;
        }
        Ok(())
    }

    /// The standard header plus what a root edit can't change
    ///
    /// # Arguments
    ///
    /// * `data` - The tool as fetched
    fn header(data: &RootData) -> String {
        // start from the standard header
        let mut header = default_header::<Self>(data);
        // say what lives elsewhere
        header.push_str(
            "# This is the tool's root. Versions never change: edit a version's spec with\n",
        );
        header.push_str("# `thorctl tools edit TOOL`, which publishes a new version.\n");
        header
    }
}

/// Build the flags of `tools update` into a merge patch over the tool's root document
///
/// # Arguments
///
/// * `view` - The tool's root document as it is now
/// * `cmd` - The update command
pub fn flags_patch(view: &RootView, cmd: &UpdateTool) -> serde_json::Value {
    let mut patch = serde_json::Map::new();
    // a rename and a description replace the old values
    if let Some(name) = &cmd.rename {
        patch.insert("name".into(), name.clone().into());
    }
    if let Some(description) = &cmd.description {
        patch.insert("description".into(), description.clone().into());
    }
    if cmd.clear_description {
        patch.insert("description".into(), serde_json::Value::Null);
    }
    // groups are added and removed against the tool's groups
    if !cmd.add_group.is_empty() || !cmd.remove_group.is_empty() {
        let mut groups = view.groups.clone();
        for group in &cmd.add_group {
            if !groups.contains(group) {
                groups.push(group.clone());
            }
        }
        groups.retain(|group| !cmd.remove_group.contains(group));
        patch.insert("groups".into(), serde_json::json!(groups));
    }
    // tags are added and removed against the tool's tags, and a key with no values goes away
    if !cmd.tag.is_empty() || !cmd.remove_tag.is_empty() {
        let mut tags = tag_pairs(&view.tags);
        for (key, value) in cmd.tag.iter().filter_map(|tag| tag.split_once('=')) {
            tags.insert((key.to_owned(), value.to_owned()));
        }
        for (key, value) in cmd.remove_tag.iter().filter_map(|tag| tag.split_once('=')) {
            tags.remove(&(key.to_owned(), value.to_owned()));
        }
        let mut changed = serde_json::Map::new();
        // keys that were there and now have no values are removed with null
        for key in view.tags.keys() {
            changed.insert(key.clone(), serde_json::Value::Null);
        }
        for (key, value) in &tags {
            let entry = changed
                .entry(key.clone())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if !entry.is_array() {
                *entry = serde_json::Value::Array(Vec::new());
            }
            if let Some(values) = entry.as_array_mut() {
                values.push(value.clone().into());
            }
        }
        patch.insert("tags".into(), changed.into());
    }
    serde_json::Value::Object(patch)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::collections::{HashMap, HashSet};
    use thorium::models::{
        Entity, EntityKinds, EntityMetadata, HarnessEnv, HarnessEnvVar, ToolEntity, ToolKind,
    };
    use uuid::Uuid;

    use super::{Root, RootData, RootView, diff, flags_patch};
    use crate::args::tools::{Tools, UpdateTool};
    use crate::args::{Args, SubCommands};
    use crate::handlers::edit::Editable;
    use crate::utils::patch::merge_patch;

    /// Build a tool of a kind in some groups, with one tag value visible in the first group
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of tool
    fn data(kind: ToolKind) -> RootData {
        // the tag is visible in the first group
        let mut tags = HashMap::new();
        tags.insert(
            "env".to_owned(),
            HashMap::from([("prod".to_owned(), HashSet::from(["corn".to_owned()]))]),
        );
        // Thorium tags every entity with its submitter
        tags.insert(
            "submitter".to_owned(),
            HashMap::from([("mcarson".to_owned(), HashSet::from(["corn".to_owned()]))]),
        );
        let meta = ToolEntity::new(kind, Utc::now());
        let entity = Entity {
            id: Uuid::new_v4(),
            name: "scan".to_owned(),
            kind: EntityKinds::Tool,
            metadata: EntityMetadata::Tool(meta.clone()),
            description: Some("Scans".to_owned()),
            submitter: "mcarson".to_owned(),
            groups: vec!["corn".to_owned(), "beans".to_owned()],
            tags,
            image: None,
            created: Utc::now(),
        };
        RootData { entity, meta }
    }

    /// Parse `tools update` arguments
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl tools update scan`
    fn update_cmd(args: &[&str]) -> UpdateTool {
        // parse the command line
        let mut full = vec!["thorctl", "tools", "update", "scan"];
        full.extend_from_slice(args);
        let SubCommands::Tools(Tools::Update(cmd)) =
            <Args as clap::Parser>::try_parse_from(full).unwrap().cmd
        else {
            panic!("expected tools update");
        };
        cmd
    }

    /// The root document shows names and values, with harness settings only on harnesses
    #[test]
    fn view_shows_the_root() {
        // an agentic tool has no harness settings
        let view = Root::to_view(&data(ToolKind::Agentic));
        assert_eq!(view.name, "scan");
        assert_eq!(view.kind, "Agentic");
        assert_eq!(view.groups, vec!["corn", "beans"]);
        assert_eq!(view.tags["env"], vec!["prod"]);
        // the tag Thorium sets is not a user tag
        assert!(!view.tags.contains_key("submitter"));
        assert!(view.harness_env.is_none() && view.disabled_models.is_none());
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.get("harness_env").is_none() && json.get("*id*").is_some());
        // a harness always shows both, empty by default
        let view = Root::to_view(&data(ToolKind::Harness));
        assert_eq!(view.harness_env, Some(HarnessEnv::default()));
        assert_eq!(view.disabled_models, Some(Vec::new()));
        // unknown keys are refused
        let bad = serde_json::json!({"name": "x", "groups": ["a"], "colour": "red"});
        assert!(serde_json::from_value::<RootView>(bad).is_err());
    }

    /// The update holds exactly what the document changed
    #[test]
    fn diff_builds_the_update() {
        let data = data(ToolKind::Agentic);
        // an unchanged document changes nothing
        let same = Root::to_view(&data);
        assert!(diff(&data, &same).unwrap().is_none());
        // a changed name, description, groups and tags
        let mut view = same.clone();
        view.name = "scan2".to_owned();
        view.description = None;
        view.groups = vec!["corn".to_owned(), "peas".to_owned()];
        view.tags.clear();
        view.tags.insert("env".to_owned(), vec!["dev".to_owned()]);
        let update = diff(&data, &view).unwrap().unwrap();
        assert_eq!(update.entity.name.as_deref(), Some("scan2"));
        assert!(update.entity.clear_description && update.entity.description.is_none());
        assert_eq!(update.entity.add_groups, vec!["peas"]);
        assert_eq!(update.entity.remove_groups, vec!["beans"]);
        assert_eq!(update.add_tags["env"], vec!["dev"]);
        assert_eq!(
            update.remove_tags,
            vec![("env".to_owned(), "prod".to_owned())]
        );
        assert!(update.entity.metadata.is_none());
        // read-only fields never produce a change
        let mut view = same;
        view.id = "other".to_owned();
        view.latest = "9.9.9".to_owned();
        assert!(diff(&data, &view).unwrap().is_none());
    }

    /// A harness's env and disabled models change its metadata, and an empty env clears it
    #[test]
    fn diff_changes_harness_settings() {
        let data = data(ToolKind::Harness);
        let mut view = Root::to_view(&data);
        // exporting a value replaces the env and disabling a model disables it
        let mut env = HarnessEnv::default();
        env.export.insert(HarnessEnvVar::ModelId);
        view.harness_env = Some(env);
        view.disabled_models = Some(vec!["gpt-5".to_owned()]);
        let update = diff(&data, &view).unwrap().unwrap();
        let Some(thorium::models::EntityMetadataUpdate::Tool {
            harness_env,
            clear_harness_env,
            disable_models,
            enable_models,
            version,
            spec,
            ..
        }) = update.entity.metadata
        else {
            panic!("expected a tool metadata update");
        };
        assert!(harness_env.unwrap().contains("ModelId"));
        assert!(clear_harness_env.is_none() && version.is_none() && spec.is_none());
        assert!(disable_models.contains("gpt-5") && enable_models.is_empty());
        // an env that was set and is now empty is cleared
        let mut set = data.clone();
        set.meta.harness_env = Some(HarnessEnv {
            export: [HarnessEnvVar::ModelId].into_iter().collect(),
            ..HarnessEnv::default()
        });
        set.meta.disabled_models = ["gpt-5".to_owned()].into_iter().collect();
        let mut view = Root::to_view(&set);
        view.harness_env = Some(HarnessEnv::default());
        view.disabled_models = Some(Vec::new());
        let update = diff(&set, &view).unwrap().unwrap();
        let Some(thorium::models::EntityMetadataUpdate::Tool {
            clear_harness_env,
            enable_models,
            ..
        }) = update.entity.metadata
        else {
            panic!("expected a tool metadata update");
        };
        assert_eq!(clear_harness_env, Some(true));
        assert!(enable_models.contains("gpt-5"));
    }

    /// The document is refused when it can't be applied
    #[test]
    fn validate_refuses_bad_documents() {
        let data = data(ToolKind::Agentic);
        let good = Root::to_view(&data);
        assert!(Root::validate(&good, &data).is_ok());
        // a name and a group are needed
        let mut view = good.clone();
        view.name = " ".to_owned();
        assert!(Root::validate(&view, &data).is_err());
        let mut view = good.clone();
        view.groups.clear();
        assert!(Root::validate(&view, &data).is_err());
        // a tag needs a key and values with text
        let mut view = good.clone();
        view.tags.insert(String::new(), vec!["x".to_owned()]);
        assert!(Root::validate(&view, &data).is_err());
        // harness settings only apply to a harness
        let mut view = good;
        view.disabled_models = Some(vec!["m".to_owned()]);
        let err = Root::validate(&view, &data).unwrap_err();
        assert!(err.to_string().contains("harness"), "{err}");
    }

    /// The per-setting flags become a patch that produces the document they describe
    #[test]
    fn flags_become_a_patch() {
        let data = data(ToolKind::Agentic);
        let view = Root::to_view(&data);
        // every flag at once
        let cmd = update_cmd(&[
            "--rename",
            "scan2",
            "--clear-description",
            "--add-group",
            "peas",
            "--add-group",
            "corn",
            "--remove-group",
            "beans",
            "--tag",
            "env=dev",
            "--tag",
            "team=a",
            "--remove-tag",
            "env=prod",
        ]);
        let patch = flags_patch(&view, &cmd);
        let mut document = serde_json::to_value(&view).unwrap();
        merge_patch(&mut document, &patch);
        let edited: RootView = serde_json::from_value(document).unwrap();
        assert_eq!(edited.name, "scan2");
        assert_eq!(edited.description, None);
        assert_eq!(edited.groups, vec!["corn", "peas"]);
        assert_eq!(edited.tags["env"], vec!["dev"]);
        assert_eq!(edited.tags["team"], vec!["a"]);
        // removing a key's last value removes the key
        let cmd = update_cmd(&["--remove-tag", "env=prod"]);
        let mut document = serde_json::to_value(&view).unwrap();
        merge_patch(&mut document, &flags_patch(&view, &cmd));
        let edited: RootView = serde_json::from_value(document).unwrap();
        assert!(edited.tags.is_empty());
        // a description replaces the old one
        let cmd = update_cmd(&["-d", "New"]);
        assert_eq!(
            flags_patch(&view, &cmd),
            serde_json::json!({"description": "New"})
        );
    }
}
