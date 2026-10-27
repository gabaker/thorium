//! Tests for toolbox import planning, applying, and rolling back, against an in-memory Thorium

// the fake implements an async trait without awaiting anything, and asserts that lists are empty
#![allow(
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::assert_is_empty
)]

use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;
use thorium::Error;
use thorium::models::{
    Entity, EntityKinds, EntityMetadataRequest, EntityMetadataUpdate, EntityRequest,
    EntityResponse, EntityUpdate, K8sEnvironment, NetworkPolicy, NetworkPolicyRequest, Pipeline,
    PipelineRequest, PipelineUpdate, ThorDigest, ToolKind, ToolSpec, WorkloadEnvironment,
    WorkloadSpec,
};
use uuid::Uuid;

use super::Decider;
use super::world::{Caller, GroupInfo, ImportApi, InstanceInfo, LiveTool, World};
use crate::handlers::agents::edit::AgentView;
use crate::handlers::toolbox::policies::PolicyUpdatePlan;
use crate::handlers::toolbox::v2::plan_types::{Conflict, Plan, Resolution};
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;
use crate::handlers::toolsets::edit::ToolSetView;

mod apply;
mod plan;
mod prepare;
mod report;
mod updates;

/// The repository's toolbox in the legacy format, kept from before its manifests became tools
pub(super) const REAL_TOOLBOX: &str = include_str!("../../../../../../tools/toolbox.v1.json");

/// The real toolbox this repository ships, built from its tool manifests
pub(super) const REAL_TOOLBOX_V2: &str = include_str!("../../../../../../tools/toolbox.json");

/// Build the JSON of an Image tool spec that runs a container image
///
/// # Arguments
///
/// * `image` - The container image the spec runs
pub(super) fn image_spec(image: &str) -> Value {
    // build a typed spec so the JSON is whatever the API model serializes
    let spec = ToolSpec::Image(WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: image.to_owned(),
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    });
    serde_json::to_value(spec).expect("serialize spec")
}

/// Build the JSON of an Image tool spec whose results depend on other tools
///
/// # Arguments
///
/// * `image` - The container image the spec runs
/// * `results` - The values of `data.dependencies.results.tools`
pub(super) fn image_spec_depending_on(image: &str, results: &[&str]) -> Value {
    // fill in the slot of a plain spec
    let mut spec = image_spec(image);
    spec["Image"]["data"]["dependencies"]["results"]["tools"] = json!(results);
    spec
}

/// Build a tool with versions for a manifest
///
/// # Arguments
///
/// * `groups` - The groups the tool is in
/// * `versions` - The labels and specs of the versions
pub(super) fn tool_json(groups: &[&str], versions: &[(&str, Value)]) -> Value {
    // each version holds its spec
    let versions: serde_json::Map<String, Value> = versions
        .iter()
        .map(|(label, spec)| ((*label).to_owned(), json!({"spec": spec})))
        .collect();
    json!({"groups": groups, "versions": versions})
}

/// Parse a manifest from its JSON
///
/// # Arguments
///
/// * `document` - The manifest as JSON
pub(super) fn manifest(document: &Value) -> ToolboxManifestV2 {
    // the v2 types read the document directly
    serde_json::from_value(document.clone()).expect("a valid manifest")
}

/// Build a world whose groups all exist and can be written to
///
/// # Arguments
///
/// * `groups` - The names of the groups
pub(super) fn world_with(groups: &[&str]) -> World {
    // a developer who is not an admin
    let mut world = World {
        caller: Some(Caller {
            username: "dev".to_owned(),
            is_admin: false,
        }),
        ..World::default()
    };
    for group in groups {
        world.groups.insert(
            (*group).to_owned(),
            GroupInfo {
                name: (*group).to_owned(),
                description: None,
                writable: true,
            },
        );
    }
    world
}

/// Build a tool that exists in a world
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `groups` - The groups the tool is in
/// * `versions` - The labels and spec hashes of its versions, oldest first
pub(super) fn live_tool(name: &str, groups: &[&str], versions: &[(&str, &str)]) -> LiveTool {
    // newest first, as Thorium lists them
    let instances: Vec<InstanceInfo> = versions
        .iter()
        .rev()
        .map(|(label, hash)| InstanceInfo {
            id: Uuid::new_v4(),
            label: (*label).to_owned(),
            spec_hash: (*hash).to_owned(),
            deprecated: false,
        })
        .collect();
    LiveTool {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        description: None,
        tags: BTreeMap::new(),
        kind: ToolKind::Image,
        harness_env: None,
        disabled_models: BTreeSet::new(),
        latest_label: versions.last().map(|(label, _)| (*label).to_owned()),
        instances,
    }
}

/// Compute the hash Thorium gives an Image spec with no references
///
/// # Arguments
///
/// * `spec` - The spec's JSON
pub(super) fn hash_of(spec: &Value) -> String {
    // parse the spec the way the API does and hash it
    let typed = ToolSpec::parse_request(&spec.to_string()).expect("a valid spec");
    ThorDigest::tool_spec(&typed, None).expect("hash")
}

/// One write the fake received
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Write(pub String);

/// What the fake holds
#[derive(Default)]
struct FakeState {
    /// The groups that exist
    groups: BTreeMap<String, GroupInfo>,
    /// The tools that exist, with their instances newest first
    tools: Vec<LiveTool>,
    /// The pipelines that exist
    pipelines: Vec<Pipeline>,
    /// The network policies that exist
    policies: Vec<NetworkPolicy>,
    /// Every write, in order
    writes: Vec<Write>,
    /// Fail the write that would be this many writes in (counting from 1)
    fail_at: Option<usize>,
    /// How many writes were attempted
    attempts: usize,
    /// Whether the caller is an admin
    admin: bool,
    /// The toolsets, agents, and workflows that exist
    entities: Vec<Entity>,
}

/// An in-memory Thorium that holds groups, tools, and pipelines, records every write, and can
/// be told to fail one
#[derive(Default)]
pub(super) struct FakeApi {
    /// What the fake holds
    state: Mutex<FakeState>,
}

impl FakeApi {
    /// Build a fake where some groups exist
    ///
    /// # Arguments
    ///
    /// * `groups` - The names of the groups
    pub(super) fn with_groups(groups: &[&str]) -> Self {
        // every group exists and can be written to
        let fake = FakeApi::default();
        {
            let mut state = fake.state.lock().expect("lock");
            for group in groups {
                state.groups.insert(
                    (*group).to_owned(),
                    GroupInfo {
                        name: (*group).to_owned(),
                        description: None,
                        writable: true,
                    },
                );
            }
        }
        fake
    }

    /// Add a toolset, agent, or workflow that exists
    ///
    /// # Arguments
    ///
    /// * `entity` - The entity
    pub(super) fn with_entity(self, entity: Entity) -> Self {
        // listed and read like any other entity
        self.state.lock().expect("lock").entities.push(entity);
        self
    }

    /// Make the caller an admin
    pub(super) fn admin(self) -> Self {
        // admins may publish workloads that run as root
        self.state.lock().expect("lock").admin = true;
        self
    }

    /// Make the write that is this many writes in fail
    ///
    /// # Arguments
    ///
    /// * `write` - The write to fail, counting from 1
    pub(super) fn fail_write(&self, write: usize) {
        // the count includes writes that were already made
        self.state.lock().expect("lock").fail_at = Some(write);
    }

    /// Get every write made so far
    pub(super) fn writes(&self) -> Vec<String> {
        // the descriptions in the order they were made
        self.state
            .lock()
            .expect("lock")
            .writes
            .iter()
            .map(|write| write.0.clone())
            .collect()
    }

    /// Get the names of the tools that exist
    pub(super) fn tool_names(&self) -> Vec<String> {
        // sorted so tests compare lists
        let mut names: Vec<String> = self
            .state
            .lock()
            .expect("lock")
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Get the labels of a tool's versions, oldest first
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    pub(super) fn labels(&self, name: &str) -> Vec<String> {
        // stored newest first
        self.state
            .lock()
            .expect("lock")
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .map_or_else(Vec::new, |tool| {
                tool.instances
                    .iter()
                    .rev()
                    .map(|i| i.label.clone())
                    .collect()
            })
    }

    /// Get the names of the pipelines that exist
    pub(super) fn pipeline_names(&self) -> Vec<String> {
        // sorted so tests compare lists
        let mut names: Vec<String> = self
            .state
            .lock()
            .expect("lock")
            .pipelines
            .iter()
            .map(|pipeline| pipeline.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Get the names of the groups that exist
    pub(super) fn group_names(&self) -> Vec<String> {
        // the map is sorted
        self.state
            .lock()
            .expect("lock")
            .groups
            .keys()
            .cloned()
            .collect()
    }

    /// Count a write, failing it if it is the one to fail
    ///
    /// # Arguments
    ///
    /// * `description` - What the write does
    fn write(&self, description: String) -> Result<(), Error> {
        let mut state = self.state.lock().expect("lock");
        state.attempts += 1;
        // the write that is told to fail doesn't land
        if state.fail_at == Some(state.attempts) {
            return Err(Error::new(format!("injected failure at: {description}")));
        }
        state.writes.push(Write(description));
        Ok(())
    }
}

impl ImportApi for FakeApi {
    /// Get a developer
    async fn whoami(&self) -> Result<Caller, Error> {
        // a developer, who is an admin when the test says so
        Ok(Caller {
            username: "dev".to_owned(),
            is_admin: self.state.lock().expect("lock").admin,
        })
    }

    /// Get a group that exists
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    async fn group(&self, name: &str) -> Result<Option<GroupInfo>, Error> {
        // the group, if it was created
        Ok(self.state.lock().expect("lock").groups.get(name).cloned())
    }

    /// List the tools in some groups, without versions
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to list
    async fn tools(&self, groups: &[String]) -> Result<Vec<LiveTool>, Error> {
        // tools in any of the groups, without their instances
        Ok(self
            .state
            .lock()
            .expect("lock")
            .tools
            .iter()
            .filter(|tool| tool.groups.iter().any(|group| groups.contains(group)))
            .map(|tool| LiveTool {
                instances: Vec::new(),
                ..tool.clone()
            })
            .collect())
    }

    /// List the versions of a tool
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool's id
    async fn tool_instances(&self, tool: Uuid) -> Result<Vec<InstanceInfo>, Error> {
        // newest first
        Ok(self
            .state
            .lock()
            .expect("lock")
            .tools
            .iter()
            .find(|candidate| candidate.id == tool)
            .map_or_else(Vec::new, |candidate| candidate.instances.clone()))
    }

    /// List the toolsets, agents, or workflows in some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity
    /// * `groups` - The groups to list
    async fn entities(&self, kind: EntityKinds, groups: &[String]) -> Result<Vec<Entity>, Error> {
        // the entities of the kind in any of the groups
        Ok(self
            .state
            .lock()
            .expect("lock")
            .entities
            .iter()
            .filter(|entity| entity.kind == kind)
            .filter(|entity| entity.groups.iter().any(|group| groups.contains(group)))
            .cloned()
            .collect())
    }

    /// Read an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    async fn entity(&self, id: Uuid) -> Result<Entity, Error> {
        // the entity, if the test added it
        self.state
            .lock()
            .expect("lock")
            .entities
            .iter()
            .find(|entity| entity.id == id)
            .cloned()
            .ok_or_else(|| Error::new(format!("no entity {id}")))
    }

    /// List the pipelines in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group
    async fn pipelines(&self, group: &str) -> Result<Vec<Pipeline>, Error> {
        // the pipelines of the group
        Ok(self
            .state
            .lock()
            .expect("lock")
            .pipelines
            .iter()
            .filter(|pipeline| pipeline.group == group)
            .cloned()
            .collect())
    }

    /// List the network policies in some groups
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups
    async fn policies(&self, groups: &[String]) -> Result<Vec<NetworkPolicy>, Error> {
        // policies in any of the groups
        Ok(self
            .state
            .lock()
            .expect("lock")
            .policies
            .iter()
            .filter(|policy| policy.groups.iter().any(|group| groups.contains(group)))
            .cloned()
            .collect())
    }

    /// Create a group
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    /// * `description` - The description
    async fn create_group(&self, name: &str, description: Option<&str>) -> Result<(), Error> {
        self.write(format!("create group {name}"))?;
        self.state.lock().expect("lock").groups.insert(
            name.to_owned(),
            GroupInfo {
                name: name.to_owned(),
                description: description.map(str::to_owned),
                writable: true,
            },
        );
        Ok(())
    }

    /// Delete a group
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    async fn delete_group(&self, name: &str) -> Result<(), Error> {
        self.write(format!("delete group {name}"))?;
        self.state.lock().expect("lock").groups.remove(name);
        Ok(())
    }

    /// Create a network policy (it is recorded, not stored)
    ///
    /// # Arguments
    ///
    /// * `policy` - The policy
    async fn create_policy(&self, policy: &NetworkPolicyRequest) -> Result<Option<Uuid>, Error> {
        self.write(format!("create policy {}", policy.name))?;
        // keep the policy so a later plan finds it; the tests' policies hold no rules
        let id = Uuid::new_v4();
        let rules = |raw: &Option<Vec<thorium::models::NetworkPolicyRuleRaw>>| {
            raw.as_ref().map(|rules| {
                rules
                    .iter()
                    .cloned()
                    .map(|rule| {
                        thorium::models::NetworkPolicyRule::try_from(rule).expect("a valid rule")
                    })
                    .collect::<Vec<_>>()
            })
        };
        self.state
            .lock()
            .expect("lock")
            .policies
            .push(NetworkPolicy {
                name: policy.name.clone(),
                id,
                k8s_name: format!("{}-{id}", policy.name),
                groups: policy.groups.clone(),
                created: now(),
                ingress: rules(&policy.ingress),
                egress: rules(&policy.egress),
                forced_policy: policy.forced_policy,
                default_policy: policy.default_policy,
                used_by: HashMap::new(),
            });
        Ok(Some(id))
    }

    /// Update a network policy
    ///
    /// # Arguments
    ///
    /// * `plan` - The update
    async fn update_policy(&self, plan: &PolicyUpdatePlan) -> Result<NetworkPolicy, Error> {
        self.write(format!("update policy {}", plan.name))?;
        Err(Error::new("the fake has no policies to update".to_owned()))
    }

    /// Restore a network policy
    ///
    /// # Arguments
    ///
    /// * `original` - The policy
    async fn restore_policy(&self, original: NetworkPolicy) -> Result<(), Error> {
        self.write(format!("restore policy {}", original.name))
    }

    /// Delete a network policy
    ///
    /// # Arguments
    ///
    /// * `name` - The name
    /// * `_id` - The id
    async fn delete_policy(&self, name: &str, _id: Option<Uuid>) -> Result<(), Error> {
        self.write(format!("delete policy {name}"))
    }

    /// Create an entity; tools are stored with their first version
    ///
    /// # Arguments
    ///
    /// * `request` - The entity to create
    async fn create_entity(&self, request: EntityRequest) -> Result<EntityResponse, Error> {
        let id = Uuid::new_v4();
        match &request.metadata {
            EntityMetadataRequest::Tool(meta) => {
                self.write(format!("create tool {} {}", request.name, meta.version))?;
                // hash the spec the way Thorium does
                let spec = ToolSpec::parse_request(&meta.spec).map_err(Error::new)?;
                let hash = ThorDigest::tool_spec(&spec, None)
                    .map_err(|err| Error::new(err.to_string()))?;
                let instance = InstanceInfo {
                    id: Uuid::new_v4(),
                    label: meta.version.clone(),
                    spec_hash: hash,
                    deprecated: false,
                };
                let mut groups = request.groups.clone();
                groups.sort();
                self.state.lock().expect("lock").tools.push(LiveTool {
                    id,
                    name: request.name.clone(),
                    groups,
                    description: request.description.clone(),
                    tags: BTreeMap::new(),
                    kind: spec.kind(),
                    harness_env: None,
                    disabled_models: BTreeSet::new(),
                    latest_label: Some(meta.version.clone()),
                    instances: vec![instance.clone()],
                });
                Ok(EntityResponse {
                    id,
                    name: request.name,
                    tool_instance: Some(instance.id),
                    portability: None,
                })
            }
            other => {
                let kind = match other {
                    EntityMetadataRequest::ToolSet(_) => "toolset",
                    EntityMetadataRequest::Agent(_) => "agent",
                    _ => "workflow",
                };
                self.write(format!("create {kind} {}", request.name))?;
                Ok(EntityResponse {
                    id,
                    name: request.name,
                    tool_instance: None,
                    portability: None,
                })
            }
        }
    }

    /// Update an entity; a tool update with a version publishes it
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `update` - The update
    async fn update_entity(&self, id: Uuid, update: EntityUpdate) -> Result<(), Error> {
        if let Some(EntityMetadataUpdate::Tool {
            version: Some(version),
            spec: Some(spec),
            ..
        }) = &update.metadata
        {
            self.write(format!("publish {version}"))?;
            let typed = ToolSpec::parse_request(spec).map_err(Error::new)?;
            let hash =
                ThorDigest::tool_spec(&typed, None).map_err(|err| Error::new(err.to_string()))?;
            let mut state = self.state.lock().expect("lock");
            let tool = state
                .tools
                .iter_mut()
                .find(|tool| tool.id == id)
                .ok_or_else(|| Error::new("no such tool".to_owned()))?;
            // the label must be new, and the spec must differ from the latest
            if tool
                .instances
                .iter()
                .any(|instance| instance.label == *version)
            {
                return Err(Error::new(format!("version {version} already exists")));
            }
            if tool
                .instances
                .first()
                .is_some_and(|latest| latest.spec_hash == hash)
            {
                return Err(Error::new(
                    "the spec is identical to the latest version".to_owned(),
                ));
            }
            tool.instances.insert(
                0,
                InstanceInfo {
                    id: Uuid::new_v4(),
                    label: version.clone(),
                    spec_hash: hash,
                    deprecated: false,
                },
            );
            tool.latest_label = Some(version.clone());
            return Ok(());
        }
        self.write(format!("update entity {id}"))
    }

    /// Delete an entity; a tool or tool instance is removed from the fake
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    async fn delete_entity(&self, id: Uuid) -> Result<(), Error> {
        self.write(format!("delete entity {id}"))?;
        let mut state = self.state.lock().expect("lock");
        // a tool goes, or one of its versions
        state.tools.retain(|tool| tool.id != id);
        for tool in &mut state.tools {
            tool.instances.retain(|instance| instance.id != id);
            tool.latest_label = tool.instances.first().map(|i| i.label.clone());
        }
        Ok(())
    }

    /// Add tags to an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `_groups` - The groups
    /// * `_tags` - The tags
    async fn tag_entity(
        &self,
        id: Uuid,
        _groups: &[String],
        _tags: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), Error> {
        self.write(format!("tag {id}"))
    }

    /// Remove tags from an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `_groups` - The groups
    /// * `_tags` - The tags
    async fn untag_entity(
        &self,
        id: Uuid,
        _groups: &[String],
        _tags: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), Error> {
        self.write(format!("untag {id}"))
    }

    /// Update an agent
    ///
    /// # Arguments
    ///
    /// * `live` - The agent
    /// * `view` - The view it should have
    async fn update_agent(&self, live: &Entity, view: AgentView) -> Result<bool, Error> {
        self.write(format!("update agent {}", view.name))?;
        let _ = live;
        Ok(true)
    }

    /// Update a toolset
    ///
    /// # Arguments
    ///
    /// * `live` - The toolset
    /// * `view` - The view it should have
    async fn update_toolset(&self, live: &Entity, view: ToolSetView) -> Result<bool, Error> {
        self.write(format!("update toolset {}", view.name))?;
        let _ = live;
        Ok(true)
    }

    /// Create a pipeline
    ///
    /// # Arguments
    ///
    /// * `request` - The pipeline
    async fn create_pipeline(&self, request: &PipelineRequest) -> Result<(), Error> {
        self.write(format!("create pipeline {}", request.name))?;
        let order: Vec<Vec<String>> = request
            .deserialize_image_order()
            .map_err(|err| Error::new(err.to_string()))?
            .into_iter()
            .map(|stage| stage.into_iter().map(str::to_owned).collect())
            .collect();
        self.state.lock().expect("lock").pipelines.push(Pipeline {
            group: request.group.clone(),
            name: request.name.clone(),
            creator: "dev".to_owned(),
            order,
            sla: request.sla.unwrap_or(thorium::models::DEFAULT_PIPELINE_SLA),
            triggers: request.triggers.clone(),
            description: request.description.clone(),
            bans: HashMap::new(),
            stage_tools: request.stage_tools.clone(),
        });
        Ok(())
    }

    /// Update a pipeline
    ///
    /// # Arguments
    ///
    /// * `group` - The group
    /// * `name` - The name
    /// * `update` - The update
    async fn update_pipeline(
        &self,
        group: &str,
        name: &str,
        update: &PipelineUpdate,
    ) -> Result<(), Error> {
        self.write(format!("update pipeline {name}"))?;
        let mut state = self.state.lock().expect("lock");
        if let Some(pipeline) = state
            .pipelines
            .iter_mut()
            .find(|pipeline| pipeline.group == group && pipeline.name == name)
        {
            // only the stage tools matter to the tests
            pipeline.stage_tools.extend(update.add_stage_tools.clone());
        }
        Ok(())
    }

    /// Delete a pipeline
    ///
    /// # Arguments
    ///
    /// * `group` - The group
    /// * `name` - The name
    async fn delete_pipeline(&self, group: &str, name: &str) -> Result<(), Error> {
        self.write(format!("delete pipeline {name}"))?;
        self.state
            .lock()
            .expect("lock")
            .pipelines
            .retain(|pipeline| !(pipeline.group == group && pipeline.name == name));
        Ok(())
    }
}

/// A decider for sessions that can't be asked: it never prompts and declines to roll back
#[derive(Debug, Default)]
pub struct NoPrompts;

impl Decider for NoPrompts {
    /// Skip, since nobody can be asked
    ///
    /// # Arguments
    ///
    /// * `_conflict` - How the item differs
    /// * `_diff` - The difference
    fn resolve(&mut self, _conflict: &Conflict, _diff: Option<&str>) -> Resolution {
        // the engine refuses before it gets here when it can't ask
        Resolution::Skip
    }

    /// Go ahead
    ///
    /// # Arguments
    ///
    /// * `_plan` - The plan
    fn confirm(&mut self, _plan: &Plan) -> bool {
        // a session that can't be asked doesn't need to be
        true
    }

    /// Keep what was applied
    ///
    /// # Arguments
    ///
    /// * `_applied` - What was applied
    fn rollback(&mut self, _applied: &[String]) -> bool {
        // without a person only the flag rolls back
        false
    }
}

/// A bar that draws nothing
pub(super) fn quiet() -> crate::handlers::progress::Bar {
    // tests stay silent
    crate::handlers::progress::Bar::new_or_quiet(
        "test",
        "test",
        crate::handlers::progress::BarKind::UnboundIO,
        true,
    )
}

/// The settings of an import that overwrites, can't prompt, and creates everything
pub(super) fn settings() -> super::Settings {
    // the non-interactive defaults most tests start from
    super::Settings {
        create: true,
        create_groups: false,
        policy: crate::handlers::toolbox::v2::plan_types::ConflictPolicy::Overwrite,
        update_network_policy: false,
        dry_run: false,
        rollback_on_failure: false,
        can_prompt: false,
        confirm: false,
        workers: 4,
    }
}

/// Time for entities the tests build
pub(super) fn now() -> chrono::DateTime<Utc> {
    // the moment the test runs
    Utc::now()
}
