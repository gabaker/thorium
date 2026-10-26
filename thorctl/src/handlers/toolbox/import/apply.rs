//! Applying a toolbox import
//!
//! Groups and network policies come first, then the items in the waves of the dependency graph
//! (items of one wave are written at once, up to the worker count), then the edges between agents,
//! which are wired once every agent exists. Every change is journaled as it lands. The first
//! failure stops new work, lets the writes in flight finish, and is reported with what was and
//! wasn't applied.

use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use thorium::models::{
    AgentEntityRequest, EntityKinds, EntityMetadataRequest, EntityMetadataUpdate, EntityRequest,
    EntityUpdate, ModelAuthSource, ToolEntityRequest, ToolSetEntityRequest, ToolSetSelector,
    WorkflowCatalogEntityRequest,
};
use uuid::Uuid;

use super::classify::{RootChange, agent_view, root_change, toolset_view, workflow_view};
use super::journal::{Entry, Journal, RootUndo};
use super::plan::{ImportPlan, Options, pipeline_request};
use super::report::{Action, ItemResult};
use super::resolve::{Ids, KnownInstance, KnownTool, Prepared, Resolve, prepare_spec};
use super::rollback::{pipeline_update, workflow_update};
use super::world::{ImportApi, World};
use crate::handlers::progress::Bar;
use crate::handlers::toolbox::v2::plan_types::{
    ItemKind, ItemPlan, ItemRef, ItemState, label_with_hash,
};
use crate::handlers::toolbox::v2::types::{ToolRef, ToolboxManifestV2};

/// Everything an apply produced
#[derive(Debug, Default)]
pub struct ApplyOutput {
    /// What happened to each item, in the order they were reached
    pub results: Vec<ItemResult>,
    /// The groups that were created
    pub groups_created: Vec<String>,
    /// The network policies that were created
    pub policies_created: Vec<String>,
    /// The network policies that were changed
    pub policies_updated: Vec<String>,
    /// What stopped the apply, when something did
    pub failure: Option<String>,
}

/// What an apply is given to work with
pub struct Applier<'a, A: ImportApi> {
    /// The API to write through
    pub api: &'a A,
    /// The manifest being imported
    pub manifest: &'a ToolboxManifestV2,
    /// What existed before the import
    pub world: &'a World,
    /// The plan to apply, with its conflicts decided
    pub plan: &'a ImportPlan,
    /// What the import is asked to do
    pub opts: &'a Options,
    /// How many writes to run at once
    pub workers: usize,
    /// The progress bar
    pub bar: &'a Bar,
    /// Where each change is recorded
    pub journal: &'a Journal,
}

/// What the items of an apply share while they run at once
struct Shared {
    /// The ids of the tools that exist, including the ones this apply creates
    ids: Mutex<Ids>,
    /// The ids of the toolsets, agents, and workflows that exist, including the ones created
    entities: Mutex<BTreeMap<ItemRef, Uuid>>,
    /// Whether an item has failed, which stops items that haven't started
    failed: AtomicBool,
    /// What happened to each item
    results: Mutex<Vec<ItemResult>>,
    /// The first failure
    failure: Mutex<Option<String>>,
}

/// Get the kind of entity an item is
///
/// # Arguments
///
/// * `kind` - The kind of item
fn entity_kind(kind: ItemKind) -> EntityKinds {
    // tools, toolsets, agents, and workflows are entities; pipelines never reach here
    match kind {
        ItemKind::Tool => EntityKinds::Tool,
        ItemKind::Toolset => EntityKinds::ToolSet,
        ItemKind::Agent => EntityKinds::Agent,
        ItemKind::Workflow | ItemKind::Pipeline => EntityKinds::WorkflowCatalog,
    }
}

impl<A: ImportApi> Applier<'_, A> {
    /// Apply the plan
    pub async fn run(&self) -> ApplyOutput {
        let mut output = ApplyOutput::default();
        // what the items share
        let shared = Shared {
            ids: Mutex::new(Ids::from_world(self.manifest, self.world)),
            entities: Mutex::new(self.existing_entities()),
            failed: AtomicBool::new(false),
            results: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        };
        // groups and policies come first, since items refer to both
        if let Err(message) = self.prepare(&mut output).await {
            output.failure = Some(message);
            return output;
        }
        // the plan of each item, by item
        let planned: HashMap<&ItemRef, &ItemPlan> = self
            .plan
            .plan
            .items
            .iter()
            .map(|item| (&item.item, item))
            .collect();
        self.bar.set_length(planned.len() as u64);
        // apply the waves in order, each wave at once
        for wave in &self.plan.waves {
            let items: Vec<&ItemPlan> = wave
                .iter()
                .filter_map(|item| planned.get(item).copied())
                .collect();
            stream::iter(items)
                .for_each_concurrent(self.workers.max(1), |item| self.apply_item(&shared, item))
                .await;
            // stop at the first failure rather than starting the next wave
            if shared.failed.load(Ordering::SeqCst) {
                break;
            }
        }
        // agents delegate to each other, so the edges go in once every agent exists
        if !shared.failed.load(Ordering::SeqCst) {
            self.wire_agents(&shared).await;
        }
        // the items never reached are reported as such
        self.note_unreached(&shared, &planned);
        output.results = std::mem::take(
            &mut *shared
                .results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        output.failure = shared
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        output
    }

    /// Find the ids of the toolsets, agents, and workflows of the manifest that already exist
    fn existing_entities(&self) -> BTreeMap<ItemRef, Uuid> {
        let mut found = BTreeMap::new();
        // each item is looked up in its own groups
        for (kind, names) in [
            (
                ItemKind::Toolset,
                self.manifest.toolsets.keys().collect::<Vec<_>>(),
            ),
            (ItemKind::Agent, self.manifest.agents.keys().collect()),
            (ItemKind::Workflow, self.manifest.workflows.keys().collect()),
        ] {
            for name in names {
                let item = ItemRef::new(kind, name);
                let groups = super::plan::item_groups(self.manifest, &item);
                if let Ok(Some(entity)) = self.world.entity(entity_kind(kind), name, &groups) {
                    found.insert(item, entity.id);
                }
            }
        }
        found
    }

    /// Create the missing groups and network policies and update the ones to update
    ///
    /// # Arguments
    ///
    /// * `output` - Where what was done is recorded
    async fn prepare(&self, output: &mut ApplyOutput) -> Result<(), String> {
        // create the groups that are missing
        for group in &self.plan.plan.groups {
            if group.state == crate::handlers::toolbox::v2::plan_types::GroupState::Missing
                && self.opts.create
            {
                self.api
                    .create_group(&group.name, group.description.as_deref())
                    .await
                    .map_err(|err| format!("failed to create group {}: {err}", group.name))?;
                self.journal.record(Entry::CreatedGroup {
                    name: group.name.clone(),
                });
                output.groups_created.push(group.name.clone());
            }
        }
        // create the network policies the tools name
        for policy in &self.plan.new_policies {
            let id =
                self.api.create_policy(policy).await.map_err(|err| {
                    format!("failed to create network policy {}: {err}", policy.name)
                })?;
            self.journal.record(Entry::CreatedPolicy {
                name: policy.name.clone(),
                id,
            });
            output.policies_created.push(policy.name.clone());
        }
        // bring the policies that differ in line when asked to
        for update in &self.plan.policy_updates {
            let original =
                self.api.update_policy(update).await.map_err(|err| {
                    format!("failed to update network policy {}: {err}", update.name)
                })?;
            self.journal.record(Entry::UpdatedPolicy {
                original: Box::new(original),
            });
            output.policies_updated.push(update.name.clone());
        }
        Ok(())
    }

    /// Record the result of an item
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `item` - The planned item
    /// * `action` - What happened to it
    /// * `detail` - The details
    fn record(&self, shared: &Shared, item: &ItemPlan, action: Action, detail: String) {
        // a failure stops the items that haven't started
        if action == Action::Failed {
            shared.failed.store(true, Ordering::SeqCst);
            shared
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert_with(|| format!("{} {}: {detail}", item.item.kind, item.item.name));
        }
        shared
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ItemResult {
                item: item.item.clone(),
                group: item.group.clone(),
                action,
                detail,
            });
        self.bar.inc(1);
    }

    /// Apply one item
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `item` - The planned item
    async fn apply_item(&self, shared: &Shared, item: &ItemPlan) {
        // items that need no write are only recorded
        let passive = match item.state {
            ItemState::Unchanged => Some(Action::Unchanged),
            ItemState::Skipped => Some(Action::Skipped),
            ItemState::NotImported => Some(Action::NotImported),
            ItemState::Create | ItemState::Update => None,
        };
        if let Some(action) = passive {
            self.record(shared, item, action, String::new());
            return;
        }
        // after a failure nothing new is started
        if shared.failed.load(Ordering::SeqCst) {
            return;
        }
        // an update of an agent waits until every agent exists
        if item.item.kind == ItemKind::Agent && item.state == ItemState::Update {
            return;
        }
        // write the item
        let created = item.state == ItemState::Create;
        let outcome = match item.item.kind {
            ItemKind::Tool => self.apply_tool(shared, &item.item.name).await,
            ItemKind::Toolset => self.apply_toolset(shared, &item.item.name, created).await,
            ItemKind::Agent => self.apply_agent(shared, &item.item.name).await,
            ItemKind::Workflow => self.apply_workflow(shared, &item.item.name, created).await,
            ItemKind::Pipeline => self.apply_pipeline(shared, &item.item.name, created).await,
        };
        match outcome {
            Ok((action, detail)) => self.record(shared, item, action, detail),
            Err(message) => self.record(shared, item, Action::Failed, message),
        }
    }

    /// Record the items that were never reached because something failed
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `planned` - The planned items
    fn note_unreached(&self, shared: &Shared, planned: &HashMap<&ItemRef, &ItemPlan>) {
        let mut results = shared
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // an item with no result and something to write was never reached
        for item in &self.plan.plan.items {
            let reached = results.iter().any(|result| result.item == item.item);
            let writes = matches!(item.state, ItemState::Create | ItemState::Update);
            if !reached && writes && planned.contains_key(&item.item) {
                results.push(ItemResult {
                    item: item.item.clone(),
                    group: item.group.clone(),
                    action: Action::NotAttempted,
                    detail: String::new(),
                });
            }
        }
    }

    /// Get the ids known so far
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    fn ids(shared: &Shared) -> Ids {
        // a copy, so no lock is held across a request
        shared
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Find the id of a toolset, agent, or workflow
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `kind` - The kind of item
    /// * `name` - The item's name
    /// * `groups` - The groups of the item that refers to it
    fn entity_id(
        &self,
        shared: &Shared,
        kind: ItemKind,
        name: &str,
        groups: &[String],
    ) -> Result<Uuid, String> {
        // the manifest's own items are known by what was found or created
        if let Some(id) = shared
            .entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&ItemRef::new(kind, name))
        {
            return Ok(*id);
        }
        // anything else must already exist in the groups
        match self.world.entity(entity_kind(kind), name, groups)? {
            Some(entity) => Ok(entity.id),
            None => Err(format!(
                "{kind} {name} doesn't exist in {}",
                groups.join(", ")
            )),
        }
    }

    /// Find the ids of the tools a list of references names
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `tools` - The references
    /// * `groups` - The groups of the item that refers to them
    fn tool_ids(
        &self,
        shared: &Shared,
        tools: &[ToolRef],
        groups: &[String],
    ) -> Result<Vec<Uuid>, String> {
        let ids = Self::ids(shared);
        let mut found = Vec::new();
        // each reference resolves to a tool, or to a pinned version of one
        for tool in tools {
            match ids.resolve(
                self.manifest,
                self.world,
                &tool.tool,
                tool.version.as_deref(),
                groups,
            ) {
                Resolve::Id(id) => found.push(id),
                Resolve::Pending(reason) | Resolve::Missing(reason) => return Err(reason),
            }
        }
        Ok(found)
    }

    /// Create or update a tool and publish its versions
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `name` - The tool's name
    async fn apply_tool(&self, shared: &Shared, name: &str) -> Result<(Action, String), String> {
        let tool = &self.manifest.tools[name];
        let existing = self.world.tool(name, &tool.groups)?;
        let mut tool_id = existing.map(|live| live.id);
        // what the tool has published, which grows as versions are published
        let mut hashes: Vec<String> = existing.map_or_else(Vec::new, |live| {
            live.instances.iter().map(|i| i.spec_hash.clone()).collect()
        });
        let mut labels: Vec<String> = existing.map_or_else(Vec::new, |live| {
            live.instances.iter().map(|i| i.label.clone()).collect()
        });
        let mut notes: Vec<String> = Vec::new();
        // the root of a tool that exists is brought in line first
        let change = match existing {
            Some(live) => root_change(tool, live)?,
            None => RootChange::default(),
        };
        if let (Some(live), false) = (existing, change.is_empty()) {
            self.apply_root(live, &change).await?;
            notes.push("updated the tool".to_owned());
        }
        // publish each version in order, the manifest's latest last
        for label in tool.publish_order() {
            let version = &tool.versions[&label];
            let ids = Self::ids(shared);
            let ready = match prepare_spec(self.manifest, self.world, &ids, version, &tool.groups)?
            {
                Prepared::Ready(ready) => ready,
                Prepared::Pending(reason) => {
                    return Err(format!(
                        "version {label} needs something not created: {reason}"
                    ));
                }
            };
            // a spec Thorium already has is left alone, under whatever label it has
            if hashes.contains(&ready.spec_hash) {
                continue;
            }
            // a label taken by a different spec is published with a prefix of the spec's hash
            let publish_label = if labels.contains(&label) {
                label_with_hash(&label, &ready.spec_hash)
            } else {
                label.clone()
            };
            let instance = match tool_id {
                None => {
                    // the first version creates the tool
                    let (id, instance) =
                        self.create_tool(name, tool, &publish_label, &ready).await?;
                    tool_id = Some(id);
                    shared
                        .ids
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert_tool(
                            name,
                            KnownTool {
                                id,
                                instances: BTreeMap::new(),
                                latest: None,
                            },
                        );
                    instance
                }
                Some(id) => self.publish(name, id, &publish_label, &ready).await?,
            };
            // later versions can refer to this one, and it is the tool's latest now
            shared
                .ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_instance(
                    name,
                    &publish_label,
                    KnownInstance {
                        id: instance,
                        spec_hash: ready.spec_hash.clone(),
                    },
                );
            hashes.push(ready.spec_hash.clone());
            labels.push(publish_label.clone());
            notes.push(format!("published {publish_label}"));
        }
        let action = if existing.is_some() {
            Action::Updated
        } else {
            Action::Created
        };
        Ok((action, notes.join(", ")))
    }

    /// Create a tool with its first version
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `tool` - The tool in the manifest
    /// * `label` - The label of the first version
    /// * `ready` - The first version's spec
    async fn create_tool(
        &self,
        name: &str,
        tool: &crate::handlers::toolbox::v2::types::ToolManifest,
        label: &str,
        ready: &super::resolve::PreparedSpec,
    ) -> Result<(Uuid, Uuid), String> {
        // the first version and the harness settings, if any
        let mut meta = ToolEntityRequest::new(label, &ready.spec)
            .map_err(|err| format!("failed to encode the spec: {err}"))?
            .provides(ready.provides.clone());
        if !tool.harness_env.is_empty() {
            let env = super::classify::parse_harness_env(&tool.harness_env)?;
            meta = meta
                .harness_env(&env)
                .map_err(|err| format!("failed to encode the harness env: {err}"))?;
        }
        meta = meta.disabled_models(tool.disabled_models.iter().cloned());
        let mut request =
            EntityRequest::new(name, EntityMetadataRequest::Tool(meta), tool.groups.clone());
        request.description.clone_from(&tool.description);
        for (key, values) in &tool.tags {
            for value in values {
                request = request.tag(key, value);
            }
        }
        // create it
        let response = self
            .api
            .create_entity(request)
            .await
            .map_err(|err| err.to_string())?;
        // the first version was created with the tool
        let instance = match response.tool_instance {
            Some(instance) => instance,
            None => self
                .api
                .tool_instances(response.id)
                .await
                .map_err(|err| err.to_string())?
                .into_iter()
                .find(|instance| instance.label == label)
                .map(|instance| instance.id)
                .ok_or_else(|| format!("the new tool has no version {label}"))?,
        };
        self.journal.record(Entry::CreatedTool {
            name: name.to_owned(),
            id: response.id,
        });
        Ok((response.id, instance))
    }

    /// Publish a version on a tool that exists
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `id` - The tool's id
    /// * `label` - The version's label
    /// * `ready` - The version's spec
    async fn publish(
        &self,
        name: &str,
        id: Uuid,
        label: &str,
        ready: &super::resolve::PreparedSpec,
    ) -> Result<Uuid, String> {
        // publish through a tool update that carries the version and its spec
        let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
            version: Some(label.to_owned()),
            spec: Some(ready.json.clone()),
            provides: ready.provides.clone(),
            harness_env: None,
            clear_harness_env: None,
            disable_models: std::collections::BTreeSet::new(),
            enable_models: std::collections::BTreeSet::new(),
        });
        self.api
            .update_entity(id, update)
            .await
            .map_err(|err| format!("failed to publish {label}: {err}"))?;
        // find the id the new version was given
        let instance = self
            .api
            .tool_instances(id)
            .await
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|instance| instance.label == label)
            .map(|instance| instance.id)
            .ok_or_else(|| format!("the published version {label} was not found"))?;
        self.journal.record(Entry::PublishedVersion {
            tool: name.to_owned(),
            label: label.to_owned(),
            id: instance,
        });
        Ok(instance)
    }

    /// Bring the root of an existing tool in line with the manifest
    ///
    /// # Arguments
    ///
    /// * `live` - The tool as it is
    /// * `change` - What differs
    async fn apply_root(
        &self,
        live: &super::world::LiveTool,
        change: &RootChange,
    ) -> Result<(), String> {
        // the description, the groups, and the harness settings go in one update
        let mut update = EntityUpdate::default();
        update.description.clone_from(&change.description);
        update.add_groups.clone_from(&change.add_groups);
        if change.harness_env.is_some()
            || !change.disable_models.is_empty()
            || !change.enable_models.is_empty()
        {
            let harness_env = change
                .harness_env
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|err| format!("failed to encode the harness env: {err}"))?;
            update.metadata = Some(EntityMetadataUpdate::Tool {
                version: None,
                spec: None,
                provides: Vec::new(),
                harness_env,
                clear_harness_env: None,
                disable_models: change.disable_models.clone(),
                enable_models: change.enable_models.clone(),
            });
        }
        let writes = update.description.is_some()
            || !update.add_groups.is_empty()
            || update.metadata.is_some();
        if writes {
            self.api
                .update_entity(live.id, update)
                .await
                .map_err(|err| format!("failed to update the tool: {err}"))?;
        }
        // the tags are added to the groups the tool is in
        if !change.add_tags.is_empty() {
            let mut groups = live.groups.clone();
            groups.extend(change.add_groups.iter().cloned());
            self.api
                .tag_entity(live.id, &groups, &change.add_tags)
                .await
                .map_err(|err| format!("failed to tag the tool: {err}"))?;
        }
        self.journal.record(Entry::UpdatedToolRoot {
            name: live.name.clone(),
            id: live.id,
            undo: RootUndo::from_change(live, change),
        });
        Ok(())
    }

    /// Create or update a toolset
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `name` - The toolset's name
    /// * `created` - Whether the toolset doesn't exist yet
    async fn apply_toolset(
        &self,
        shared: &Shared,
        name: &str,
        created: bool,
    ) -> Result<(Action, String), String> {
        let toolset = &self.manifest.toolsets[name];
        let item = ItemRef::new(ItemKind::Toolset, name);
        // an existing toolset is brought in line with the view of the manifest
        if !created {
            let live = self
                .world
                .entity(EntityKinds::ToolSet, name, &toolset.groups)?
                .ok_or_else(|| format!("toolset {name} is gone"))?;
            let changed = self
                .api
                .update_toolset(live, toolset_view(name, toolset))
                .await
                .map_err(|err| err.to_string())?;
            if changed {
                self.journal.record(Entry::UpdatedEntity {
                    kind: ItemKind::Toolset,
                    original: Box::new(live.clone()),
                });
            }
            return Ok((
                if changed {
                    Action::Updated
                } else {
                    Action::Unchanged
                },
                String::new(),
            ));
        }
        // a new toolset includes the tools it names
        let mut meta = ToolSetEntityRequest::default();
        for id in self.tool_ids(shared, &toolset.tools, &toolset.groups)? {
            meta = meta.tool(id);
        }
        if let Some(selector) = &toolset.selector {
            meta = meta.selector(selector.parse::<ToolSetSelector>()?);
        }
        meta.read_only = toolset.read_only;
        meta.auto_instance = toolset.auto_instance;
        let mut request = EntityRequest::new(
            name,
            EntityMetadataRequest::ToolSet(meta),
            toolset.groups.clone(),
        );
        request.description.clone_from(&toolset.description);
        let response = self
            .api
            .create_entity(request)
            .await
            .map_err(|err| err.to_string())?;
        self.journal.record(Entry::CreatedEntity {
            kind: ItemKind::Toolset,
            name: name.to_owned(),
            id: response.id,
        });
        shared
            .entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(item, response.id);
        Ok((Action::Created, String::new()))
    }

    /// Create an agent without its edges to other agents
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `name` - The agent's name
    async fn apply_agent(&self, shared: &Shared, name: &str) -> Result<(Action, String), String> {
        let agent = &self.manifest.agents[name];
        // the agent's config, tools, toolsets, harness, and model auth
        let mut meta = AgentEntityRequest::new(agent.config.clone());
        for id in self.tool_ids(shared, &agent.tools, &agent.groups)? {
            meta = meta.tool(id);
        }
        for toolset in &agent.toolsets {
            let id = self.entity_id(shared, ItemKind::Toolset, &toolset.toolset, &agent.groups)?;
            meta = meta.tool_set(id);
        }
        if let Some(harness) = &agent.harness {
            let ids = self.tool_ids(shared, std::slice::from_ref(harness), &agent.groups)?;
            if let Some(id) = ids.first() {
                meta = meta.harness(*id);
            }
        }
        if !agent.model_auth.is_empty() {
            meta = meta.model_auth(ModelAuthSource {
                levels: agent.model_auth.iter().copied().collect(),
            });
        }
        if agent.extend_target_groups {
            meta = meta.extend_target_groups();
        }
        let mut request = EntityRequest::new(
            name,
            EntityMetadataRequest::Agent(meta),
            agent.groups.clone(),
        );
        request.description.clone_from(&agent.description);
        let response = self
            .api
            .create_entity(request)
            .await
            .map_err(|err| err.to_string())?;
        self.journal.record(Entry::CreatedEntity {
            kind: ItemKind::Agent,
            name: name.to_owned(),
            id: response.id,
        });
        shared
            .entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ItemRef::new(ItemKind::Agent, name), response.id);
        Ok((Action::Created, String::new()))
    }

    /// Wire the agents' edges to each other, and update the agents that exist
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    async fn wire_agents(&self, shared: &Shared) {
        for planned in &self.plan.plan.items {
            if planned.item.kind != ItemKind::Agent {
                continue;
            }
            let name = &planned.item.name;
            let outcome = match planned.state {
                ItemState::Create => self.wire_new_agent(shared, name).await.map(|()| None),
                ItemState::Update => self.update_agent(name).await.map(Some),
                _ => continue,
            };
            // a failure stops the wiring
            match outcome {
                Ok(Some((action, detail))) => self.record(shared, planned, action, detail),
                Ok(None) => {}
                Err(message) => {
                    self.record(
                        shared,
                        planned,
                        Action::Failed,
                        format!("failed to wire the agent: {message}"),
                    );
                    return;
                }
            }
        }
    }

    /// Give a new agent its edges to the agents it delegates to
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `name` - The agent's name
    async fn wire_new_agent(&self, shared: &Shared, name: &str) -> Result<(), String> {
        let agent = &self.manifest.agents[name];
        if agent.sub_agents.is_empty() {
            return Ok(());
        }
        // find the agents it delegates to
        let mut add_sub_agents = Vec::new();
        for sub_agent in &agent.sub_agents {
            add_sub_agents.push(self.entity_id(
                shared,
                ItemKind::Agent,
                sub_agent,
                &agent.groups,
            )?);
        }
        let id = self.entity_id(shared, ItemKind::Agent, name, &agent.groups)?;
        // add the edges
        let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Agent {
            config: None,
            model_auth: None,
            harness: None,
            clear_harness: None,
            add_tools: Vec::new(),
            remove_tools: Vec::new(),
            add_tool_sets: Vec::new(),
            remove_tool_sets: Vec::new(),
            add_sub_agents,
            remove_sub_agents: Vec::new(),
            extend_target_groups: agent.extend_target_groups,
        });
        self.api
            .update_entity(id, update)
            .await
            .map_err(|err| err.to_string())
    }

    /// Bring an agent that exists in line with the manifest
    ///
    /// # Arguments
    ///
    /// * `name` - The agent's name
    async fn update_agent(&self, name: &str) -> Result<(Action, String), String> {
        let agent = &self.manifest.agents[name];
        let live = self
            .world
            .entity(EntityKinds::Agent, name, &agent.groups)?
            .ok_or_else(|| format!("agent {name} is gone"))?;
        let changed = self
            .api
            .update_agent(live, agent_view(name, agent))
            .await
            .map_err(|err| err.to_string())?;
        if changed {
            self.journal.record(Entry::UpdatedEntity {
                kind: ItemKind::Agent,
                original: Box::new(live.clone()),
            });
        }
        Ok((
            if changed {
                Action::Updated
            } else {
                Action::Unchanged
            },
            String::new(),
        ))
    }

    /// Create or update a workflow
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `name` - The workflow's name
    /// * `created` - Whether the workflow doesn't exist yet
    async fn apply_workflow(
        &self,
        shared: &Shared,
        name: &str,
        created: bool,
    ) -> Result<(Action, String), String> {
        let workflow = &self.manifest.workflows[name];
        // the agents the workflow runs
        let mut agent_ids: BTreeMap<String, Uuid> = BTreeMap::new();
        for agent in &workflow.agents {
            agent_ids.insert(
                agent.clone(),
                self.entity_id(shared, ItemKind::Agent, agent, &workflow.groups)?,
            );
        }
        // a workflow that exists is brought in line with the manifest's view
        if !created {
            let live = self
                .world
                .entity(EntityKinds::WorkflowCatalog, name, &workflow.groups)?
                .ok_or_else(|| format!("workflow {name} is gone"))?;
            // the agents the workflow has now are known by name too
            if let thorium::models::EntityMetadata::WorkflowCatalog(meta) = &live.metadata {
                for agent in &meta.agents {
                    agent_ids.entry(agent.name.clone()).or_insert(agent.id);
                }
            }
            let Some(update) = workflow_update(live, &workflow_view(name, workflow), &agent_ids)?
            else {
                return Ok((Action::Unchanged, String::new()));
            };
            self.api
                .update_entity(live.id, update)
                .await
                .map_err(|err| err.to_string())?;
            self.journal.record(Entry::UpdatedEntity {
                kind: ItemKind::Workflow,
                original: Box::new(live.clone()),
            });
            return Ok((Action::Updated, String::new()));
        }
        // a new workflow runs its agents from its entry agent
        let mut meta = WorkflowCatalogEntityRequest::default();
        for id in agent_ids.values() {
            meta = meta.agent(*id);
        }
        if let Some(entry) = &workflow.entry_agent {
            meta = meta.entry_agent(self.entity_id(
                shared,
                ItemKind::Agent,
                entry,
                &workflow.groups,
            )?);
        }
        meta.auto_instance = workflow.auto_instance;
        meta.limits = Some(workflow.limits.clone());
        meta.default_models.clone_from(&workflow.default_models);
        if !workflow.model_auth.is_empty() {
            meta.model_auth = Some(ModelAuthSource {
                levels: workflow.model_auth.iter().copied().collect(),
            });
        }
        let mut request = EntityRequest::new(
            name,
            EntityMetadataRequest::WorkflowCatalog(meta),
            workflow.groups.clone(),
        );
        request.description.clone_from(&workflow.description);
        let response = self
            .api
            .create_entity(request)
            .await
            .map_err(|err| err.to_string())?;
        self.journal.record(Entry::CreatedEntity {
            kind: ItemKind::Workflow,
            name: name.to_owned(),
            id: response.id,
        });
        shared
            .entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ItemRef::new(ItemKind::Workflow, name), response.id);
        Ok((Action::Created, String::new()))
    }

    /// Create or update a pipeline
    ///
    /// # Arguments
    ///
    /// * `shared` - What the items share
    /// * `name` - The pipeline's name
    /// * `created` - Whether the pipeline doesn't exist yet
    async fn apply_pipeline(
        &self,
        shared: &Shared,
        name: &str,
        created: bool,
    ) -> Result<(Action, String), String> {
        let (_, version) = super::plan::choose_pipeline_version(&self.manifest.pipelines[name])
            .ok_or_else(|| "has no version".to_owned())?;
        let config = version
            .config
            .as_ref()
            .ok_or_else(|| "has no config".to_owned())?;
        // every stage's tool exists by now
        let ids = Self::ids(shared);
        let request = pipeline_request(self.manifest, self.world, &ids, config, version)?
            .ok_or_else(|| "a stage's tool was not created".to_owned())?;
        // a new pipeline is created from the request
        if created {
            self.api
                .create_pipeline(&request)
                .await
                .map_err(|err| err.to_string())?;
            self.journal.record(Entry::CreatedPipeline {
                group: request.group.clone(),
                name: request.name.clone(),
            });
            return Ok((Action::Created, String::new()));
        }
        // one that exists is brought in line with it
        let live = self
            .world
            .pipeline(&config.group, &config.name)
            .ok_or_else(|| "the pipeline is gone".to_owned())?;
        let Some(update) = pipeline_update(live, &request) else {
            return Ok((Action::Unchanged, String::new()));
        };
        self.api
            .update_pipeline(&config.group, &config.name, &update)
            .await
            .map_err(|err| err.to_string())?;
        self.journal.record(Entry::UpdatedPipeline {
            original: Box::new(live.clone()),
        });
        Ok((Action::Updated, String::new()))
    }
}
