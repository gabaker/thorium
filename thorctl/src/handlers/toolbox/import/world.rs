//! What a toolbox import reads from and writes to Thorium
//!
//! [`ImportApi`] is every call the import makes. The planner only reads, through a [`World`]
//! snapshot of the target groups; the apply step writes. Putting the calls behind a trait lets the
//! tests drive the whole import against an in-memory fake.

use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thorium::models::{
    Entity, EntityKinds, EntityMetadata, EntityRequest, EntityResponse, EntityUpdate, HarnessEnv,
    NetworkPolicy, NetworkPolicyRequest, Pipeline, PipelineRequest, PipelineUpdate, ToolKind,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::handlers::agents::edit::AgentView;
use crate::handlers::toolbox::policies::PolicyUpdatePlan;
use crate::handlers::toolsets::edit::ToolSetView;

/// Who is running the import
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The caller's username
    pub username: String,
    /// Whether the caller is an admin
    pub is_admin: bool,
}

/// A group that exists in Thorium and what the caller can do in it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    /// The group's name
    pub name: String,
    /// The group's description
    pub description: Option<String>,
    /// Whether the caller can create entities in the group
    pub writable: bool,
}

/// One published version of a tool
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInfo {
    /// The id of the tool instance
    pub id: Uuid,
    /// The version label
    pub label: String,
    /// The hash of the instance's spec
    pub spec_hash: String,
    /// Whether the instance is deprecated
    pub deprecated: bool,
}

/// A tool that exists in Thorium
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTool {
    /// The tool's id
    pub id: Uuid,
    /// The tool's name
    pub name: String,
    /// The groups the tool is in
    pub groups: Vec<String>,
    /// The tool's description
    pub description: Option<String>,
    /// The user tags on the tool, keyed by tag key
    pub tags: BTreeMap<String, BTreeSet<String>>,
    /// The kind of tool
    pub kind: ToolKind,
    /// The values exported to a harness (harness tools only)
    pub harness_env: Option<HarnessEnv>,
    /// The model ids a harness may not use (harness tools only)
    pub disabled_models: BTreeSet<String>,
    /// The label of the tool's latest version
    pub latest_label: Option<String>,
    /// The published versions of the tool, newest first (empty until they are read)
    pub instances: Vec<InstanceInfo>,
}

impl LiveTool {
    /// Build a tool from the entity Thorium returned for it, without its instances
    ///
    /// Returns `None` when the entity is not a tool.
    ///
    /// # Arguments
    ///
    /// * `entity` - The entity to read
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Option<Self> {
        // only tools have tool metadata
        let EntityMetadata::Tool(meta) = &entity.metadata else {
            return None;
        };
        // flatten the tag map to the values each key carries, whatever group shows them
        let mut tags: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (key, values) in &entity.tags {
            tags.entry(key.clone())
                .or_default()
                .extend(values.keys().cloned());
        }
        let mut groups = entity.groups.clone();
        groups.sort();
        Some(LiveTool {
            id: entity.id,
            name: entity.name.clone(),
            groups,
            description: entity.description.clone(),
            tags,
            kind: meta.kind,
            harness_env: meta.harness_env.clone(),
            disabled_models: meta.disabled_models.clone(),
            latest_label: meta.latest_version.as_ref().map(ToString::to_string),
            instances: Vec::new(),
        })
    }

    /// Find a published version by its label
    ///
    /// # Arguments
    ///
    /// * `label` - The label to find
    #[must_use]
    pub fn instance(&self, label: &str) -> Option<&InstanceInfo> {
        // labels are unique within a tool
        self.instances
            .iter()
            .find(|instance| instance.label == label)
    }
}

/// Every call a toolbox import makes to Thorium
pub trait ImportApi {
    /// Get who the caller is
    async fn whoami(&self) -> Result<Caller, Error>;

    /// Get a group the caller can see, or `None` when it doesn't exist or can't be seen
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    async fn group(&self, name: &str) -> Result<Option<GroupInfo>, Error>;

    /// List the tools in some groups, without their instances
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to list the tools of
    async fn tools(&self, groups: &[String]) -> Result<Vec<LiveTool>, Error>;

    /// List the published versions of a tool, newest first
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool's id
    async fn tool_instances(&self, tool: Uuid) -> Result<Vec<InstanceInfo>, Error>;

    /// List the entities of a kind in some groups, with their edges filled in
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to list
    /// * `groups` - The groups to list in
    async fn entities(&self, kind: EntityKinds, groups: &[String]) -> Result<Vec<Entity>, Error>;

    /// Read one entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    async fn entity(&self, id: Uuid) -> Result<Entity, Error>;

    /// List the pipelines in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to list the pipelines of
    async fn pipelines(&self, group: &str) -> Result<Vec<Pipeline>, Error>;

    /// List the network policies in some groups
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to list the policies of
    async fn policies(&self, groups: &[String]) -> Result<Vec<NetworkPolicy>, Error>;

    /// Create a group owned by the caller
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    /// * `description` - The group's description
    async fn create_group(&self, name: &str, description: Option<&str>) -> Result<(), Error>;

    /// Delete a group
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    async fn delete_group(&self, name: &str) -> Result<(), Error>;

    /// Create a network policy and find the id it was given, if it can be found
    ///
    /// # Arguments
    ///
    /// * `policy` - The policy to create
    async fn create_policy(&self, policy: &NetworkPolicyRequest) -> Result<Option<Uuid>, Error>;

    /// Update an existing network policy, returning what it was before the update
    ///
    /// # Arguments
    ///
    /// * `plan` - The update to make
    async fn update_policy(&self, plan: &PolicyUpdatePlan) -> Result<NetworkPolicy, Error>;

    /// Put a network policy back the way it was
    ///
    /// # Arguments
    ///
    /// * `original` - The policy as it was before an update
    async fn restore_policy(&self, original: NetworkPolicy) -> Result<(), Error>;

    /// Delete a network policy
    ///
    /// # Arguments
    ///
    /// * `name` - The policy's name
    /// * `id` - The policy's id, when it is known
    async fn delete_policy(&self, name: &str, id: Option<Uuid>) -> Result<(), Error>;

    /// Create an entity
    ///
    /// # Arguments
    ///
    /// * `request` - The entity to create
    async fn create_entity(&self, request: EntityRequest) -> Result<EntityResponse, Error>;

    /// Update an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `update` - The update to apply
    async fn update_entity(&self, id: Uuid, update: EntityUpdate) -> Result<(), Error>;

    /// Delete an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    async fn delete_entity(&self, id: Uuid) -> Result<(), Error>;

    /// Add tags to an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `groups` - The groups the tags are visible to
    /// * `tags` - The tag values to add, keyed by tag key
    async fn tag_entity(
        &self,
        id: Uuid,
        groups: &[String],
        tags: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), Error>;

    /// Remove tags from an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `groups` - The groups to remove the tags from
    /// * `tags` - The tag values to remove, keyed by tag key
    async fn untag_entity(
        &self,
        id: Uuid,
        groups: &[String],
        tags: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), Error>;

    /// Bring an agent in line with a view, returning whether anything changed
    ///
    /// # Arguments
    ///
    /// * `live` - The agent as it is now
    /// * `view` - The view the agent should have
    async fn update_agent(&self, live: &Entity, view: AgentView) -> Result<bool, Error>;

    /// Bring a toolset in line with a view, returning whether anything changed
    ///
    /// # Arguments
    ///
    /// * `live` - The toolset as it is now
    /// * `view` - The view the toolset should have
    async fn update_toolset(&self, live: &Entity, view: ToolSetView) -> Result<bool, Error>;

    /// Create a pipeline
    ///
    /// # Arguments
    ///
    /// * `request` - The pipeline to create
    async fn create_pipeline(&self, request: &PipelineRequest) -> Result<(), Error>;

    /// Update a pipeline
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's group
    /// * `name` - The pipeline's name
    /// * `update` - The update to apply
    async fn update_pipeline(
        &self,
        group: &str,
        name: &str,
        update: &PipelineUpdate,
    ) -> Result<(), Error>;

    /// Delete a pipeline
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's group
    /// * `name` - The pipeline's name
    async fn delete_pipeline(&self, group: &str, name: &str) -> Result<(), Error>;
}

/// What exists in the groups a manifest names
#[derive(Debug, Clone, Default)]
pub struct World {
    /// Who is importing
    pub caller: Option<Caller>,
    /// The groups that exist, keyed by name (a group missing from the map doesn't exist or can't
    /// be seen)
    pub groups: BTreeMap<String, GroupInfo>,
    /// The tools in those groups (instances are read only for the tools the manifest names)
    pub tools: Vec<LiveTool>,
    /// The toolsets in those groups
    pub toolsets: Vec<Entity>,
    /// The agents in those groups
    pub agents: Vec<Entity>,
    /// The workflows in those groups
    pub workflows: Vec<Entity>,
    /// The pipelines in those groups
    pub pipelines: Vec<Pipeline>,
    /// The network policies in those groups, keyed by group and name
    pub policies: HashMap<(String, String), NetworkPolicy>,
}

/// What a world is read for
#[derive(Debug, Clone, Default)]
pub struct Wanted {
    /// The groups the manifest names
    pub groups: BTreeSet<String>,
    /// The names of the tools the manifest defines or refers to, whose versions are read
    pub tools: BTreeSet<String>,
    /// Whether any toolsets, agents, or workflows are in the manifest
    pub entities: bool,
    /// Whether any pipelines are in the manifest
    pub pipelines: bool,
    /// Whether any network policies are in the manifest
    pub policies: bool,
}

impl World {
    /// Find the tool with a name in one of some groups
    ///
    /// Returns an error naming the matches when several tools of that name are in those groups.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `groups` - The groups the tool should be in (any of them)
    pub fn tool(&self, name: &str, groups: &[String]) -> Result<Option<&LiveTool>, String> {
        // keep the tools of this name that share a group
        let matches: Vec<&LiveTool> = self
            .tools
            .iter()
            .filter(|tool| tool.name == name)
            .filter(|tool| tool.groups.iter().any(|group| groups.contains(group)))
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [tool] => Ok(Some(tool)),
            several => Err(format!(
                "several tools are named {name} in {}: {}",
                groups.join(", "),
                several
                    .iter()
                    .map(|tool| tool.id.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            )),
        }
    }

    /// Find the toolset, agent, or workflow with a name in one of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity
    /// * `name` - The entity's name
    /// * `groups` - The groups the entity should be in (any of them)
    pub fn entity(
        &self,
        kind: EntityKinds,
        name: &str,
        groups: &[String],
    ) -> Result<Option<&Entity>, String> {
        // pick the list of the right kind
        let list = match kind {
            EntityKinds::ToolSet => &self.toolsets,
            EntityKinds::Agent => &self.agents,
            _ => &self.workflows,
        };
        // keep the entities of this name that share a group
        let matches: Vec<&Entity> = list
            .iter()
            .filter(|entity| entity.name == name)
            .filter(|entity| entity.groups.iter().any(|group| groups.contains(group)))
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [entity] => Ok(Some(entity)),
            several => Err(format!(
                "several {kind} entities are named {name} in {}: {}",
                groups.join(", "),
                several
                    .iter()
                    .map(|entity| entity.id.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            )),
        }
    }

    /// Find a pipeline by group and name
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's group
    /// * `name` - The pipeline's name
    #[must_use]
    pub fn pipeline(&self, group: &str, name: &str) -> Option<&Pipeline> {
        // a pipeline name is unique within its group
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.group == group && pipeline.name == name)
    }

    /// Read what exists in some groups
    ///
    /// # Arguments
    ///
    /// * `api` - The API to read through
    /// * `wanted` - What to read
    /// * `workers` - How many reads to run at once
    pub async fn load(
        api: &impl ImportApi,
        wanted: &Wanted,
        workers: usize,
    ) -> Result<World, Error> {
        let workers = workers.max(1);
        let mut world = World {
            caller: Some(api.whoami().await?),
            ..World::default()
        };
        // find which of the named groups exist, reading them at once
        let names: Vec<&String> = wanted.groups.iter().collect();
        let found: Vec<Result<Option<GroupInfo>, Error>> = stream::iter(names.iter().copied())
            .map(|name| async move { api.group(name).await })
            .buffered(workers)
            .collect()
            .await;
        for (name, info) in names.iter().zip(found) {
            if let Some(info) = info? {
                world.groups.insert((*name).clone(), info);
            }
        }
        // everything else is only read in the groups that exist
        let existing: Vec<String> = world.groups.keys().cloned().collect();
        if existing.is_empty() {
            return Ok(world);
        }
        // read the tools, and the versions of the ones the manifest names
        let mut tools = api.tools(&existing).await?;
        let wanted_ids: Vec<usize> = tools
            .iter()
            .enumerate()
            .filter(|(_, tool)| wanted.tools.contains(&tool.name))
            .map(|(index, _)| index)
            .collect();
        let instances: Vec<Result<Vec<InstanceInfo>, Error>> = stream::iter(wanted_ids.iter())
            .map(|index| {
                let id = tools[*index].id;
                async move { api.tool_instances(id).await }
            })
            .buffered(workers)
            .collect()
            .await;
        for (index, found) in wanted_ids.into_iter().zip(instances) {
            tools[index].instances = found?;
        }
        world.tools = tools;
        // read the toolsets, agents, and workflows
        if wanted.entities {
            world.toolsets = api.entities(EntityKinds::ToolSet, &existing).await?;
            world.agents = api.entities(EntityKinds::Agent, &existing).await?;
            world.workflows = api
                .entities(EntityKinds::WorkflowCatalog, &existing)
                .await?;
        }
        // read the pipelines of each group
        if wanted.pipelines {
            for group in &existing {
                world.pipelines.extend(api.pipelines(group).await?);
            }
        }
        // read the network policies, indexed the way the policy flow looks them up
        if wanted.policies {
            for policy in api.policies(&existing).await? {
                for group in &policy.groups {
                    if existing.contains(group) {
                        world
                            .policies
                            .insert((group.clone(), policy.name.clone()), policy.clone());
                    }
                }
            }
        }
        Ok(world)
    }
}

/// Read the tools, entities, and pipelines Thorium has through the real client
impl ImportApi for Thorium {
    /// Get who the caller is
    async fn whoami(&self) -> Result<Caller, Error> {
        // ask Thorium who our token belongs to
        let user = self.users.info().await?;
        Ok(Caller {
            username: user.username,
            is_admin: matches!(user.role, thorium::models::UserRole::Admin),
        })
    }

    /// Get a group the caller can see
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    async fn group(&self, name: &str) -> Result<Option<GroupInfo>, Error> {
        // a group that can't be read is treated as missing
        let group = match self.groups.get(name).await {
            Ok(group) => group,
            Err(err) if matches!(err.status(), Some(http::StatusCode::NOT_FOUND)) => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        // the caller writes entities as an admin, or in a group that allows entities with a
        // role that can create
        let caller = self.whoami().await?;
        let role = group.role(&caller.username);
        let can_create = matches!(
            role,
            thorium::models::Roles::Owner
                | thorium::models::Roles::Manager
                | thorium::models::Roles::Analyst
                | thorium::models::Roles::User
        );
        Ok(Some(GroupInfo {
            name: group.name.clone(),
            description: group.description.clone(),
            writable: group.allowed.entities && (caller.is_admin || can_create),
        }))
    }

    /// List the tools in some groups
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to list the tools of
    async fn tools(&self, groups: &[String]) -> Result<Vec<LiveTool>, Error> {
        // read every tool entity in the groups
        let entities = self.entities(EntityKinds::Tool, groups).await?;
        Ok(entities.iter().filter_map(LiveTool::from_entity).collect())
    }

    /// List the published versions of a tool
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool's id
    async fn tool_instances(&self, tool: Uuid) -> Result<Vec<InstanceInfo>, Error> {
        // the summaries hold everything the plan compares
        let summaries = self.entities.tool_instances(tool).await?;
        Ok(summaries
            .into_iter()
            .map(|summary| InstanceInfo {
                id: summary.id,
                label: summary.version.to_string(),
                spec_hash: summary.spec_hash,
                deprecated: summary.deprecated,
            })
            .collect())
    }

    /// List the entities of a kind in some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to list
    /// * `groups` - The groups to list in
    async fn entities(&self, kind: EntityKinds, groups: &[String]) -> Result<Vec<Entity>, Error> {
        // list every entity of the kind with its details
        let opts = thorium::models::EntityListOpts::default()
            .kind(kind)
            .groups(groups.to_vec())
            .limit(usize::try_from(crate::utils::LIST_ALL_LIMIT).unwrap_or(usize::MAX));
        let mut cursor = self.entities.list_details(&opts).await?;
        let mut found: Vec<Entity> = Vec::new();
        loop {
            // keep this page
            found.append(&mut cursor.data);
            // stop once every page was read
            if cursor.exhausted() {
                break;
            }
            cursor.refill().await?;
        }
        // an entity in several of the groups is listed once per group
        found.sort_by_key(|entity| entity.id);
        found.dedup_by_key(|entity| entity.id);
        // the details of agents, toolsets, and workflows leave their edges out, so read each
        // one whole
        if matches!(
            kind,
            EntityKinds::Agent | EntityKinds::ToolSet | EntityKinds::WorkflowCatalog
        ) {
            let mut whole = Vec::with_capacity(found.len());
            for entity in &found {
                whole.push(self.entities.get(entity.id).await?);
            }
            return Ok(whole);
        }
        Ok(found)
    }

    /// Read one entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    async fn entity(&self, id: Uuid) -> Result<Entity, Error> {
        // read it with its edges
        self.entities.get(id).await
    }

    /// List the pipelines in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to list the pipelines of
    async fn pipelines(&self, group: &str) -> Result<Vec<Pipeline>, Error> {
        // read every page of the group's pipeline names, then each pipeline whole
        let mut cursor = self
            .pipelines
            .list(group)
            .limit(crate::utils::LIST_ALL_LIMIT);
        let mut names = Vec::new();
        loop {
            // fetch the next page
            cursor.next().await?;
            names.append(&mut cursor.names);
            // stop once every page was read
            if cursor.exhausted {
                break;
            }
        }
        let mut found = Vec::with_capacity(names.len());
        for name in names {
            found.push(self.pipelines.get(group, &name).await?);
        }
        Ok(found)
    }

    /// List the network policies in some groups
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to list the policies of
    async fn policies(&self, groups: &[String]) -> Result<Vec<NetworkPolicy>, Error> {
        // reuse the lookup the policy flow already has, which tolerates groups that can't be seen
        let existing =
            crate::handlers::toolbox::policies::fetch_existing_in_groups(self, groups).await?;
        Ok(existing.into_values().collect())
    }

    /// Create a group owned by the caller
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    /// * `description` - The group's description
    async fn create_group(&self, name: &str, description: Option<&str>) -> Result<(), Error> {
        // the creator becomes an owner
        let mut request = thorium::models::GroupRequest::new(name);
        request.description = description.map(str::to_owned);
        self.groups.create(&request).await?;
        Ok(())
    }

    /// Delete a group
    ///
    /// # Arguments
    ///
    /// * `name` - The group's name
    async fn delete_group(&self, name: &str) -> Result<(), Error> {
        // remove the group
        self.groups.delete(name).await?;
        Ok(())
    }

    /// Create a network policy
    ///
    /// # Arguments
    ///
    /// * `policy` - The policy to create
    async fn create_policy(&self, policy: &NetworkPolicyRequest) -> Result<Option<Uuid>, Error> {
        crate::handlers::toolbox::policies::create_one(self, policy).await
    }

    /// Update an existing network policy
    ///
    /// # Arguments
    ///
    /// * `plan` - The update to make
    async fn update_policy(&self, plan: &PolicyUpdatePlan) -> Result<NetworkPolicy, Error> {
        crate::handlers::toolbox::policies::update_one(self, plan, None, |_| {}).await
    }

    /// Put a network policy back the way it was
    ///
    /// # Arguments
    ///
    /// * `original` - The policy as it was before an update
    async fn restore_policy(&self, original: NetworkPolicy) -> Result<(), Error> {
        // the journal of the policy flow knows how to restore a snapshot
        let journal = crate::handlers::imports::rollback::Journal::new();
        journal.updated_network_policy(original);
        let bar = crate::handlers::progress::Bar::new_or_quiet(
            "restore network policy",
            "Restoring...",
            crate::handlers::progress::BarKind::Timer,
            true,
        );
        journal.rollback(self, &bar).await
    }

    /// Delete a network policy
    ///
    /// # Arguments
    ///
    /// * `name` - The policy's name
    /// * `id` - The policy's id, when it is known
    async fn delete_policy(&self, name: &str, id: Option<Uuid>) -> Result<(), Error> {
        // target the exact policy when its id is known
        self.network_policies.delete(name, id).await?;
        Ok(())
    }

    /// Create an entity
    ///
    /// # Arguments
    ///
    /// * `request` - The entity to create
    async fn create_entity(&self, request: EntityRequest) -> Result<EntityResponse, Error> {
        // create it
        self.entities.create(request).await
    }

    /// Update an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `update` - The update to apply
    async fn update_entity(&self, id: Uuid, update: EntityUpdate) -> Result<(), Error> {
        // apply it
        self.entities.update(id, update).await?;
        Ok(())
    }

    /// Delete an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    async fn delete_entity(&self, id: Uuid) -> Result<(), Error> {
        // remove it
        self.entities.delete(id).await?;
        Ok(())
    }

    /// Add tags to an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `groups` - The groups the tags are visible to
    /// * `tags` - The tag values to add, keyed by tag key
    async fn tag_entity(
        &self,
        id: Uuid,
        groups: &[String],
        tags: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), Error> {
        // build one request carrying every tag
        let mut request = thorium::models::TagRequest::default();
        for group in groups {
            request = request.group(group);
        }
        for (key, values) in tags {
            for value in values {
                request = request.add(key, value);
            }
        }
        self.entities.tag(id, &request).await?;
        Ok(())
    }

    /// Remove tags from an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `groups` - The groups to remove the tags from
    /// * `tags` - The tag values to remove, keyed by tag key
    async fn untag_entity(
        &self,
        id: Uuid,
        groups: &[String],
        tags: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), Error> {
        // build one request naming every tag
        let mut request = thorium::models::TagDeleteRequest::default();
        for group in groups {
            request = request.group(group);
        }
        for (key, values) in tags {
            for value in values {
                request = request.add(key, value);
            }
        }
        self.entities.untag(id, &request).await?;
        Ok(())
    }

    /// Bring an agent in line with a view
    ///
    /// # Arguments
    ///
    /// * `live` - The agent as it is now
    /// * `view` - The view the agent should have
    async fn update_agent(&self, live: &Entity, view: AgentView) -> Result<bool, Error> {
        use crate::handlers::agents::edit::{AgentEditable, AgentTarget};
        use crate::handlers::edit::Editable;
        // work out the update the same way `agents edit` does
        let Some(update) = AgentEditable::calculate_update(self, live, view).await? else {
            return Ok(false);
        };
        // send it
        let target = AgentTarget {
            id: live.id,
            extend_target_groups: false,
        };
        AgentEditable::send_update(self, &target, live, &update).await?;
        Ok(true)
    }

    /// Bring a toolset in line with a view
    ///
    /// # Arguments
    ///
    /// * `live` - The toolset as it is now
    /// * `view` - The view the toolset should have
    async fn update_toolset(&self, live: &Entity, view: ToolSetView) -> Result<bool, Error> {
        use crate::handlers::edit::Editable;
        use crate::handlers::toolsets::edit::{ToolSetEditable, ToolSetTarget};
        // work out the update the same way `toolsets edit` does
        let Some(update) = ToolSetEditable::calculate_update(self, live, view).await? else {
            return Ok(false);
        };
        // send it
        let target = ToolSetTarget {
            id: live.id,
            extend_target_groups: false,
        };
        ToolSetEditable::send_update(self, &target, live, &update).await?;
        Ok(true)
    }

    /// Create a pipeline
    ///
    /// # Arguments
    ///
    /// * `request` - The pipeline to create
    async fn create_pipeline(&self, request: &PipelineRequest) -> Result<(), Error> {
        // create it
        self.pipelines.create(request).await?;
        Ok(())
    }

    /// Update a pipeline
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's group
    /// * `name` - The pipeline's name
    /// * `update` - The update to apply
    async fn update_pipeline(
        &self,
        group: &str,
        name: &str,
        update: &PipelineUpdate,
    ) -> Result<(), Error> {
        // apply it
        self.pipelines.update(group, name, update).await?;
        Ok(())
    }

    /// Delete a pipeline
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's group
    /// * `name` - The pipeline's name
    async fn delete_pipeline(&self, group: &str, name: &str) -> Result<(), Error> {
        // remove it
        self.pipelines.delete(group, name).await?;
        Ok(())
    }
}
